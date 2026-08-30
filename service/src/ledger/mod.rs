//! The reserve ledger: reservation/capacity accounting and the
//! bridge-request state machine (docs/04-state-machines.md,
//! docs/05-reserve-accounting.md). Owns every mutation of
//! `bridge_requests`/`reserve_ledger` — chain-observation modules
//! (`goldcoin::indexer`, `solana::indexer`) call into this module rather
//! than touching SQL directly, so the accounting invariant is enforced in
//! exactly one place.
//!
//! # Concurrency and crash safety
//!
//! SQLite serializes writers DB-wide; every mutating operation here runs
//! inside a single `BEGIN IMMEDIATE` transaction that either fully commits
//! or fully rolls back, which is what makes "reservation and settlement
//! bookkeeping" race-free per docs/05-reserve-accounting.md without a
//! separate row-lock primitive — SQLite's write lock IS the lock. A crash
//! mid-operation leaves the last COMMITted state on disk (WAL mode); there
//! is no partial-write state to recover from, and every observation-
//! processing entry point below is additionally idempotent (checked via a
//! UNIQUE constraint or an explicit already-processed check) so replaying
//! the same chain event after a restart is always safe (constraint 5).

mod schema;
mod types;

pub use types::{
    AdminAuditEntry, AdminAuditFilter, AdminAuditOutcome, AdminAuditRow, BridgeRequest,
    CustodyTransition, CustodyTransitionKind, CustodyTransitionState, Direction, RebalanceKind,
    RebalanceRequest, RebalanceState, RequestAmounts, RequestState, ReserveDirection,
};

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("reserve {0:?} has not been initialized")]
    ReserveNotInitialized(ReserveDirection),
    #[error("bridge request {0} not found")]
    RequestNotFound(i64),
    #[error(
        "accounting invariant violated for {direction:?}: balance {balance} < protected_minimum \
         {protected_minimum} + reserved_liquidity {reserved_liquidity}"
    )]
    InvariantViolated {
        direction: ReserveDirection,
        balance: i64,
        protected_minimum: i64,
        reserved_liquidity: i64,
    },
    #[error("requested {requested} vault UTXOs to reserve, but only {available} of them are still Available (a concurrent reservation won)")]
    VaultUtxoUnavailable { requested: usize, available: usize },
    #[error("a Goldcoin payout already exists for request {0}")]
    PayoutAlreadyExists(i64),
    #[error("no Goldcoin payout record exists for request {0}")]
    PayoutNotFound(i64),
    #[error("vault UTXO {}:{vout} reserved for request {request_id}'s payout is no longer reserved exactly as it was left — needs operator investigation before recovery can proceed", crate::goldcoin::hex::encode(txid))]
    VaultUtxoReservationDrifted {
        request_id: i64,
        txid: [u8; 32],
        vout: u32,
    },
    #[error(
        "cannot finalize request {0}: on-chain completion has not been submitted/confirmed yet"
    )]
    CompletionNotSubmitted(i64),
    #[error("invalid rebalance request: {0}")]
    InvalidRebalanceRequest(String),
    #[error("rebalance request {0} not found")]
    RebalanceNotFound(i64),
    #[error("rebalance request {id} is in state {actual:?}, expected {expected:?}")]
    RebalanceWrongState {
        id: i64,
        expected: RebalanceState,
        actual: RebalanceState,
    },
    #[error("invalid custody transition: {0}")]
    InvalidCustodyTransition(String),
    #[error("custody transition {0} not found")]
    CustodyTransitionNotFound(i64),
    #[error("custody transition {id} is in state {actual:?}, expected {expected:?}")]
    CustodyTransitionWrongState {
        id: i64,
        expected: CustodyTransitionState,
        actual: CustodyTransitionState,
    },
    #[error(
        "custody transition {id} requires {direction:?} to be paused before execution can be recorded"
    )]
    CustodyTransitionRequiresPause {
        id: i64,
        direction: ReserveDirection,
    },
    /// [`Ledger::set_glc_to_sol_deposit_address`] was called for a
    /// request that isn't `GlcToSol` — only that direction has a
    /// Goldcoin deposit step at all.
    #[error("request {id} is {actual_direction:?}, not GlcToSol — it has no Goldcoin deposit address to assign")]
    NotAGlcToSolRequest {
        id: i64,
        actual_direction: Direction,
    },
    /// The request already has a DIFFERENT deposit address assigned.
    /// Never silently overwritten — a request-specific deposit address,
    /// once assigned, may already have been shown to a user or received
    /// funds; changing it out from under either would be a real
    /// accounting hazard, not just a cosmetic inconsistency. Calling
    /// this again with the SAME address is fine (idempotent).
    #[error("request {id} already has deposit address {existing}, cannot reassign to {attempted}")]
    DepositAddressAlreadySet {
        id: i64,
        existing: String,
        attempted: String,
    },
    #[error(
        "no vault UTXO {}:{vout} is known to this ledger",
        crate::goldcoin::hex::encode(txid)
    )]
    VaultUtxoNotFound { txid: [u8; 32], vout: u32 },
    #[error(
        "vault UTXO {}:{vout} is not splittable — state is {state}, not Available",
        crate::goldcoin::hex::encode(txid)
    )]
    VaultUtxoNotSplittable {
        txid: [u8; 32],
        vout: u32,
        state: String,
    },
    #[error(
        "vault UTXO {}:{vout} has already been split",
        crate::goldcoin::hex::encode(txid)
    )]
    VaultUtxoAlreadySplit { txid: [u8; 32], vout: u32 },
    #[error("vault UTXO split #{0} not found")]
    VaultUtxoSplitNotFound(i64),
    #[error(
        "vault UTXO split #{id} is in state {state} — the requested transition does not apply"
    )]
    VaultUtxoSplitNotRecoverable { id: i64, state: String },
    /// [`Ledger::resume_manual_review_sol_to_glc`] was called for a
    /// request that isn't `SolToGlc` — only that direction can land in
    /// `ManualReview` via [`Ledger::fold_sol_deposit`]'s admission/
    /// capacity gate, which is the only thing this command resumes.
    #[error("request {id} is {actual_direction:?}, not SolToGlc — this command only resumes a SolToGlc request parked by fold_sol_deposit")]
    NotASolToGlcRequest {
        id: i64,
        actual_direction: Direction,
    },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses: the request is
    /// not in a state this command can safely act on (wrong state, an
    /// unrecognized/non-fold `manual_review_note`, a Goldcoin payout or
    /// destination transaction already exists, or the source deposit was
    /// never finalized). Deliberately one variant with a human-readable
    /// detail, mirroring `signing::goldcoin_vault::SigningError::
    /// PayoutNotRecoverable`'s shape — every case here is "no, and here is
    /// exactly why," not a distinct recovery path per cause.
    #[error("request {id} cannot be resumed from ManualReview: {detail}")]
    ManualReviewNotRecoverable { id: i64, detail: String },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses (no override,
    /// no mutation — the request is left exactly as it was in
    /// `ManualReview`): the mature Goldcoin UTXO pool is still at or below
    /// `utxo_pool_min_available_count`, the same count-based admission
    /// gate [`Ledger::fold_sol_deposit`] applies to a brand-new
    /// obligation, applied here to something already accepted so a resume
    /// can never re-admit demand the mature pool still can't safely
    /// support (docs/09-runbook.md's "UTXO liquidity" section). Retrying
    /// this exact same call once `available_utxo_count` recovers succeeds
    /// normally — this is a transient, self-clearing refusal, not a
    /// terminal one.
    #[error(
        "cannot resume request {request_id}: mature Goldcoin UTXO pool ({available_utxo_count} \
         available) is still at or below the configured floor ({min_available_count}) — \
         utxo_liquidity_low"
    )]
    UtxoLiquidityLow {
        request_id: i64,
        available_utxo_count: i64,
        min_available_count: i64,
    },
    /// [`Ledger::check_utxo_liquidity_for_admission`] refuses (no
    /// override, no mutation): the mature Goldcoin UTXO pool is still at
    /// or below `utxo_pool_min_available_count`, the same count-based
    /// admission gate [`Ledger::fold_sol_deposit`] applies to a brand-new
    /// obligation — reopening admission onto a pool this thin would
    /// immediately re-admit exactly the demand backpressure exists to
    /// hold back. Always includes `own_unconfirmed_change_atomic` so an
    /// operator can see, in the same error, whether the "missing"
    /// liquidity is already known and en route to maturing rather than
    /// genuinely gone. Never produced for `SolanaReserve`, which has no
    /// UTXO-pool concept — Solana admission is completely unaffected.
    #[error(
        "cannot open admission for {direction:?}: mature Goldcoin UTXO pool \
         ({available_utxo_count} available) is still at or below the configured floor \
         ({min_available_count}) — utxo_liquidity_low ({own_unconfirmed_change_atomic} atomic \
         units are known to be this service's own unconfirmed payout change, not yet spendable)"
    )]
    UtxoLiquidityLowForAdmission {
        direction: ReserveDirection,
        available_utxo_count: i64,
        min_available_count: i64,
        own_unconfirmed_change_atomic: u64,
    },
    #[error(
        "no unmatched Goldcoin deposit {}:{vout} is known to this ledger",
        crate::goldcoin::hex::encode(txid)
    )]
    UnmatchedDepositNotFound { txid: [u8; 32], vout: u32 },
    /// [`Ledger::reconcile_unmatched_goldcoin_deposit`] refuses: no
    /// `Broadcast` `vault_utxo_splits` transaction with this txid exists,
    /// or this exact `(vout, amount_atomic)` is not one of its expected
    /// outputs. No override — reconciling anything else would mean
    /// marking a genuinely unexplained deposit as explained.
    #[error(
        "unmatched Goldcoin deposit {}:{vout} does not exactly match any known vault split output",
        crate::goldcoin::hex::encode(txid)
    )]
    UnmatchedDepositNotAKnownSplitOutput { txid: [u8; 32], vout: u32 },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses (no override,
    /// no mutation): this recipient still has another qualifying SolToGlc
    /// obligation inside the rolling 24-hour window (docs/09-runbook.md's
    /// recipient rate limit) — checked unconditionally on every resume
    /// attempt, regardless of the request's original `manual_review_note`,
    /// so a manual operator resume can never bypass the window. Only a
    /// STRICT PREDECESSOR (an earlier row, by `(created_at, id)`) to the
    /// same recipient can ever be the blocker named here — a later
    /// sibling can never block an earlier one, which is what keeps
    /// oldest-first draining true for a busy recipient. `retry_after`
    /// is the unix timestamp at which the blocking request ages out of the
    /// window; retrying this exact call at or after that time succeeds
    /// normally, same self-clearing shape as `UtxoLiquidityLow`.
    #[error(
        "cannot resume request {request_id}: recipient {} already received a SolToGlc payout \
         inside the rolling 24-hour window, retry after {retry_after} — recipient_rate_limited",
        crate::goldcoin::hex::encode(recipient)
    )]
    RecipientRateLimited {
        request_id: i64,
        recipient: Vec<u8>,
        retry_after: i64,
    },
    /// [`Ledger::resume_manual_review_sol_to_glc`] refuses (no override,
    /// no mutation): this Solana source wallet (the on-chain
    /// `WithdrawalObligation.requester` — the deposit's actual signer,
    /// never a client-provided string, see `deposit_to_reserve.rs`) still
    /// has another qualifying SolToGlc obligation inside the rolling
    /// 24-hour window — the same rule as [`LedgerError::RecipientRateLimited`],
    /// keyed by the depositor's wallet instead of the Goldcoin recipient,
    /// checked unconditionally on every resume attempt so a manual
    /// operator resume can never bypass this window either. Same
    /// strict-predecessor-only blocking rule and the same self-clearing
    /// `retry_after` shape.
    #[error(
        "cannot resume request {request_id}: Solana source wallet {} already made a SolToGlc \
         deposit inside the rolling 24-hour window, retry after {retry_after} — \
         source_wallet_rate_limited",
        crate::goldcoin::hex::encode(requester)
    )]
    SourceWalletRateLimited {
        request_id: i64,
        requester: Vec<u8>,
        retry_after: i64,
    },
}

pub struct Ledger {
    conn: Connection,
}

/// A write transaction for one Ledger mutation. Standalone (the only
/// case before the admin control plane existed) this is EXACTLY the old
/// `BEGIN IMMEDIATE` transaction — same statement, same write-lock
/// acquisition, same rollback-on-drop. When an admin-action scope is
/// already open on the connection ([`Ledger::begin_admin_action`]) it is
/// a SAVEPOINT instead, so the mutation nests inside the scope and
/// commits or rolls back atomically WITH its audit row rather than
/// failing on a nested `BEGIN`.
enum WriteTx<'conn> {
    Transaction(rusqlite::Transaction<'conn>),
    Savepoint(rusqlite::Savepoint<'conn>),
}

impl<'conn> WriteTx<'conn> {
    fn commit(self) -> rusqlite::Result<()> {
        match self {
            WriteTx::Transaction(tx) => tx.commit(),
            WriteTx::Savepoint(sp) => sp.commit(),
        }
    }

    fn rollback(self) -> rusqlite::Result<()> {
        match self {
            WriteTx::Transaction(tx) => tx.rollback(),
            WriteTx::Savepoint(mut sp) => sp.rollback(),
        }
    }
}

impl std::ops::Deref for WriteTx<'_> {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            WriteTx::Transaction(tx) => tx,
            WriteTx::Savepoint(sp) => sp,
        }
    }
}

/// Begins a [`WriteTx`] on `conn` — a free function over the connection
/// (not a `Ledger` method) so call sites keep the same field-level borrow
/// shape as the `self.conn.transaction_with_behavior(...)` calls it
/// replaced.
fn write_tx(conn: &mut Connection) -> Result<WriteTx<'_>, LedgerError> {
    if conn.is_autocommit() {
        Ok(WriteTx::Transaction(conn.transaction_with_behavior(
            rusqlite::TransactionBehavior::Immediate,
        )?))
    } else {
        Ok(WriteTx::Savepoint(conn.savepoint()?))
    }
}

/// `(from_state, to_state, at, reason)` — one row of a request's audit
/// trail, per [`Ledger::state_log`].
pub type StateLogEntry = (Option<RequestState>, RequestState, i64, Option<String>);

/// `(from_state, to_state, at, reason, actor)` — one row of a rebalance
/// request's audit trail, per [`Ledger::rebalance_state_log`].
pub type RebalanceStateLogEntry = (
    Option<RebalanceState>,
    RebalanceState,
    i64,
    Option<String>,
    String,
);

/// `(from_state, to_state, at, reason, actor)` — one row of a custody
/// transition's audit trail, per [`Ledger::custody_transition_state_log`].
pub type CustodyTransitionStateLogEntry = (
    Option<CustodyTransitionState>,
    CustodyTransitionState,
    i64,
    Option<String>,
    String,
);

/// Outcome of [`Ledger::create_request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateRequestOutcome {
    /// Capacity reserved; request created in `AwaitingDeposit`.
    Reserved { request_id: i64 },
    /// Never accept a transfer that cannot be fulfilled (docs/05): no row
    /// is created, no capacity is touched.
    InsufficientLiquidity { available_capacity: i64 },
    /// The destination reserve (or the bridge globally) is paused.
    Paused,
}

/// See [`Ledger::get_goldcoin_payout`]. `state` is the raw `goldcoin_payouts.state`
/// text value (`'Built'|'Signed'|'Broadcast'|'Confirmed'|'Completed'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldcoinPayoutSnapshot {
    pub payout_atomic: u64,
    pub txid: Option<[u8; 32]>,
    pub state: String,
    pub confirmations: i64,
    pub mined_height: Option<i64>,
    pub onchain_completion_signature: Option<[u8; 64]>,
    /// When `onchain_completion_signature` was last (re-)submitted — what
    /// the orchestrator's completion-confirmation tick uses to decide
    /// that a still-unobserved submission is old enough to have
    /// demonstrably expired and must be re-sent.
    pub onchain_completion_submitted_at: Option<i64>,
}

/// See [`Ledger::get_goldcoin_payout_full`] — every persisted fact about
/// an existing payout that [`crate::goldcoin::payout_recovery`] needs to
/// independently reconstruct and re-verify its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldcoinPayoutFull {
    pub commitment_hash: [u8; 32],
    pub payout_atomic: u64,
    /// Sum of `change_outputs` — kept for every existing consumer that
    /// only needs the total (e.g. `pending_destination_settlement_amount`'s
    /// SQL, unchanged since this migration).
    pub change_atomic: u64,
    /// The deterministic change FAN-OUT itself, in construction order
    /// (`goldcoin::coin::finalize_fanout`) — reconstructed from
    /// `goldcoin_payout_change_outputs` when present, or synthesized as a
    /// single legacy output equal to `change_atomic` for a payout built
    /// before this column existed (never backfilled; see
    /// `schema::apply_v12`). Empty exactly when `change_atomic == 0`.
    pub change_outputs: Vec<u64>,
    pub fee_atomic: u64,
    pub dest_p2pkh_hash: [u8; 20],
    pub unsigned_tx_hex: Option<String>,
    pub signed_tx_hex: Option<String>,
    /// Raw `goldcoin_payouts.state` text value
    /// (`'Built'|'Signed'|'Broadcast'|'Confirmed'|'Completed'`).
    pub state: String,
}

/// The Goldcoin vault's UTXO-pool health, distinguishing what a naive
/// "reserve balance dropped" reading cannot (docs/09-runbook.md's "UTXO
/// liquidity" section): (A) actual reserve loss is neither of these
/// figures — it is whatever a `reconcile` call classifies as an
/// unexplained residual drop; (B) `own_unconfirmed_change_atomic`/
/// `unconfirmed_change_utxo_count` is reserve value KNOWN to be
/// temporarily locked in this service's own broadcast-but-immature payout
/// change, not missing; (C) `mature_available_atomic`/
/// `available_utxo_count` is the real, currently spendable liquidity coin
/// selection can actually draw from right now. See [`Ledger::
/// utxo_pool_health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UtxoPoolHealth {
    pub mature_available_atomic: u64,
    pub own_unconfirmed_change_atomic: u64,
    pub available_utxo_count: u32,
    pub unconfirmed_change_utxo_count: u32,
    /// Authoritative payout change below the confirmed threshold and not
    /// on a parent-validation hold — the 0-conf-spendability policy's
    /// candidate pool (depth cap applied at selection, not here). Shown
    /// SEPARATELY from `mature_available_atomic` so an operator never
    /// mistakes it for confirmed reserve liquidity.
    pub zero_conf_change_candidate_atomic: u64,
    pub zero_conf_change_candidate_count: u32,
    /// Change outputs currently excluded because their parent payout is
    /// not known/accepted by the configured node (see
    /// `Ledger::set_zero_conf_hold`). Nonzero deserves operator
    /// attention: a parent payout may have been evicted or conflicted.
    pub zero_conf_change_held_count: u32,
}

/// A single `vault_utxos` row's live state, as needed by
/// [`crate::goldcoin::split`]'s independent re-derivation — see
/// [`Ledger::get_vault_utxo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultUtxoRow {
    pub amount_atomic: u64,
    pub script_pubkey_hex: String,
    /// Raw `vault_utxos.state` text value
    /// (`'Available'|'Reserved'|'Spent'|'Unconfirmed'`).
    pub state: String,
}

/// One not-yet-`Broadcast` `vault_utxo_splits` row, as returned by
/// [`Ledger::pending_vault_utxo_splits`] — just enough to locate the full
/// snapshot ([`Ledger::get_vault_utxo_split`]) and dispatch the right
/// resume path per `state`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVaultUtxoSplit {
    pub id: i64,
    pub source_txid: [u8; 32],
    pub source_vout: u32,
    /// `'Built'` or `'Signed'`.
    pub state: String,
}

/// One `Broadcast` `vault_utxo_splits` row, as returned by
/// [`Ledger::broadcast_vault_utxo_splits`] — what lifecycle maintenance
/// needs to confirm, re-broadcast, or abandon it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnconfirmedBroadcastSplit {
    pub id: i64,
    pub txid: [u8; 32],
    /// Always present for a `Broadcast` row (`record_vault_utxo_split_
    /// signed` sets it before `Broadcast` is reachable) — `Option` only
    /// because the column is nullable in earlier states.
    pub signed_tx_hex: Option<String>,
}

/// See [`Ledger::get_vault_utxo_split`] — everything an operator or a
/// re-run of `split-vault-utxo` needs to know about a previously attempted
/// split of a given source outpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultUtxoSplitSnapshot {
    pub id: i64,
    pub source_amount_atomic: u64,
    pub chunk_count: i64,
    pub chunk_target_atomic: u64,
    pub fee_atomic: u64,
    pub unsigned_tx_hex: String,
    pub signed_tx_hex: Option<String>,
    pub txid: Option<[u8; 32]>,
    /// Raw `vault_utxo_splits.state` text value
    /// (`'Built'|'Signed'|'Broadcast'`).
    pub state: String,
}

/// See [`Ledger::get_broadcast_vault_utxo_split`] — the already-persisted
/// figures needed to reproduce a `Broadcast` split's exact output list
/// (`crate::goldcoin::split::matches_expected_split_output`) purely from
/// its broadcast `txid`, without touching `unsigned_tx_hex` (this crate's
/// `Transaction` type has no deserializer, deliberately — see
/// `goldcoin::tx` module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastVaultUtxoSplit {
    pub source_amount_atomic: u64,
    pub fee_atomic: u64,
    pub chunk_count: i64,
}

/// Outcome of [`Ledger::reconcile_unmatched_goldcoin_deposit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileUnmatchedDepositOutcome {
    Reconciled,
    /// Already reconciled by a prior call — a safe, non-mutating no-op.
    AlreadyReconciled,
}

/// Outcome of [`Ledger::record_glc_deposit_observed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlcObservationOutcome {
    Recorded,
    /// Already recorded for this exact request+txid+vout (restart replay).
    AlreadyRecorded,
    /// No `AwaitingDeposit` request exists with this id/direction — the
    /// vault payment is real but unmatched. Callers should log it to
    /// [`Ledger::record_unmatched_goldcoin_deposit`] for audit rather than
    /// discard it (never silently ignore a real vault payment).
    NoMatchingRequest,
    /// Observed amount does not equal the request's reserved amount — the
    /// deposit is recorded but routed to `ManualReview` rather than
    /// silently accepted (constraint 6/10: never let an observed amount
    /// override what capacity was actually reserved for).
    AmountMismatch {
        expected: u64,
        observed: u64,
    },
    /// The deposit arrived after this request's reservation had already
    /// `Expired`, but capacity was still available: a fresh reservation was
    /// auto-recreated on the same request and the deposit was recorded
    /// against it, continuing the flow normally (docs/04-state-machines.md
    /// "Open design item: late deposits after expiry").
    LateDepositRecreated,
    /// The deposit arrived after this request's reservation had already
    /// `Expired`, and capacity is no longer available to re-reserve. The
    /// deposit is real and irreversible, so this is routed to
    /// `ManualReview` for a compensating action rather than dropped.
    LateDepositNoCapacity,
}

/// Outcome of [`Ledger::approve_rebalance`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceApprovalOutcome {
    /// This approval was recorded but the threshold has not been reached
    /// yet.
    Recorded { approvals: u32, required: u32 },
    /// This approval was the one that reached `required_approvals`; the
    /// request has moved to `RebalanceState::Approved`.
    ThresholdReached,
}

/// Outcome of [`Ledger::approve_custody_transition`]. Structurally
/// identical to [`RebalanceApprovalOutcome`]; kept as a distinct type so
/// each state machine's approval outcome is self-describing at call
/// sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyApprovalOutcome {
    /// This approval was recorded but the threshold has not been reached
    /// yet.
    Recorded { approvals: u32, required: u32 },
    /// This approval was the one that reached `required_approvals`; the
    /// transition has moved to `CustodyTransitionState::Approved`.
    ThresholdReached,
}

/// Outcome of [`Ledger::fold_sol_deposit`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolFoldOutcome {
    /// Capacity was available; a request was created directly in
    /// `SourceFinalized` (Solana finality is a single instant at the
    /// commitment level, unlike Goldcoin's confirmation-depth ramp — see
    /// module docs on the asymmetry).
    FoldedFinalized { request_id: i64 },
    /// Already folded for this obligation index (restart replay).
    AlreadyFolded { request_id: i64 },
    /// No pre-existing reservation is possible for this direction (the
    /// on-chain `deposit_to_reserve` instruction has no reservation-
    /// correlation parameter — see module docs) and capacity was NOT
    /// available at fold time. The deposit is real and irreversible on
    /// Solana; it is recorded in `ManualReview`, never dropped.
    FoldedManualReview { request_id: i64 },
}

/// Outcome of [`Ledger::resume_manual_review_sol_to_glc`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeManualReviewOutcome {
    /// The request moved `ManualReview -> SourceFinalized` and its
    /// capacity was reserved.
    Resumed,
    /// The request was already past `ManualReview` (a prior call to this
    /// same command already resumed it) — a safe, non-mutating no-op,
    /// safe to call again.
    AlreadyResumed { state: RequestState },
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self, LedgerError> {
        let conn = Connection::open(path)?;
        schema::open_and_migrate(&conn)?;
        Ok(Ledger { conn })
    }

    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        schema::open_and_migrate(&conn)?;
        Ok(Ledger { conn })
    }

    // ------------------------------------------------------------ reserve setup --

    /// Initializes (or re-parameterizes) a reserve's threshold configuration.
    /// Idempotent — safe to call at every startup with the current config.
    /// Does not touch `reserved_liquidity`/`pending_obligations`, which are
    /// derived from live `bridge_requests`, not configuration.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_reserve(
        &mut self,
        direction: ReserveDirection,
        initial_balance: u64,
        protected_minimum: u64,
        target_reserve: u64,
        warning_reserve: u64,
        critical_reserve: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        assert!(
            critical_reserve > protected_minimum,
            "critical_reserve must exceed protected_minimum (docs/05-reserve-accounting.md)"
        );
        self.conn.execute(
            "INSERT INTO reserve_ledger
                (direction, total_reserve_balance, balance_refreshed_at, protected_minimum,
                 target_reserve, warning_reserve, critical_reserve)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(direction) DO UPDATE SET
                protected_minimum = excluded.protected_minimum,
                target_reserve = excluded.target_reserve,
                warning_reserve = excluded.warning_reserve,
                critical_reserve = excluded.critical_reserve",
            rusqlite::params![
                direction,
                initial_balance as i64,
                now,
                protected_minimum as i64,
                target_reserve as i64,
                warning_reserve as i64,
                critical_reserve as i64,
            ],
        )?;
        Ok(())
    }

    /// Updates the cached live-chain balance (called by reconciliation after
    /// a real chain read — never guessed, never left stale silently: callers
    /// must pass an actually-observed balance).
    pub fn refresh_reserve_balance(
        &mut self,
        direction: ReserveDirection,
        observed_balance: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, balance_refreshed_at = ?2
             WHERE direction = ?3",
            rusqlite::params![observed_balance as i64, now, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn set_paused(
        &mut self,
        direction: ReserveDirection,
        paused: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET paused = ?1, pause_reason = ?2 WHERE direction = ?3",
            rusqlite::params![paused as i64, reason, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn is_paused(&self, direction: ReserveDirection) -> Result<bool, LedgerError> {
        let paused: i64 = self
            .conn
            .query_row(
                "SELECT paused FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                // Same actionable error `set_paused` reports for a
                // missing row — an operator on a fresh database needs
                // "configure the reserve", not a storage error.
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(paused != 0)
    }

    /// The last note recorded alongside a [`Ledger::set_paused`] call —
    /// last-write-wins display context for an operator dashboard; the
    /// full history lives in `admin_audit_log`.
    pub fn pause_reason(&self, direction: ReserveDirection) -> Result<Option<String>, LedgerError> {
        let reason: Option<String> = self.conn.query_row(
            "SELECT pause_reason FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| r.get(0),
        )?;
        Ok(reason)
    }

    /// The last note recorded alongside a [`Ledger::set_admission`] call
    /// — same last-write-wins caveat as [`Ledger::pause_reason`].
    pub fn admission_reason(
        &self,
        direction: ReserveDirection,
    ) -> Result<Option<String>, LedgerError> {
        let reason: Option<String> = self.conn.query_row(
            "SELECT admission_reason FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| r.get(0),
        )?;
        Ok(reason)
    }

    /// Closes or opens admission of NEW obligations for `direction` — a
    /// separate axis from [`Ledger::set_paused`] (docs/09-runbook.md's
    /// "Admission control (Solana->Goldcoin)" section). Nothing in this
    /// crate ever calls this automatically: unlike `paused` (which
    /// reconciliation/the rolling-volume quota can set on a breach),
    /// `admission_closed` changes ONLY via an explicit operator call
    /// (`glc-admin close-admission`/`open-admission`) — there is no
    /// automatic reopen, and nothing auto-closes it either.
    pub fn set_admission(
        &mut self,
        direction: ReserveDirection,
        closed: bool,
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET admission_closed = ?1, admission_reason = ?2 WHERE direction = ?3",
            rusqlite::params![closed as i64, reason, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    pub fn is_admission_closed(&self, direction: ReserveDirection) -> Result<bool, LedgerError> {
        let closed: i64 = self
            .conn
            .query_row(
                "SELECT admission_closed FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(closed != 0)
    }

    /// Configures GoldcoinReserve's UTXO-liquidity admission backpressure
    /// (docs/09-runbook.md's "UTXO liquidity" section):
    /// `min_available_count` is the number of mature, unreserved vault
    /// UTXOs that must remain after admitting one more SolToGlc obligation
    /// — `Ledger::fold_sol_deposit` parks (never drops) a new obligation to
    /// `ManualReview` with reason `utxo_liquidity_low_at_fold` whenever the
    /// live count would fall to or below this floor, exactly the same
    /// fail-closed shape as its existing `paused`/`admission_closed`
    /// checks. `warning_count` (>= `min_available_count`) is purely
    /// observational — surfaced via `Ledger::utxo_pool_health` for
    /// operator visibility before backpressure actually engages; never
    /// itself gates admission. Defaults to `(0, 0)` (no backpressure, no
    /// warning) on every reserve until explicitly configured — idempotent,
    /// safe to call at every startup with the current config, matching
    /// `configure_reserve`.
    pub fn set_utxo_pool_thresholds(
        &mut self,
        direction: ReserveDirection,
        min_available_count: u32,
        warning_count: u32,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE reserve_ledger SET utxo_pool_min_available_count = ?1, utxo_pool_warning_count = ?2
             WHERE direction = ?3",
            rusqlite::params![min_available_count, warning_count, direction],
        )?;
        if n == 0 {
            return Err(LedgerError::ReserveNotInitialized(direction));
        }
        Ok(())
    }

    /// The `(min_available_count, warning_count)` pair last set by
    /// [`Ledger::set_utxo_pool_thresholds`] — `(0, 0)` (no backpressure, no
    /// warning) until explicitly configured. Read by
    /// [`crate::ops::reserve_health`] so an operator can see how close
    /// `utxo_pool_health().available_utxo_count` is to engaging
    /// backpressure, without duplicating the threshold values.
    pub fn utxo_pool_thresholds(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u32, u32), LedgerError> {
        let (min_available_count, warning_count): (u32, u32) = self.conn.query_row(
            "SELECT utxo_pool_min_available_count, utxo_pool_warning_count
             FROM reserve_ledger WHERE direction = ?1",
            [direction],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((min_available_count, warning_count))
    }

    /// `total_reserve_balance - protected_minimum - reserved_liquidity`
    /// (docs/05-reserve-accounting.md). Not clamped at zero deliberately:
    /// a negative value is itself diagnostic (see [`Ledger::check_invariant`]).
    ///
    /// Deliberately does NOT subtract `accrued_fees_atomic`
    /// (docs/20-bridge-fee.md): `reserved_liquidity`/`pending_obligations`/
    /// `settled_liquidity_total` already track NET customer entitlements
    /// only (never gross), so fee revenue was never counted as a customer
    /// obligation in the first place — there is nothing to double-subtract.
    /// A separate subtraction here would incorrectly shrink capacity by
    /// the fee amount twice.
    pub fn available_capacity(&self, direction: ReserveDirection) -> Result<i64, LedgerError> {
        let (balance, protected_minimum, reserved) = self.reserve_row(direction)?;
        Ok(balance - protected_minimum - reserved)
    }

    fn reserve_row(&self, direction: ReserveDirection) -> Result<(i64, i64, i64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    /// Asserts `available reserves >= all releases that can currently become
    /// payable` — i.e. `total_reserve_balance >= protected_minimum +
    /// reserved_liquidity`. Called defensively by tests after every mutating
    /// operation and by reconciliation; a violation here means the ledger's
    /// own bookkeeping has diverged from what it promised, which must never
    /// happen by construction and is treated as a hard error, not a
    /// warning.
    pub fn check_invariant(&self, direction: ReserveDirection) -> Result<(), LedgerError> {
        let (balance, protected_minimum, reserved) = self.reserve_row(direction)?;
        if balance < protected_minimum + reserved {
            return Err(LedgerError::InvariantViolated {
                direction,
                balance,
                protected_minimum,
                reserved_liquidity: reserved,
            });
        }
        Ok(())
    }

    /// The same count-based admission gate [`Ledger::fold_sol_deposit`]
    /// applies to a brand-new obligation, applied here to reopening
    /// admission direction-wide: refuses (no override) if the mature
    /// Goldcoin UTXO pool is still at or below `utxo_pool_min_available_count`,
    /// so admission is never reopened onto a pool this thin — that would
    /// immediately re-admit exactly the demand backpressure exists to hold
    /// back. Does NOT replace [`Ledger::check_invariant`] — callers must
    /// still check that separately; this check is purely additive and
    /// never weakens the hard reserve invariant. Always `Ok(())` for
    /// `SolanaReserve`, which has no UTXO-pool concept — Solana admission
    /// behavior is completely unaffected by this check.
    pub fn check_utxo_liquidity_for_admission(
        &self,
        direction: ReserveDirection,
    ) -> Result<(), LedgerError> {
        if direction != ReserveDirection::GoldcoinReserve {
            return Ok(());
        }
        let (min_available_count, _warning_count) = self.utxo_pool_thresholds(direction)?;
        let pool = self.utxo_pool_health()?;
        let available_utxo_count = pool.available_utxo_count as i64;
        let min_available_count = min_available_count as i64;
        // `== 0` means backpressure is disabled — identical short-circuit
        // to `fold_sol_deposit`'s own.
        let utxo_liquidity_ok =
            min_available_count == 0 || available_utxo_count > min_available_count;
        if !utxo_liquidity_ok {
            return Err(LedgerError::UtxoLiquidityLowForAdmission {
                direction,
                available_utxo_count,
                min_available_count,
                own_unconfirmed_change_atomic: pool.own_unconfirmed_change_atomic,
            });
        }
        Ok(())
    }

    // -------------------------------------------------------------- reservation --

    /// Never accept a transfer that cannot be fulfilled: capacity check and
    /// reservation write are one atomic transaction. `amounts` must already
    /// be a fully computed, internally consistent gross/fee/net breakdown
    /// (`amount_conversion::compute_fee`, converted to the destination's
    /// native unit) — the ledger never computes a fee or a conversion
    /// itself (docs/20-bridge-fee.md). The capacity check compares
    /// `amounts.net_destination_atomic` (what the destination reserve must
    /// actually release) against available capacity, NOT the gross amount
    /// the user declared.
    pub fn create_request(
        &mut self,
        direction: Direction,
        amounts: RequestAmounts,
        recipient: &[u8],
        requester: Option<[u8; 32]>,
        reservation_ttl_secs: i64,
        now: i64,
    ) -> Result<CreateRequestOutcome, LedgerError> {
        let reserve = direction.destination_reserve();
        let tx = write_tx(&mut self.conn)?;

        let paused: i64 = tx.query_row(
            "SELECT paused FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| r.get(0),
        )?;
        if paused != 0 {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::Paused);
        }

        let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
            "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let available = balance - protected_minimum - reserved;
        if (amounts.net_destination_atomic as i64) > available {
            tx.rollback()?;
            return Ok(CreateRequestOutcome::InsufficientLiquidity {
                available_capacity: available,
            });
        }

        tx.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, reservation_expires_at, source_confirmations)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, 0)",
            rusqlite::params![
                direction,
                RequestState::AwaitingDeposit,
                amounts.gross_atomic as i64,
                amounts.fee_bps as i64,
                amounts.fee_atomic as i64,
                amounts.net_atomic as i64,
                amounts.net_destination_atomic as i64,
                recipient,
                requester.map(|r| r.to_vec()),
                now,
                now + reservation_ttl_secs,
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            RequestState::LiquidityReserved,
            now,
            None,
            "system",
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::LiquidityReserved),
            RequestState::AwaitingDeposit,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1 WHERE direction = ?2",
            rusqlite::params![amounts.net_destination_atomic as i64, reserve],
        )?;
        tx.commit()?;
        Ok(CreateRequestOutcome::Reserved { request_id })
    }

    /// Sweeps `AwaitingDeposit`/`LiquidityReserved` requests past their
    /// `reservation_expires_at`, releasing their reserved capacity. Returns
    /// the number expired. Idempotent — a request already past `Expired`
    /// is never matched again by the `WHERE` clause.
    pub fn expire_reservations(&mut self, now: i64) -> Result<u32, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut stmt = tx.prepare(
            "SELECT id, direction, net_destination_atomic FROM bridge_requests
             WHERE state = 'AwaitingDeposit' AND reservation_expires_at IS NOT NULL
               AND reservation_expires_at <= ?1",
        )?;
        let rows: Vec<(i64, Direction, i64)> = stmt
            .query_map([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        let mut count = 0u32;
        for (id, direction, amount) in rows {
            tx.execute(
                "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
                rusqlite::params![RequestState::Expired, id],
            )?;
            log_transition(
                &tx,
                id,
                Some(RequestState::AwaitingDeposit),
                RequestState::Expired,
                now,
                Some("reservation_ttl_elapsed"),
                "system",
            )?;
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1 WHERE direction = ?2",
                rusqlite::params![amount, direction.destination_reserve()],
            )?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Operator/user cancellation before a deposit is observed. Same
    /// capacity-release effect as expiry, distinct reason.
    pub fn cancel_request(&mut self, id: i64, now: i64, note: &str) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, amount, state): (Direction, i64, RequestState) = tx
            .query_row(
                "SELECT direction, net_destination_atomic, state FROM bridge_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(id))?;
        assert!(
            matches!(
                state,
                RequestState::LiquidityReserved | RequestState::AwaitingDeposit
            ),
            "cancel_request called on a request past reservation ({state:?}); caller bug"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = ?2 WHERE id = ?3",
            rusqlite::params![RequestState::Cancelled, note, id],
        )?;
        log_transition(
            &tx,
            id,
            Some(state),
            RequestState::Cancelled,
            now,
            Some(note),
            "operator",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1 WHERE direction = ?2",
            rusqlite::params![amount, direction.destination_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------------ Goldcoin leg --

    /// Looks up a request by id, for the Goldcoin indexer's OP_RETURN-
    /// encoded-id correlation (docs/01-reuse-inventory.md notes this
    /// replaces recipient-only matching to remove FIFO ambiguity).
    pub fn get_request(&self, id: i64) -> Result<Option<BridgeRequest>, LedgerError> {
        self.conn
            .query_row(SELECT_REQUEST, [id], row_to_request)
            .optional()
            .map_err(LedgerError::from)
    }

    // --------------------------------------------- unique deposit addresses --
    //
    // Schema/ledger support for the OP_RETURN-replacement redesign.
    // `request_id` doubles as the derivation index (`goldcoin::
    // derivation`'s own docs) — nothing here derives an address itself;
    // callers compute it via `goldcoin::derivation::derive_request_vault`
    // and pass the result in. The indexer (`goldcoin::indexer`), the API
    // (`api::BridgeApi::create_glc_to_sol_transfer`), and the SolToGlc
    // payout path (`signing::goldcoin_vault::rederive_plan`) all read
    // these columns now.

    /// Assigns a freshly-derived Goldcoin deposit address to a `GlcToSol`
    /// request. Idempotent on an exact repeat (same address); fails
    /// closed — never silently overwrites — if the request already has a
    /// DIFFERENT address, or isn't `GlcToSol` at all (only that
    /// direction has a Goldcoin deposit step). The database-level
    /// partial unique index on `deposit_script_pubkey_hex`
    /// (`ux_bridge_requests_deposit_script`) is the actual, race-safe
    /// guarantee that no two requests are ever assigned the same
    /// deposit script — this method's own pre-check is a friendlier
    /// error message for the ordinary case, not the safety boundary
    /// itself.
    pub fn set_glc_to_sol_deposit_address(
        &mut self,
        request_id: i64,
        address: &str,
        script_pubkey_hex: &str,
        redeem_script_hex: &str,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Direction, Option<String>)> = tx
            .query_row(
                "SELECT direction, deposit_address FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((direction, existing_address)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if direction != Direction::GlcToSol {
            tx.rollback()?;
            return Err(LedgerError::NotAGlcToSolRequest {
                id: request_id,
                actual_direction: direction,
            });
        }
        if let Some(existing) = existing_address {
            tx.rollback()?;
            if existing == address {
                return Ok(());
            }
            return Err(LedgerError::DepositAddressAlreadySet {
                id: request_id,
                existing,
                attempted: address.to_string(),
            });
        }
        tx.execute(
            "UPDATE bridge_requests
             SET deposit_address = ?1, deposit_script_pubkey_hex = ?2, deposit_redeem_script_hex = ?3
             WHERE id = ?4",
            rusqlite::params![address, script_pubkey_hex, redeem_script_hex, request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Resolves a live on-chain P2SH scriptPubKey to the `GlcToSol`
    /// request it was assigned to, if any — the indexer's future
    /// address-based match step (not wired in yet). `script_pubkey_hex`
    /// must be compared byte-for-byte as produced by
    /// [`crate::goldcoin::vault::MultisigVault::script_pubkey_hex`] —
    /// this does no normalization (matches this codebase's existing
    /// exact-match convention for the legacy `vault_script_hex`
    /// comparison in `goldcoin::deposit::vault_output_candidates`).
    pub fn find_glc_to_sol_request_by_deposit_script(
        &self,
        script_pubkey_hex: &str,
    ) -> Result<Option<i64>, LedgerError> {
        self.conn
            .query_row(
                "SELECT id FROM bridge_requests
                 WHERE direction = 'GlcToSol' AND deposit_script_pubkey_hex = ?1",
                [script_pubkey_hex],
                |r| r.get(0),
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Every deposit scriptPubKey ever assigned to a `GlcToSol` request,
    /// regardless of that request's current state — a future indexer
    /// widening its watch-list needs the full historical set, not just
    /// currently-open requests, since a settled request's UTXO can still
    /// sit unswept at its derived address. Not currently called by
    /// anything; exists so this capability exists once the indexer step
    /// needs it.
    pub fn all_glc_to_sol_deposit_script_pubkeys(&self) -> Result<Vec<String>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT deposit_script_pubkey_hex FROM bridge_requests
             WHERE direction = 'GlcToSol' AND deposit_script_pubkey_hex IS NOT NULL",
        )?;
        let rows: Result<Vec<String>, _> = stmt.query_map([], |r| r.get(0))?.collect();
        Ok(rows?)
    }

    /// Every deposit ADDRESS ever assigned to a `GlcToSol` request — same
    /// full-historical-set discipline as
    /// [`Ledger::all_glc_to_sol_deposit_script_pubkeys`], but returning the
    /// human-readable address `listunspent` actually accepts
    /// (`Orchestrator::watched_goldcoin_addresses`), not the scriptPubKey
    /// used for indexer-side matching.
    pub fn all_glc_to_sol_deposit_addresses(&self) -> Result<Vec<String>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT deposit_address FROM bridge_requests
             WHERE direction = 'GlcToSol' AND deposit_address IS NOT NULL",
        )?;
        let rows: Result<Vec<String>, _> = stmt.query_map([], |r| r.get(0))?.collect();
        Ok(rows?)
    }

    /// Raw `bridge_requests.destination_txid` bytes — a 64-byte Solana
    /// transaction signature for a `GlcToSol` release
    /// ([`Ledger::record_release_submitted`]) or a 32-byte Goldcoin txid
    /// for a `SolToGlc` payout ([`Ledger::record_goldcoin_payout_broadcast`]);
    /// length depends on direction, so this returns the raw bytes rather
    /// than a fixed-size array.
    pub fn get_destination_txid(&self, request_id: i64) -> Result<Option<Vec<u8>>, LedgerError> {
        Ok(self
            .conn
            .query_row(
                "SELECT destination_txid FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Records that a candidate Goldcoin deposit binds to `request_id`.
    /// Idempotent on `(source_txid, source_vout)` — calling this twice with
    /// the same observation after a restart returns `AlreadyRecorded`
    /// rather than erroring or double-counting.
    ///
    /// If `request_id` is already `Expired` (deposit arrived after the
    /// reservation TTL elapsed), this implements
    /// docs/04-state-machines.md's late-deposit auto-recreate: capacity is
    /// re-checked and, if available, re-reserved on the same request before
    /// continuing the flow normally (see [`GlcObservationOutcome::LateDepositRecreated`]);
    /// otherwise the request is routed to `ManualReview`
    /// ([`GlcObservationOutcome::LateDepositNoCapacity`]) rather than
    /// treated as an uncorrelated payment.
    #[allow(clippy::too_many_arguments)]
    pub fn record_glc_deposit_observed(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        vout: u32,
        observed_amount: u64,
        block_height: i64,
        block_hash: [u8; 32],
        now: i64,
    ) -> Result<GlcObservationOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut recreated_from_expired = false;
        #[allow(clippy::type_complexity)]
        let row: Option<(Direction, RequestState, i64, i64, Option<Vec<u8>>)> = tx
            .query_row(
                "SELECT direction, state, gross_amount_atomic, net_destination_atomic, source_txid
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((direction, mut state, reserved_amount, net_destination_atomic, existing_txid)) =
            row
        else {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        };

        if (state == RequestState::DepositObserved || state == RequestState::Confirming)
            && existing_txid.as_deref() == Some(txid.as_slice())
        {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::AlreadyRecorded);
        }
        if direction != Direction::GlcToSol {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        }

        // Late deposit: the reservation TTL elapsed before this deposit was
        // observed, but the Goldcoin payment is real and irreversible
        // (docs/04-state-machines.md "Open design item: late deposits after
        // expiry"). Never fold this into the uncorrelated-payment path
        // below (`NoMatchingRequest`) — the OP_RETURN binding already
        // resolved this to a specific request, so the request's own
        // capacity, not "any capacity", is what must be re-checked.
        if state == RequestState::Expired {
            let reserve = direction.destination_reserve();
            let (balance, protected_minimum, reserved_liquidity): (i64, i64, i64) = tx.query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
                 FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let available = balance - protected_minimum - reserved_liquidity;
            if net_destination_atomic > available {
                tx.execute(
                    "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                        source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6
                     WHERE id = ?7",
                    rusqlite::params![
                        RequestState::ManualReview,
                        txid.as_slice(),
                        vout,
                        block_height,
                        block_hash.as_slice(),
                        "late_deposit_no_capacity",
                        request_id,
                    ],
                )?;
                log_transition(
                    &tx,
                    request_id,
                    Some(RequestState::Expired),
                    RequestState::ManualReview,
                    now,
                    Some("late_deposit_no_capacity"),
                    "system",
                )?;
                tx.commit()?;
                return Ok(GlcObservationOutcome::LateDepositNoCapacity);
            }

            tx.execute(
                "UPDATE bridge_requests
                 SET state = ?1, reserved_at = ?2, reservation_expires_at = NULL
                 WHERE id = ?3",
                rusqlite::params![RequestState::AwaitingDeposit, now, request_id],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::Expired),
                RequestState::LiquidityReserved,
                now,
                Some("late_deposit_recreated"),
                "system",
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::LiquidityReserved),
                RequestState::AwaitingDeposit,
                now,
                None,
                "system",
            )?;
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1 WHERE direction = ?2",
                rusqlite::params![net_destination_atomic, reserve],
            )?;
            state = RequestState::AwaitingDeposit;
            recreated_from_expired = true;
        }

        if state != RequestState::AwaitingDeposit {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        }

        if observed_amount != reserved_amount as u64 {
            tx.execute(
                "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                    source_block_height = ?4, source_block_hash = ?5, manual_review_note = ?6
                 WHERE id = ?7",
                rusqlite::params![
                    RequestState::ManualReview,
                    txid.as_slice(),
                    vout,
                    block_height,
                    block_hash.as_slice(),
                    format!("deposit_amount_mismatch: expected {reserved_amount} observed {observed_amount}"),
                    request_id,
                ],
            )?;
            log_transition(
                &tx,
                request_id,
                Some(RequestState::AwaitingDeposit),
                RequestState::ManualReview,
                now,
                Some("deposit_amount_mismatch"),
                "system",
            )?;
            tx.commit()?;
            return Ok(GlcObservationOutcome::AmountMismatch {
                expected: reserved_amount as u64,
                observed: observed_amount,
            });
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_txid = ?2, source_vout = ?3,
                source_block_height = ?4, source_block_hash = ?5, source_confirmations = 1
             WHERE id = ?6",
            rusqlite::params![
                RequestState::DepositObserved,
                txid.as_slice(),
                vout,
                block_height,
                block_hash.as_slice(),
                request_id,
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::AwaitingDeposit),
            RequestState::DepositObserved,
            now,
            None,
            "system",
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::DepositObserved),
            RequestState::Confirming,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![RequestState::Confirming, request_id],
        )?;
        tx.commit()?;
        if recreated_from_expired {
            Ok(GlcObservationOutcome::LateDepositRecreated)
        } else {
            Ok(GlcObservationOutcome::Recorded)
        }
    }

    /// Creates `unmatched_goldcoin_deposits` if it doesn't exist yet (fresh
    /// database) and ensures the `reconciled_at` column is present
    /// (idempotent `ALTER`, same `column_exists`-guarded discipline as
    /// `schema::apply_v9` — this table lives outside the versioned
    /// schema-migration system, created ad hoc on first use, so it needs
    /// its own idempotent-columnar handling rather than a numbered
    /// migration). Never drops or recreates anything already there.
    fn ensure_unmatched_goldcoin_deposits_table(conn: &Connection) -> Result<(), LedgerError> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS unmatched_goldcoin_deposits (
                id INTEGER PRIMARY KEY, txid BLOB NOT NULL, vout INTEGER NOT NULL,
                amount_atomic INTEGER NOT NULL, block_height INTEGER NOT NULL,
                reason TEXT NOT NULL, discovered_at INTEGER NOT NULL,
                reconciled_at INTEGER, reconciliation_note TEXT,
                UNIQUE(txid, vout)
             )",
            [],
        )?;
        if !schema::column_exists(conn, "unmatched_goldcoin_deposits", "reconciled_at")? {
            conn.execute(
                "ALTER TABLE unmatched_goldcoin_deposits ADD COLUMN reconciled_at INTEGER",
                [],
            )?;
        }
        if !schema::column_exists(conn, "unmatched_goldcoin_deposits", "reconciliation_note")? {
            conn.execute(
                "ALTER TABLE unmatched_goldcoin_deposits ADD COLUMN reconciliation_note TEXT",
                [],
            )?;
        }
        Ok(())
    }

    /// A real vault payment that could not be matched to any pending
    /// request — recorded for audit rather than dropped (constraint: never
    /// silently ignore a real chain observation).
    pub fn record_unmatched_goldcoin_deposit(
        &mut self,
        txid: [u8; 32],
        vout: u32,
        amount_atomic: u64,
        block_height: i64,
        reason: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        Self::ensure_unmatched_goldcoin_deposits_table(&self.conn)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO unmatched_goldcoin_deposits
                (txid, vout, amount_atomic, block_height, reason, discovered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                txid.as_slice(),
                vout,
                amount_atomic as i64,
                block_height,
                reason,
                now
            ],
        )?;
        Ok(())
    }

    /// Marks a previously-recorded unmatched deposit `reconciled_at = now`
    /// — never deletes the row, so the audit history stays intact
    /// (docs/09-runbook.md's "Vault UTXO splitting" section). Refuses (no
    /// override) unless `(txid, vout, amount_atomic)` exactly matches an
    /// expected output of a `Broadcast` `vault_utxo_splits` transaction —
    /// the same [`crate::goldcoin::split::matches_expected_split_output`]
    /// check `goldcoin::indexer` uses to recognize a split output live, so
    /// this can retroactively reconcile a row recorded before that
    /// recognition existed. Idempotent: reconciling an already-reconciled
    /// row again is a safe no-op reporting so, not a second write.
    pub fn reconcile_unmatched_goldcoin_deposit(
        &mut self,
        txid: [u8; 32],
        vout: u32,
        note: &str,
        now: i64,
    ) -> Result<ReconcileUnmatchedDepositOutcome, LedgerError> {
        Self::ensure_unmatched_goldcoin_deposits_table(&self.conn)?;
        let tx = write_tx(&mut self.conn)?;

        let row: Option<(i64, Option<i64>)> = tx
            .query_row(
                "SELECT amount_atomic, reconciled_at FROM unmatched_goldcoin_deposits
                 WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![txid.as_slice(), vout],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((amount_atomic, reconciled_at)) = row else {
            tx.rollback()?;
            return Err(LedgerError::UnmatchedDepositNotFound { txid, vout });
        };
        if reconciled_at.is_some() {
            tx.rollback()?;
            return Ok(ReconcileUnmatchedDepositOutcome::AlreadyReconciled);
        }

        let split: Option<(i64, i64, i64)> = tx
            .query_row(
                "SELECT source_amount_atomic, fee_atomic, chunk_count
                 FROM vault_utxo_splits WHERE txid = ?1 AND state IN ('Broadcast','Confirmed')",
                [txid.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((source_amount_atomic, fee_atomic, chunk_count)) = split else {
            tx.rollback()?;
            return Err(LedgerError::UnmatchedDepositNotAKnownSplitOutput { txid, vout });
        };
        let matches = crate::goldcoin::split::matches_expected_split_output(
            source_amount_atomic as u64,
            fee_atomic as u64,
            chunk_count as u64,
            vout,
            amount_atomic as u64,
        );
        if !matches {
            tx.rollback()?;
            return Err(LedgerError::UnmatchedDepositNotAKnownSplitOutput { txid, vout });
        }

        tx.execute(
            "UPDATE unmatched_goldcoin_deposits SET reconciled_at = ?1, reconciliation_note = ?2
             WHERE txid = ?3 AND vout = ?4",
            rusqlite::params![now, note, txid.as_slice(), vout],
        )?;
        tx.commit()?;
        Ok(ReconcileUnmatchedDepositOutcome::Reconciled)
    }

    /// The already-persisted figures a `Broadcast` `vault_utxo_splits`
    /// transaction's output list was deterministically built from — what
    /// [`crate::goldcoin::split::matches_expected_split_output`] needs to
    /// reproduce that exact output list independently, from `txid` alone.
    pub fn get_broadcast_vault_utxo_split(
        &self,
        split_txid: [u8; 32],
    ) -> Result<Option<BroadcastVaultUtxoSplit>, LedgerError> {
        self.conn
            .query_row(
                "SELECT source_amount_atomic, fee_atomic, chunk_count
                 FROM vault_utxo_splits WHERE txid = ?1 AND state IN ('Broadcast','Confirmed')",
                [split_txid.as_slice()],
                |r| {
                    Ok(BroadcastVaultUtxoSplit {
                        source_amount_atomic: r.get::<_, i64>(0)? as u64,
                        fee_atomic: r.get::<_, i64>(1)? as u64,
                        chunk_count: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Updates confirmation depth for a `Confirming` request; a no-op if
    /// the depth hasn't increased (idempotent under repeated ticks).
    pub fn update_glc_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE bridge_requests SET source_confirmations = ?1
             WHERE id = ?2 AND state = 'Confirming' AND source_confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        Ok(())
    }

    /// `Confirming -> SourceFinalized`: the source deposit is now treated as
    /// an irreversible fact. Moves the amount into `pending_obligations`
    /// (docs/05: committed exposure that can no longer safely expire).
    /// Idempotent: a no-op if already `SourceFinalized`.
    pub fn mark_glc_source_finalized(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Direction, RequestState, i64)> = tx
            .query_row(
                "SELECT direction, state, net_destination_atomic FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((direction, state, amount)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if state == RequestState::SourceFinalized {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::Confirming,
            "mark_glc_source_finalized on unexpected state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_finalized_at = ?2 WHERE id = ?3",
            rusqlite::params![RequestState::SourceFinalized, now, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::SourceFinalized,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
            rusqlite::params![amount, direction.destination_reserve()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Confirming -> ManualReview`: the vault output backing this
    /// GlcToSol deposit was found already spent when re-checked at
    /// confirmation depth — anomalous, not a routine reorg (that path is
    /// `find_fork_point`/rollback, which returns the request to
    /// `AwaitingDeposit`, not here). This happens when a concurrent
    /// SolToGlc payout's coin selection picks the same vault UTXO before
    /// this GlcToSol deposit reaches `SourceFinalized`; prevention lives in
    /// `available_vault_utxos` (excluding UTXOs still backing a
    /// non-finalized GlcToSol deposit), but this is the required fail-
    /// closed backstop for any case that slips past it (e.g. a UTXO spent
    /// by something outside this service's own payout path). Never
    /// silently left in `Confirming` forever — the previous behavior was
    /// to warn and continue, permanently stranding the request and its
    /// reservation with no operator-visible terminal state. No
    /// `reserve_ledger` accounting changes here: the request was never
    /// `SourceFinalized`, so `pending_obligations` was never incremented
    /// for it — same as every other pre-finalization ManualReview
    /// transition in this module. Idempotent: a no-op if already
    /// `ManualReview`.
    pub fn mark_glc_deposit_spent_before_finalized(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<RequestState> = tx
            .query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = row else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };
        if state == RequestState::ManualReview {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::Confirming,
            "mark_glc_deposit_spent_before_finalized on unexpected state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = ?2 WHERE id = ?3",
            rusqlite::params![
                RequestState::ManualReview,
                "deposit_spent_before_finalized",
                request_id
            ],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::ManualReview,
            now,
            Some("deposit_spent_before_finalized"),
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Pre-finality reorg: the block carrying the deposit was orphaned.
    /// Releases the source-txid claim and returns the request to
    /// `AwaitingDeposit` so a future re-observation (same or different
    /// qualifying transaction) can bind cleanly — a documented
    /// simplification of docs/04-state-machines.md's "retry via Confirming
    /// if the tx still exists" vs "AwaitingDeposit if gone" distinction:
    /// this always retries via `AwaitingDeposit`, which is safe (the next
    /// indexer tick re-discovers the deposit if it is still valid, in
    /// whichever block it ends up mined in) at the cost of one extra
    /// confirmation cycle in the same-block-different-branch case.
    /// Reserved liquidity is NOT released (the reservation is still live,
    /// just waiting for a fresh confirmation) — only the source binding is
    /// cleared. Never callable once `SourceFinalized` (irreversible by
    /// policy; see docs/10-threat-model.md's post-finality-reorg section).
    pub fn mark_glc_reorged(&mut self, request_id: i64, now: i64) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: RequestState = tx
            .query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        assert!(
            matches!(
                state,
                RequestState::DepositObserved | RequestState::Confirming
            ),
            "mark_glc_reorged called post-finality or pre-observation ({state:?}) — caller bug; \
             post-finality reorg must never auto-revert (docs/10-threat-model.md)"
        );
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::Reorged,
            now,
            Some("block_orphaned"),
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = ?1, source_txid = NULL, source_vout = NULL,
                source_block_height = NULL, source_block_hash = NULL, source_confirmations = 0
             WHERE id = ?2",
            rusqlite::params![RequestState::AwaitingDeposit, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::Reorged),
            RequestState::AwaitingDeposit,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------------- Solana leg --

    /// `bridge_requests.manual_review_note` values [`Ledger::fold_sol_deposit`]
    /// can produce for a `SolToGlc` request — the exact, exhaustive set
    /// [`Ledger::resume_manual_review_sol_to_glc`]'s allowlist recognizes as
    /// structurally recoverable (every one of them means "capacity/gating
    /// was the only problem," never a data-integrity or fraud concern).
    /// Shared here so the two functions can never drift apart on the exact
    /// string values.
    const MANUAL_REVIEW_REASON_ADMISSION_CLOSED: &str = "admission_closed_at_fold";
    const MANUAL_REVIEW_REASON_PAUSED: &str = "reserve_paused_at_fold";
    const MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY: &str = "insufficient_capacity_at_fold";
    /// Distinct from `MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY`
    /// (accounting-figure exhaustion): this means the mature, unreserved
    /// vault UTXO POOL itself would run dangerously thin — the exact
    /// production incident this reason exists to prevent (many payouts,
    /// each consuming a mature UTXO and creating immature change, draining
    /// the pool faster than 6-confirmation maturity replenished it) — see
    /// `Ledger::set_utxo_pool_thresholds`/`Ledger::utxo_pool_health`.
    /// `pub(crate)`, not private: `Orchestrator`'s automatic-recovery phase
    /// (`tick_auto_resume_utxo_liquidity_backlog`) filters
    /// `ManualReview`-parked requests down to exactly this reason, and must
    /// read it from here rather than a duplicated string literal, so the
    /// two can never drift apart.
    pub(crate) const MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW: &str = "utxo_liquidity_low_at_fold";
    /// A `SolToGlc` recipient (Goldcoin L1 address) may receive at most one
    /// accepted bridge payout per rolling [`Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS`]
    /// window — see [`Ledger::fold_sol_deposit`]'s recipient-rate-limit
    /// check and [`Ledger::resume_manual_review_sol_to_glc`]'s unconditional
    /// re-check. `pub(crate)` for the same reason as
    /// `MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW`: `Orchestrator`'s
    /// automatic-recovery phase filters `ManualReview`-parked requests down
    /// to exactly this reason (in addition to the UTXO-liquidity one) and
    /// must read it from here, never a duplicated string literal.
    pub(crate) const MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED: &str = "recipient_rate_limited";
    /// A `SolToGlc` Solana source wallet (the on-chain
    /// `WithdrawalObligation.requester` — the deposit's actual signer, see
    /// `deposit_to_reserve.rs`'s `record.requester = ctx.accounts.user.key()`,
    /// never a client-provided string) may make at most one qualifying
    /// deposit per rolling [`Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS`]
    /// window — the SAME rule as [`Self::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED`],
    /// keyed by wallet instead of recipient, added ALONGSIDE it (never
    /// replacing it) to close the bypass where one wallet spreads deposits
    /// across many different Goldcoin recipients. See
    /// [`Ledger::fold_sol_deposit`]'s source-wallet-rate-limit check and
    /// [`Ledger::resume_manual_review_sol_to_glc`]'s unconditional
    /// re-check. `pub(crate)` for the same reason as the recipient one:
    /// `Orchestrator`'s automatic-recovery phase filters `ManualReview`-
    /// parked requests down to exactly this reason too, and must read it
    /// from here, never a duplicated string literal.
    pub(crate) const MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED: &str =
        "source_wallet_rate_limited";
    /// The rolling window backing both the per-recipient AND per-source-
    /// wallet SolToGlc rate limits above: "a Goldcoin L1 recipient address
    /// — or a Solana source wallet — may be party to at most one
    /// accepted/completed SolToGlc bridge payout in a rolling 24-hour
    /// window" (docs/09-runbook.md). 24 hours, in seconds. `pub` so the
    /// API layer can report the window itself (`GET /recipients/sol-to-glc/
    /// eligibility`'s `window_seconds`) from this one definition rather
    /// than a duplicated `86_400`. Deliberately the SAME constant for both
    /// limiters, not two separately-named ones — the task requires
    /// identical rolling-window semantics for each, so a single shared
    /// definition makes them structurally unable to drift apart.
    pub const RECIPIENT_RATE_LIMIT_WINDOW_SECS: i64 = 86_400;

    /// The single home of the recipient-rate-limit window query. Every
    /// consumer of the rule — [`Ledger::fold_sol_deposit`]'s admission
    /// check, [`Ledger::resume_manual_review_sol_to_glc`]'s unconditional
    /// re-check, and the read-only
    /// [`Ledger::sol_to_glc_recipient_rate_limited_until`] the public API
    /// serves — goes through here, so the three can never drift apart on
    /// the window, the state exclude-list, or the matching semantics
    /// (exact `recipient` byte equality, the same bytes the on-chain
    /// obligation carried and a payout would be built from).
    ///
    /// Returns the `MAX(created_at)` of the qualifying rows, i.e. the
    /// newest blocker; `None` means "not rate limited". The two SQL
    /// literals below differ ONLY by the strict-predecessor clause and are
    /// kept adjacent deliberately — see
    /// `resume_manual_review_sol_to_glc`'s comment for why a resume
    /// candidate may only ever be blocked by a row ordered strictly before
    /// it by `(created_at, id)`, while admission of a brand-new obligation
    /// considers every row in the window.
    fn recipient_rate_limit_blocker_created_at(
        conn: &rusqlite::Connection,
        recipient: &[u8],
        now: i64,
        strict_predecessor_of: Option<(i64, i64)>,
    ) -> Result<Option<i64>, LedgerError> {
        let window_start = now - Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS;
        let blocker = match strict_predecessor_of {
            None => conn.query_row(
                "SELECT MAX(created_at) FROM bridge_requests
                 WHERE direction = 'SolToGlc' AND recipient = ?1
                   AND created_at > ?2
                   AND state NOT IN ('Failed', 'DestinationSubmissionFailed',
                                      'InsufficientReserveAtSettlement', 'Cancelled',
                                      'Expired', 'Reorged')",
                rusqlite::params![recipient, window_start],
                |r| r.get(0),
            )?,
            Some((candidate_created_at, candidate_id)) => conn.query_row(
                "SELECT MAX(created_at) FROM bridge_requests
                 WHERE direction = 'SolToGlc' AND recipient = ?1
                   AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))
                   AND created_at > ?4
                   AND state NOT IN ('Failed', 'DestinationSubmissionFailed',
                                      'InsufficientReserveAtSettlement', 'Cancelled',
                                      'Expired', 'Reorged')",
                rusqlite::params![recipient, candidate_created_at, candidate_id, window_start],
                |r| r.get(0),
            )?,
        };
        Ok(blocker)
    }

    /// Read-only answer to "may this Goldcoin recipient be admitted for a
    /// NEW SolToGlc obligation right now?" — `Some(retry_after)` (the unix
    /// second the window reopens) when rate-limited, `None` when eligible.
    ///
    /// This is exactly the check [`Ledger::fold_sol_deposit`] will apply
    /// to the next arriving obligation for these bytes — same query, via
    /// [`Self::recipient_rate_limit_blocker_created_at`] — surfaced
    /// without any mutation so the API/UI can warn a user BEFORE they
    /// sign a Solana transaction that would only get parked in
    /// `ManualReview`. Purely advisory: admission itself still re-checks
    /// at fold time, so a stale answer here can never bypass the limit.
    pub fn sol_to_glc_recipient_rate_limited_until(
        &self,
        recipient: &[u8],
        now: i64,
    ) -> Result<Option<i64>, LedgerError> {
        let blocker =
            Self::recipient_rate_limit_blocker_created_at(&self.conn, recipient, now, None)?;
        Ok(blocker.map(|created_at| created_at + Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS))
    }

    /// The Solana-source-wallet twin of [`Self::recipient_rate_limit_blocker_created_at`]:
    /// identical query shape (same window, same state exclude-list, same
    /// strict-predecessor-only blocking rule for a resume candidate),
    /// matching on `requester` instead of `recipient`. Kept as a separate
    /// function rather than parameterizing the column name — a hardcoded
    /// column per query keeps both trivially auditable as exact mirrors of
    /// each other, and neither may ever silently drift onto the wrong
    /// column.
    fn source_wallet_rate_limit_blocker_created_at(
        conn: &rusqlite::Connection,
        requester: &[u8],
        now: i64,
        strict_predecessor_of: Option<(i64, i64)>,
    ) -> Result<Option<i64>, LedgerError> {
        let window_start = now - Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS;
        let blocker = match strict_predecessor_of {
            None => conn.query_row(
                "SELECT MAX(created_at) FROM bridge_requests
                 WHERE direction = 'SolToGlc' AND requester = ?1
                   AND created_at > ?2
                   AND state NOT IN ('Failed', 'DestinationSubmissionFailed',
                                      'InsufficientReserveAtSettlement', 'Cancelled',
                                      'Expired', 'Reorged')",
                rusqlite::params![requester, window_start],
                |r| r.get(0),
            )?,
            Some((candidate_created_at, candidate_id)) => conn.query_row(
                "SELECT MAX(created_at) FROM bridge_requests
                 WHERE direction = 'SolToGlc' AND requester = ?1
                   AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))
                   AND created_at > ?4
                   AND state NOT IN ('Failed', 'DestinationSubmissionFailed',
                                      'InsufficientReserveAtSettlement', 'Cancelled',
                                      'Expired', 'Reorged')",
                rusqlite::params![requester, candidate_created_at, candidate_id, window_start],
                |r| r.get(0),
            )?,
        };
        Ok(blocker)
    }

    /// The Solana-source-wallet twin of
    /// [`Self::sol_to_glc_recipient_rate_limited_until`] — same read-only,
    /// purely advisory contract, keyed by the depositor's wallet
    /// (`requester`) instead of the Goldcoin recipient.
    pub fn sol_to_glc_source_wallet_rate_limited_until(
        &self,
        requester: &[u8],
        now: i64,
    ) -> Result<Option<i64>, LedgerError> {
        let blocker =
            Self::source_wallet_rate_limit_blocker_created_at(&self.conn, requester, now, None)?;
        Ok(blocker.map(|created_at| created_at + Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS))
    }

    /// Folds an observed Solana `WithdrawalObligation` (a `deposit_to_reserve`
    /// execution, seen at `finalized` commitment) into the ledger. See the
    /// module docs and [`SolFoldOutcome`] for why this direction has no
    /// pre-existing-reservation match and instead reserves/commits capacity
    /// retroactively, and why it folds directly to `SourceFinalized` (Solana
    /// finality is a single instant, unlike Goldcoin's depth ramp).
    /// Idempotent on `source_obligation_index`. `amounts` must already be a
    /// fully computed gross/fee/net breakdown for this obligation's raw
    /// on-chain amount (docs/20-bridge-fee.md — see [`Ledger::create_request`]'s
    /// matching doc comment). The capacity check compares
    /// `amounts.net_destination_atomic` (Goldcoin-native — the destination
    /// for this direction) against available `GoldcoinReserve` capacity,
    /// NOT the raw gross Solana amount that was deposited.
    pub fn fold_sol_deposit(
        &mut self,
        obligation_index: u64,
        amounts: RequestAmounts,
        requester: [u8; 32],
        recipient_glc_address: &[u8],
        now: i64,
    ) -> Result<SolFoldOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;

        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM bridge_requests WHERE source_obligation_index = ?1",
                [obligation_index as i64],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.rollback()?;
            return Ok(SolFoldOutcome::AlreadyFolded { request_id: id });
        }

        let reserve = ReserveDirection::GoldcoinReserve;
        let (paused, admission_closed, min_available_utxo_count): (i64, i64, i64) = tx.query_row(
            "SELECT paused, admission_closed, utxo_pool_min_available_count
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
            "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let available = balance - protected_minimum - reserved;
        // The live, mature, unreserved UTXO count — the same candidate
        // pool `available_vault_utxos` offers coin selection, counted
        // rather than fetched in full. A leading indicator distinct from
        // `available` (an accounting figure): the accounting can look
        // perfectly healthy while the POOL itself is a single oversized
        // UTXO or a handful of exhausted ones, exactly the shape of the
        // real incident `utxo_pool_min_available_count` guards against.
        let available_utxo_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND NOT EXISTS (
                 SELECT 1 FROM bridge_requests b
                 WHERE b.direction = 'GlcToSol'
                   AND b.source_txid = v.txid
                   AND b.source_vout = v.vout
                   AND b.state IN ('DepositObserved', 'Confirming')
               )",
            [],
            |r| r.get(0),
        )?;
        // `min_available_utxo_count == 0` (the default until an operator
        // explicitly configures it, and every reserve other than
        // GoldcoinReserve) means "backpressure disabled" — never requires
        // even one physical UTXO to exist, so accounting-only capacity
        // tests/reserves that never touch `vault_utxos` at all are
        // unaffected. A nonzero floor requires STRICTLY MORE than that many
        // to remain available.
        let utxo_liquidity_ok =
            min_available_utxo_count == 0 || available_utxo_count > min_available_utxo_count;
        // "A Goldcoin L1 recipient address may receive at most one
        // accepted/completed SolToGlc bridge payout in a rolling 24-hour
        // window" (docs/09-runbook.md). Any row for this recipient created
        // inside the window counts UNLESS it's a terminal state that never
        // produced (and now never will produce) a real payout — an
        // exclude-list, not an include-list, so a future state addition
        // defaults to counting (the safe direction) rather than silently
        // being ignored. `Failed`/`DestinationSubmissionFailed`/
        // `InsufficientReserveAtSettlement` are defined but never set
        // anywhere in this codebase today, and `Cancelled`/`Expired`/
        // `Reorged` are structurally unreachable for SolToGlc — excluded
        // here anyway, defensively, since they clearly represent "no
        // payout resulted." (Exclude-list and window live in
        // `recipient_rate_limit_blocker_created_at`, shared with the
        // resume re-check and the API's read-only eligibility view.)
        let recipient_rate_limited =
            Self::recipient_rate_limit_blocker_created_at(&tx, recipient_glc_address, now, None)?
                .is_some();
        // The Solana-source-wallet twin of the check just above: "a Solana
        // source wallet may make at most one qualifying SolToGlc deposit in
        // a rolling 24-hour window" — independent of, and enforced
        // ALONGSIDE, the per-recipient rule (never replacing it), closing
        // the bypass where a single wallet spreads deposits across many
        // different Goldcoin recipients to evade the recipient-only limit.
        // Same window, same state exclude-list, same matching semantics —
        // `requester` is decoded straight from the on-chain
        // `WithdrawalObligation` account by `solana::indexer`
        // (`WithdrawalObligationSnapshot.requester`, itself
        // `record.requester = ctx.accounts.user.key()` set by the program
        // from the deposit's own `Signer`), never a client-supplied string.
        let source_wallet_rate_limited = Self::source_wallet_rate_limit_blocker_created_at(
            &tx,
            requester.as_slice(),
            now,
            None,
        )?
        .is_some();
        // Admission is a separate axis from `paused` (docs/09-runbook.md's
        // "Admission control (Solana->Goldcoin)" section): EITHER gate
        // blocks a new obligation from being admitted — an operator who
        // has explicitly closed admission gets that respected even if
        // `paused` is (or later becomes) clear, and the pre-existing
        // `paused` gate keeps working exactly as before either way. The
        // UTXO-liquidity check is the same shape: it never touches
        // `paused`/`admission_closed`/the accounting-capacity check, and
        // never affects a request that already made it past this gate.
        let capacity_ok = paused == 0
            && admission_closed == 0
            && utxo_liquidity_ok
            && !recipient_rate_limited
            && !source_wallet_rate_limited
            && (amounts.net_destination_atomic as i64) <= available;
        let manual_review_reason = if admission_closed != 0 {
            Self::MANUAL_REVIEW_REASON_ADMISSION_CLOSED
        } else if paused != 0 {
            Self::MANUAL_REVIEW_REASON_PAUSED
        } else if source_wallet_rate_limited {
            Self::MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED
        } else if recipient_rate_limited {
            Self::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED
        } else if !utxo_liquidity_ok {
            Self::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW
        } else {
            Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY
        };

        tx.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 reserved_at, source_obligation_index, source_confirmations, source_finalized_at,
                 manual_review_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, 1, ?10, ?12)",
            rusqlite::params![
                Direction::SolToGlc,
                if capacity_ok {
                    RequestState::SourceFinalized
                } else {
                    RequestState::ManualReview
                },
                amounts.gross_atomic as i64,
                amounts.fee_bps as i64,
                amounts.fee_atomic as i64,
                amounts.net_atomic as i64,
                amounts.net_destination_atomic as i64,
                recipient_glc_address,
                requester.as_slice(),
                now,
                obligation_index as i64,
                if capacity_ok {
                    None
                } else {
                    Some(manual_review_reason)
                },
            ],
        )?;
        let request_id = tx.last_insert_rowid();
        log_transition(
            &tx,
            request_id,
            None,
            if capacity_ok {
                RequestState::SourceFinalized
            } else {
                RequestState::ManualReview
            },
            now,
            Some("retroactive_fold_sol_deposit"),
            "system",
        )?;

        if capacity_ok {
            tx.execute(
                "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                    pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
                rusqlite::params![amounts.net_destination_atomic as i64, reserve],
            )?;
        }
        tx.commit()?;

        Ok(if capacity_ok {
            SolFoldOutcome::FoldedFinalized { request_id }
        } else {
            SolFoldOutcome::FoldedManualReview { request_id }
        })
    }

    /// Resumes a `SolToGlc` request `fold_sol_deposit` parked in
    /// `ManualReview` purely because admission was closed, the reserve was
    /// paused, or capacity was insufficient at that exact moment — never a
    /// request in `ManualReview` for any other reason. Applies the SAME
    /// `reserved_liquidity`/`pending_obligations` increment a successful
    /// fold would have applied, refusing (no override) if that increment
    /// would breach the reserve invariant right now — the identical
    /// `available_capacity` check `fold_sol_deposit`/`create_request`
    /// already use. Deliberately does NOT consult `paused`/
    /// `admission_closed` at all: admission may remain closed while this
    /// resumes an already-accepted obligation (docs/09-runbook.md's
    /// "Admission control (Solana->Goldcoin)" section — this command never
    /// admits anything new, it only unblocks something already accepted).
    ///
    /// Idempotent: calling this again once the request has already moved
    /// past `ManualReview` (by a prior call to this same command) is a
    /// safe no-op reporting [`ResumeManualReviewOutcome::AlreadyResumed`],
    /// never a second reservation. Never creates a new row, never touches
    /// `source_obligation_index` — this transitions the EXISTING request
    /// in place, so a duplicate obligation is impossible by construction,
    /// not just by convention. The idempotency check itself does not
    /// filter by `actor` (see the query below) — the `(ManualReview ->
    /// SourceFinalized)` transition is only ever written here, by any
    /// caller, so its mere presence is unambiguous proof of a prior
    /// resume regardless of which actor performed it.
    ///
    /// `actor` is recorded verbatim in `bridge_request_state_log` — pass
    /// `"operator"` for a human-initiated `glc-admin resume-manual-review`
    /// call, or `"auto-resume"` for `Orchestrator::
    /// tick_auto_resume_utxo_liquidity_backlog`'s automatic recovery.
    /// Every other safety check below is identical regardless of `actor`;
    /// this parameter affects only the audit trail, never eligibility.
    pub fn resume_manual_review_sol_to_glc(
        &mut self,
        request_id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<ResumeManualReviewOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;

        #[allow(clippy::type_complexity)]
        let row: Option<(
            Direction,
            RequestState,
            Option<String>,
            i64,
            Option<i64>,
            Option<Vec<u8>>,
            Vec<u8>,
            i64,
            Option<Vec<u8>>,
        )> = tx
            .query_row(
                "SELECT direction, state, manual_review_note, net_destination_atomic,
                        source_finalized_at, destination_txid, recipient, created_at, requester
                 FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            direction,
            state,
            manual_review_note,
            net_destination_atomic,
            source_finalized_at,
            destination_txid,
            recipient,
            candidate_created_at,
            requester,
        )) = row
        else {
            tx.rollback()?;
            return Err(LedgerError::RequestNotFound(request_id));
        };

        if direction != Direction::SolToGlc {
            tx.rollback()?;
            return Err(LedgerError::NotASolToGlcRequest {
                id: request_id,
                actual_direction: direction,
            });
        }
        // `fold_sol_deposit` always records `requester` for a SolToGlc row
        // (it's a required, non-`Option` parameter there); `NULL` here
        // would mean this row was never folded through that path, which
        // cannot happen for a `SolToGlc`-direction request. Defensive only.
        let Some(requester) = requester else {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "SolToGlc request has no requester recorded".to_string(),
            });
        };

        if state != RequestState::ManualReview {
            // Distinguishes a genuine repeat call (this exact command
            // already resumed this request, whether by an operator or by
            // automatic recovery) from a request that reached
            // SourceFinalized some other way (e.g. a normal fold) and was
            // never in ManualReview to begin with — the latter must still
            // be refused, not reported as a harmless no-op. No `actor`
            // filter: this exact (from = ManualReview, to =
            // SourceFinalized) transition is written ONLY by this
            // function (verified: no other call site in this file logs
            // it), so its mere presence — regardless of which actor
            // performed it — is unambiguous proof of a prior resume.
            let previously_resumed: bool = tx
                .query_row(
                    "SELECT 1 FROM bridge_request_state_log
                     WHERE request_id = ?1 AND from_state = ?2 AND to_state = ?3 LIMIT 1",
                    rusqlite::params![
                        request_id,
                        RequestState::ManualReview,
                        RequestState::SourceFinalized
                    ],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            tx.rollback()?;
            if previously_resumed {
                return Ok(ResumeManualReviewOutcome::AlreadyResumed { state });
            }
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!("state is {state:?}, not ManualReview"),
            });
        }

        let is_known_recoverable_reason = matches!(
            manual_review_note.as_deref(),
            Some(Self::MANUAL_REVIEW_REASON_ADMISSION_CLOSED)
                | Some(Self::MANUAL_REVIEW_REASON_PAUSED)
                | Some(Self::MANUAL_REVIEW_REASON_INSUFFICIENT_CAPACITY)
                | Some(Self::MANUAL_REVIEW_REASON_UTXO_LIQUIDITY_LOW)
                | Some(Self::MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED)
                | Some(Self::MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED)
        );
        if !is_known_recoverable_reason {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: format!(
                    "manual_review_note {manual_review_note:?} is not a known recoverable reason"
                ),
            });
        }

        if source_finalized_at.is_none() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "source deposit is not finalized".to_string(),
            });
        }

        if destination_txid.is_some() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a destination transaction already exists".to_string(),
            });
        }

        let existing_payout: Option<i64> = tx
            .query_row(
                "SELECT request_id FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if existing_payout.is_some() {
            tx.rollback()?;
            return Err(LedgerError::ManualReviewNotRecoverable {
                id: request_id,
                detail: "a Goldcoin payout already exists for this request".to_string(),
            });
        }

        // Checked UNCONDITIONALLY, regardless of the request's original
        // `manual_review_note` — this is what makes "manual operator
        // resume must not bypass the 24-hour window" true even for a
        // request that was never parked for this reason in the first
        // place. Same exclude-list as `fold_sol_deposit`'s check, so the
        // two can never drift apart on which states count.
        //
        // Only a STRICT PREDECESSOR — an earlier row, ordered by
        // `(created_at, id)` — may ever count as a blocker here. Without
        // this ordering restriction, a later-arriving sibling row to the
        // same recipient (itself still parked, since it necessarily
        // arrived after this one and so was itself rate-limited) would
        // shadow-block this earlier, rightfully-next-in-line candidate —
        // inverting oldest-first draining, and in the worst case letting a
        // steady trickle of new same-recipient arrivals starve the oldest
        // parked request indefinitely. Restricting to `(created_at, id) <
        // (candidate's own)` makes that structurally impossible: this
        // candidate's eligibility can only ever depend on rows that
        // already existed before it did, never on ones that showed up
        // later. `(created_at, id)` rather than `created_at` alone breaks
        // ties deterministically when two rows share the same `created_at`
        // (insertion/id order is itself a legitimate secondary ordering,
        // since ids are assigned in strict creation order).
        // The Solana-source-wallet twin of the recipient check just below —
        // same UNCONDITIONAL re-check (regardless of this request's own
        // `manual_review_note`), same strict-predecessor-only blocking
        // rule, same self-clearing shape. Checked first so a wallet that is
        // itself still rate-limited is reported ahead of a recipient
        // finding, matching the eligibility API's precedence, though a
        // resume attempt is refused either way if EITHER independent limit
        // still applies.
        let source_wallet_rate_limited_until = Self::source_wallet_rate_limit_blocker_created_at(
            &tx,
            &requester,
            now,
            Some((candidate_created_at, request_id)),
        )?;
        if let Some(blocking_created_at) = source_wallet_rate_limited_until {
            tx.rollback()?;
            return Err(LedgerError::SourceWalletRateLimited {
                request_id,
                requester,
                retry_after: blocking_created_at + Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS,
            });
        }

        let recipient_rate_limited_until = Self::recipient_rate_limit_blocker_created_at(
            &tx,
            &recipient,
            now,
            Some((candidate_created_at, request_id)),
        )?;
        if let Some(blocking_created_at) = recipient_rate_limited_until {
            tx.rollback()?;
            return Err(LedgerError::RecipientRateLimited {
                request_id,
                recipient,
                retry_after: blocking_created_at + Self::RECIPIENT_RATE_LIMIT_WINDOW_SECS,
            });
        }

        let reserve = ReserveDirection::GoldcoinReserve;
        let (balance, protected_minimum, reserved, min_available_utxo_count): (i64, i64, i64, i64) =
            tx.query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity,
                    utxo_pool_min_available_count
             FROM reserve_ledger WHERE direction = ?1",
                [reserve],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;

        // The SAME count-based admission gate `fold_sol_deposit` applies to
        // a brand-new obligation (docs/09-runbook.md's "UTXO liquidity"
        // section), applied here to something already accepted: resuming a
        // parked request re-admits real demand on the mature UTXO pool
        // exactly as a fresh fold would, so it must never bypass the same
        // floor just because the request was accepted once before. `== 0`
        // means backpressure is disabled — identical short-circuit to
        // `fold_sol_deposit`'s own.
        let available_utxo_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND NOT EXISTS (
                 SELECT 1 FROM bridge_requests b
                 WHERE b.direction = 'GlcToSol'
                   AND b.source_txid = v.txid
                   AND b.source_vout = v.vout
                   AND b.state IN ('DepositObserved', 'Confirming')
               )",
            [],
            |r| r.get(0),
        )?;
        let utxo_liquidity_ok =
            min_available_utxo_count == 0 || available_utxo_count > min_available_utxo_count;
        if !utxo_liquidity_ok {
            tx.rollback()?;
            return Err(LedgerError::UtxoLiquidityLow {
                request_id,
                available_utxo_count,
                min_available_count: min_available_utxo_count,
            });
        }

        let available = balance - protected_minimum - reserved;
        // Equivalent to requiring the reserve invariant still hold AFTER
        // this reservation is applied (balance >= protected_minimum +
        // reserved + net_destination_atomic) — the same check
        // `create_request`/`fold_sol_deposit` already use to admit
        // anything new, applied here to something already accepted.
        if net_destination_atomic > available {
            tx.rollback()?;
            return Err(LedgerError::InvariantViolated {
                direction: reserve,
                balance,
                protected_minimum,
                reserved_liquidity: reserved + net_destination_atomic,
            });
        }

        tx.execute(
            "UPDATE bridge_requests SET state = ?1, manual_review_note = NULL WHERE id = ?2",
            rusqlite::params![RequestState::SourceFinalized, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::ManualReview),
            RequestState::SourceFinalized,
            now,
            Some(note),
            actor,
        )?;
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + ?1,
                pending_obligations = pending_obligations + ?1 WHERE direction = ?2",
            rusqlite::params![net_destination_atomic, reserve],
        )?;
        tx.commit()?;
        Ok(ResumeManualReviewOutcome::Resumed)
    }

    pub fn last_synced_obligation_count(&self) -> Result<u64, LedgerError> {
        let n: i64 = self
            .conn
            .query_row(
                "SELECT last_obligation_count FROM solana_indexer_state WHERE id = 0",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n as u64)
    }

    pub fn set_last_synced_obligation_count(
        &mut self,
        count: u64,
        slot: u64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO solana_indexer_state (id, last_obligation_count, last_checked_slot, updated_at)
             VALUES (0, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET last_obligation_count = ?1, last_checked_slot = ?2, updated_at = ?3",
            rusqlite::params![count as i64, slot as i64, now],
        )?;
        Ok(())
    }

    /// `(total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations)`
    /// — used by the reconciliation engine.
    pub fn reserve_snapshot(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u64, u64, u64, u64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations
                 FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64, r.get::<_, i64>(2)? as u64, r.get::<_, i64>(3)? as u64)),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => LedgerError::ReserveNotInitialized(direction),
                other => LedgerError::Sqlite(other),
            })
    }

    /// Sum of `net_destination_atomic` across every request whose
    /// settlement has been broadcast to `direction`'s chain
    /// (`DestinationSubmitted`/`DestinationConfirmed`) but not yet marked
    /// `Settled` in this ledger — the real, currently-pending amount that
    /// can legitimately explain an observed balance drop at the exact
    /// instant a reconciliation tick runs before this service's own
    /// indexer has caught up (docs/24-load-soak-harness.md's documented
    /// `InFlightExplained` gap; reconciliation module docs).
    ///
    /// For `GoldcoinReserve` specifically, also adds the FULL input value
    /// (`payout_atomic + change_atomic + fee_atomic`, not just the net
    /// payout amount) of every `goldcoin_payouts` row still in
    /// `Broadcast` state — a UTXO-based-chain-specific effect with no
    /// SolanaReserve equivalent: spending the vault's UTXO to fund a
    /// payout makes that UTXO's *entire* value (the paid-out portion AND
    /// its change) temporarily invisible to a confirmed-only balance read
    /// until the transaction itself confirms and the change output
    /// matures, even though none of that value has actually left the
    /// vault's control yet (the change returns to it). Only the net
    /// payout amount would under-explain this drop.
    ///
    /// Used only to CAP how much of an already-observed drop
    /// reconciliation treats as explained — it never manufactures
    /// headroom, and the hard solvency invariant in `reconcile` is
    /// checked against the real observed balance independent of this
    /// figure.
    pub fn pending_destination_settlement_amount(
        &self,
        direction: ReserveDirection,
    ) -> Result<u64, LedgerError> {
        let bridge_direction = match direction {
            ReserveDirection::SolanaReserve => Direction::GlcToSol,
            ReserveDirection::GoldcoinReserve => Direction::SolToGlc,
        };
        let settlement_amount: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(net_destination_atomic), 0) FROM bridge_requests
             WHERE direction = ?1 AND state IN ('DestinationSubmitted', 'DestinationConfirmed')",
            [bridge_direction],
            |r| r.get(0),
        )?;
        let mut total = settlement_amount as u64;
        if direction == ReserveDirection::GoldcoinReserve {
            let broadcast_payout_value: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(payout_atomic + change_atomic + fee_atomic), 0)
                 FROM goldcoin_payouts WHERE state = 'Broadcast'",
                [],
                |r| r.get(0),
            )?;
            total = total.saturating_add(broadcast_payout_value as u64);
            // Additional, live-state-grounded coverage for change fan-out
            // (docs/09-runbook.md's "UTXO liquidity" section): the term
            // above only covers a payout while its OWN lifecycle state is
            // still `Broadcast` — once it reaches `Confirmed` (its own tx
            // has enough confirmations) but its change output(s) haven't
            // yet independently reached `vault_min_confirmations` maturity
            // in `vault_utxos` (a separate, differently-configured
            // threshold), that term stops counting it even though the
            // change is still genuinely immature. `own_unconfirmed_change_
            // atomic` closes that gap by checking the PHYSICAL UTXO state
            // directly rather than the payout's lifecycle state, so it is
            // never stale and never double-counts a change output that
            // has already matured (which the `Broadcast`-only term above
            // also cannot do, since maturity always implies the amount is
            // already reflected in a fresh `observed_balance`). The two
            // terms overlap (both can cover the same change amount while a
            // payout is genuinely `Broadcast` and still immature) — purely
            // additive over-explanation, capped by `raw_drop` at the call
            // site, never a weakening of the hard invariant or of
            // `unexplained_drop` detection for any amount beyond what is
            // genuinely, currently known to be this service's own in-flight
            // change.
            total = total.saturating_add(self.own_unconfirmed_change_atomic()?);
            // The real Goldcoin network fee genuinely, permanently leaves
            // the vault the instant a payout broadcasts — unlike the
            // payout (covered by `net_destination_atomic` above, for as
            // long as `bridge_requests.state` remains DestinationSubmitted/
            // DestinationConfirmed) and the change (covered by
            // `own_unconfirmed_change_atomic`), there is no OTHER term
            // that ever explains it once the payout moves past
            // `Broadcast`. Narrow and small in practice (one real network
            // fee), but a genuine drop that must still be accounted for at
            // `reconciliation_tolerance = 0` if a reconciliation catch-up
            // happens to land after the payout has already confirmed.
            let confirmed_fee_value: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(fee_atomic), 0) FROM goldcoin_payouts WHERE state = 'Confirmed'",
                [],
                |r| r.get(0),
            )?;
            total = total.saturating_add(confirmed_fee_value as u64);
            // A vault UTXO split's network fee is the same kind of genuine,
            // permanent departure as a payout's fee above: the source's
            // full value disappears from a confirmed-only balance read the
            // instant the split broadcasts, its chunk outputs are covered
            // by `own_unconfirmed_change_atomic` (they are inserted as
            // `Unconfirmed` rows atomically at broadcast — see
            // `record_vault_utxo_split_broadcast_effects` — and matured
            // chunks are already back in `observed_balance`), leaving
            // exactly the fee with no other term to explain it.
            let split_fee_value: i64 = self.conn.query_row(
                "SELECT COALESCE(SUM(fee_atomic), 0) FROM vault_utxo_splits WHERE state IN ('Broadcast','Confirmed')",
                [],
                |r| r.get(0),
            )?;
            total = total.saturating_add(split_fee_value as u64);
        }
        Ok(total)
    }

    /// `(total_reserve_balance, protected_minimum, target_reserve,
    /// warning_reserve, critical_reserve)` — the full threshold-band
    /// configuration for `direction` (docs/09-runbook.md "Threshold bands
    /// and responses"), used by [`crate::rebalance::assess`] to classify
    /// imbalance severity and suggest a rebalance amount from values the
    /// operator already configured, never an invented one.
    pub fn reserve_thresholds(
        &self,
        direction: ReserveDirection,
    ) -> Result<(u64, u64, u64, u64, u64), LedgerError> {
        self.conn
            .query_row(
                "SELECT total_reserve_balance, protected_minimum, target_reserve, \
                 warning_reserve, critical_reserve FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)? as u64,
                        r.get::<_, i64>(1)? as u64,
                        r.get::<_, i64>(2)? as u64,
                        r.get::<_, i64>(3)? as u64,
                        r.get::<_, i64>(4)? as u64,
                    ))
                },
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    /// Cumulative fee revenue recognized (at settlement, not reservation)
    /// on `direction`'s row — ALWAYS canonical units (`amount_conversion::
    /// CanonicalAtomic`) regardless of which reserve the row belongs to, a
    /// deliberate exception from that row's other native-unit columns
    /// (docs/20-bridge-fee.md). Reporting/audit only: never subtracted
    /// from [`Ledger::available_capacity`] — see that function's doc
    /// comment for why no such subtraction is needed.
    pub fn accrued_fees(&self, direction: ReserveDirection) -> Result<u64, LedgerError> {
        self.conn
            .query_row(
                "SELECT accrued_fees_atomic FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_reconciliation_finding(
        &mut self,
        direction: ReserveDirection,
        expected: i64,
        observed: i64,
        delta: i64,
        classification: &str,
        auto_paused: bool,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO reconciliation_findings
                (detected_at, direction, expected, observed, delta, classification, auto_paused)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                now,
                direction.as_str(),
                expected,
                observed,
                delta,
                classification,
                auto_paused as i64
            ],
        )?;
        Ok(())
    }

    /// One page of `reconciliation_findings`, newest first — the real,
    /// already-persisted per-tick balance history behind the public
    /// `/reserves/history` read-projection (`api::BridgeApi::
    /// reserves_history`). Never fabricates a data point: every row here
    /// is a reconciliation tick that actually ran (including `SKIPPED`
    /// ones, whose `classification` says so explicitly rather than the
    /// gap being silently absent). `id` is the append-only, strictly
    /// monotonic pagination cursor — safe even when two rows share the
    /// same `detected_at` second.
    pub fn reconciliation_findings_page(
        &self,
        direction: Option<ReserveDirection>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ReconciliationFindingRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, direction, detected_at, expected, observed, delta, classification, \
             auto_paused FROM reconciliation_findings \
             WHERE (?1 IS NULL OR direction = ?1) AND (?2 IS NULL OR id < ?2) \
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![direction, before_id, limit as i64], |r| {
                Ok(ReconciliationFindingRow {
                    id: r.get(0)?,
                    direction: r.get(1)?,
                    detected_at: r.get(2)?,
                    expected: r.get(3)?,
                    observed: r.get(4)?,
                    delta: r.get(5)?,
                    classification: r.get(6)?,
                    auto_paused: r.get::<_, i64>(7)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One page of `bridge_request_state_log`, newest first, joined to
    /// each row's own `direction` — the real, already-persisted
    /// user-facing settlement lifecycle behind the public
    /// `/explorer/events` read-projection (`api::BridgeApi::
    /// explorer_events`). Deliberately scoped to `bridge_request_state_log`
    /// only: `rebalance_state_log`/`custody_transition_state_log` carry
    /// real operator identities (an approver's name) and internal
    /// tx_reference values, which have no place on a public feed — that
    /// audit trail stays operator-only via `glc-admin rebalance-list`/
    /// `custody-list`. `id` is the strictly monotonic pagination cursor.
    pub fn explorer_events_page(
        &self,
        direction: Option<Direction>,
        to_state: Option<RequestState>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<ExplorerEventRow>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT srl.id, srl.request_id, br.direction, srl.from_state, srl.to_state, \
             srl.at, srl.reason \
             FROM bridge_request_state_log srl \
             JOIN bridge_requests br ON br.id = srl.request_id \
             WHERE (?1 IS NULL OR br.direction = ?1) \
               AND (?2 IS NULL OR srl.to_state = ?2) \
               AND (?3 IS NULL OR srl.id < ?3) \
             ORDER BY srl.id DESC LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![direction, to_state, before_id, limit as i64],
                |r| {
                    let from_state: Option<String> = r.get(3)?;
                    let to_state: String = r.get(4)?;
                    Ok(ExplorerEventRow {
                        id: r.get(0)?,
                        request_id: r.get(1)?,
                        direction: r.get(2)?,
                        from_state: from_state.map(|s| s.parse().unwrap()),
                        to_state: to_state.parse().unwrap(),
                        at: r.get(5)?,
                        reason: r.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ---------------------------------------------------- Goldcoin chain tracking --

    /// The locally indexed tip, if any.
    pub fn goldcoin_chain_tip(&self) -> Result<Option<(i64, [u8; 32])>, LedgerError> {
        self.conn
            .query_row(
                "SELECT height, hash FROM goldcoin_indexed_blocks ORDER BY height DESC LIMIT 1",
                [],
                |r| {
                    let h: Vec<u8> = r.get(1)?;
                    Ok((r.get::<_, i64>(0)?, to_array32(&h)))
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    pub fn goldcoin_block_hash_at(&self, height: i64) -> Result<Option<[u8; 32]>, LedgerError> {
        self.conn
            .query_row(
                "SELECT hash FROM goldcoin_indexed_blocks WHERE height = ?1",
                [height],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map(|o| o.map(|v| to_array32(&v)))
            .map_err(LedgerError::from)
    }

    pub fn goldcoin_ingest_block(
        &mut self,
        height: i64,
        hash: [u8; 32],
        prev_hash: [u8; 32],
        block_time: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO goldcoin_indexed_blocks (height, hash, prev_hash, block_time, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(height) DO UPDATE SET hash = excluded.hash, prev_hash = excluded.prev_hash,
                block_time = excluded.block_time, indexed_at = excluded.indexed_at",
            rusqlite::params![
                height,
                hash.as_slice(),
                prev_hash.as_slice(),
                block_time,
                now
            ],
        )?;
        Ok(())
    }

    /// Rolls back locally indexed blocks above `fork_height`, records a
    /// reorg event, and reorgs (via [`Ledger::mark_glc_reorged`]) every
    /// active `GlcToSol` request whose source block was orphaned.
    /// `SourceFinalized`-or-later requests are never touched here — a
    /// post-finality reorg is a distinct, non-automatic incident (see
    /// `mark_glc_reorged`'s panic guard and docs/10-threat-model.md).
    pub fn goldcoin_rollback_reorg(
        &mut self,
        fork_height: i64,
        fork_hash: [u8; 32],
        old_tip_height: i64,
        old_tip_hash: [u8; 32],
        now: i64,
    ) -> Result<i64, LedgerError> {
        let tx = write_tx(&mut self.conn)?;

        let affected: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM bridge_requests
                 WHERE direction = 'GlcToSol' AND state IN ('DepositObserved','Confirming')
                   AND source_block_height > ?1",
            )?;
            let rows: Result<Vec<i64>, _> = stmt.query_map([fork_height], |r| r.get(0))?.collect();
            rows?
        };
        for id in &affected {
            let state: RequestState = tx.query_row(
                "SELECT state FROM bridge_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
                 VALUES (?1, ?2, 'Reorged', ?3, 'block_orphaned', 'system')",
                rusqlite::params![id, state.as_str(), now],
            )?;
            tx.execute(
                "UPDATE bridge_requests SET state = 'AwaitingDeposit', source_txid = NULL,
                    source_vout = NULL, source_block_height = NULL, source_block_hash = NULL,
                    source_confirmations = 0 WHERE id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
                 VALUES (?1, 'Reorged', 'AwaitingDeposit', ?2, NULL, 'system')",
                rusqlite::params![id, now],
            )?;
        }

        tx.execute(
            "DELETE FROM goldcoin_indexed_blocks WHERE height > ?1",
            [fork_height],
        )?;
        tx.execute(
            "INSERT INTO goldcoin_reorg_events
                (detected_at, fork_height, old_tip_height, old_tip_hash, new_tip_height, new_tip_hash, orphaned_count)
             VALUES (?1, ?2, ?3, ?4, ?2, ?5, ?6)",
            rusqlite::params![now, fork_height, old_tip_height, old_tip_hash.as_slice(), fork_hash.as_slice(), affected.len() as i64],
        )?;
        tx.commit()?;
        Ok(affected.len() as i64)
    }

    /// Read-only check: which `GlcToSol` requests, already told their
    /// deposit was final (`source_finalized_at IS NOT NULL`), had their
    /// source block above `fork_height` — i.e. would be orphaned by
    /// rolling back to `fork_height` (docs/22-production-readiness-
    /// review.md P1 "dedicated post-finality reorg protection",
    /// docs/10-threat-model.md). Callers (`goldcoin::indexer::Indexer::
    /// tick`) run this BEFORE [`Ledger::goldcoin_rollback_reorg`], which
    /// deliberately only ever touches pre-finality requests — a non-empty
    /// result here means the reorg about to be rolled back is not routine
    /// and must be handled via [`Ledger::record_post_finality_reorg`]
    /// instead of (not in addition to) the normal rollback path.
    pub fn detect_post_finality_reorg(&self, fork_height: i64) -> Result<Vec<i64>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM bridge_requests
             WHERE direction = 'GlcToSol' AND source_finalized_at IS NOT NULL
               AND source_block_height > ?1",
        )?;
        let rows: Result<Vec<i64>, _> = stmt.query_map([fork_height], |r| r.get(0))?.collect();
        Ok(rows?)
    }

    /// Records the dedicated post-finality-reorg audit event and, per
    /// docs/10-threat-model.md ("should be treated as an automatic
    /// global-pause trigger... never classified as WITHIN_TOLERANCE"),
    /// pauses BOTH reserve directions — not just the Goldcoin one — since
    /// a previously-final Goldcoin observation turning out reversible
    /// undermines confidence in the ledger's Goldcoin-side bookkeeping
    /// that both bridge directions ultimately rely on. Like every other
    /// pause in this codebase, never auto-cleared; an operator must
    /// explicitly `set_paused(.., false, ..)` (`glc-admin unpause`/
    /// `onchain-unpause`) after investigating.
    pub fn record_post_finality_reorg(
        &mut self,
        fork_height: i64,
        old_tip_height: i64,
        affected_request_ids: &[i64],
        now: i64,
    ) -> Result<i64, LedgerError> {
        let ids_json =
            serde_json::to_string(affected_request_ids).expect("Vec<i64> always serializes");
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "INSERT INTO post_finality_reorg_events
                (detected_at, fork_height, old_tip_height, affected_request_ids, auto_paused)
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![now, fork_height, old_tip_height, ids_json],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;

        let reason = format!(
            "post-finality Goldcoin reorg detected: fork_height={fork_height} (old tip \
             {old_tip_height}), {} already-finalized request(s) affected — see \
             post_finality_reorg_events #{id}",
            affected_request_ids.len()
        );
        self.set_paused(ReserveDirection::GoldcoinReserve, true, Some(&reason))?;
        self.set_paused(ReserveDirection::SolanaReserve, true, Some(&reason))?;
        Ok(id)
    }

    /// Count of post-finality-reorg events ever recorded — surfaced by
    /// `ops::health`/`glc-admin status` so this is visible without an
    /// operator having to know to query the table directly.
    pub fn post_finality_reorg_event_count(&self) -> Result<i64, LedgerError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM post_finality_reorg_events", [], |r| {
                r.get(0)
            })?)
    }

    /// Cumulative amount ever settled for a direction — an accounting
    /// counter, not part of the capacity formula (docs/05-reserve-
    /// accounting.md).
    pub fn settled_liquidity(&self, direction: ReserveDirection) -> Result<u64, LedgerError> {
        self.conn
            .query_row(
                "SELECT settled_liquidity_total FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u64)
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::ReserveNotInitialized(direction)
                }
                other => LedgerError::Sqlite(other),
            })
    }

    pub fn goldcoin_reorg_event_count(&self) -> Result<i64, LedgerError> {
        Ok(self
            .conn
            .query_row("SELECT count(*) FROM goldcoin_reorg_events", [], |r| {
                r.get(0)
            })?)
    }

    // ------------------------------------------------------- Solana release --

    /// `SourceFinalized -> DestinationSubmitted` for a `GlcToSol` request:
    /// the `release_from_reserve` transaction (carrying the threshold
    /// attestation proof) has been submitted to Solana, `signature` is its
    /// transaction signature. Unlike the Goldcoin payout path, there is no
    /// separate "Signed" step to persist — attestation collection happens
    /// in-memory, off the database, immediately before submission
    /// ([`crate::signing::attestation`]). Idempotent: a no-op once already
    /// `DestinationSubmitted` or later.
    pub fn record_release_submitted(
        &mut self,
        request_id: i64,
        signature: [u8; 64],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state): (Direction, RequestState) = tx.query_row(
            "SELECT direction, state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        assert_eq!(
            direction,
            Direction::GlcToSol,
            "record_release_submitted on a non-GlcToSol request"
        );
        if state != RequestState::SourceFinalized {
            tx.rollback()?;
            return Ok(());
        }
        tx.execute(
            "UPDATE bridge_requests SET state = 'DestinationSubmitted', destination_txid = ?1 WHERE id = ?2",
            rusqlite::params![signature.as_slice(), request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::DestinationSubmitted,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `DestinationSubmitted -> Settled` for a `GlcToSol` request: the
    /// `release_from_reserve` transaction has confirmed on Solana. Unlike
    /// the Goldcoin payout path there is no further on-chain step after
    /// this confirms (the release instruction itself both creates the
    /// replay-guard `DepositClaim` and moves the funds), so
    /// `DestinationConfirmed` and `Settled` are reached together. Moves the
    /// amount out of `reserved_liquidity`/`pending_obligations` into
    /// `settled_liquidity_total` for the Solana reserve
    /// (docs/05-reserve-accounting.md). Idempotent: a no-op if already
    /// `Settled`.
    pub fn mark_release_confirmed(&mut self, request_id: i64, now: i64) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let (direction, state, amount, fee): (Direction, RequestState, i64, i64) = tx.query_row(
            "SELECT direction, state, net_destination_atomic, fee_amount_atomic FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        assert_eq!(
            direction,
            Direction::GlcToSol,
            "mark_release_confirmed on a non-GlcToSol request"
        );
        if state == RequestState::Settled {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            state,
            RequestState::DestinationSubmitted,
            "mark_release_confirmed on unexpected bridge_request state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = 'DestinationConfirmed' WHERE id = ?1",
            [request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::DestinationConfirmed,
            now,
            None,
            "system",
        )?;
        tx.execute(
            "UPDATE bridge_requests SET state = 'Settled', settled_at = ?1 WHERE id = ?2",
            rusqlite::params![now, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(RequestState::DestinationConfirmed),
            RequestState::Settled,
            now,
            None,
            "system",
        )?;
        // `total_reserve_balance` is decremented here, not left for the next
        // reconciliation pass to discover: reconciliation flags any drop
        // between its cached balance and a fresh on-chain read as an
        // unexplained breach (and pauses the reserve, one-way). A confirmed
        // release is an *explained* drop this service itself caused, so the
        // cache must reflect it immediately — otherwise the very next
        // reconcile sees the real chain balance already down by `amount`
        // while its own cache is still stale, and misclassifies a routine
        // settlement as a breach.
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1, pending_obligations = pending_obligations - ?1,
                settled_liquidity_total = settled_liquidity_total + ?1, total_reserve_balance = total_reserve_balance - ?1
                WHERE direction = 'SolanaReserve'",
            [amount],
        )?;
        // The fee for a GlcToSol settlement is collected on the SOURCE side
        // (Goldcoin — docs/20-bridge-fee.md: "the fee remains on the source
        // side where it was collected"), in canonical units (numerically
        // Goldcoin-native already) — a separate row from the SolanaReserve
        // update above, and deliberately never netted against it.
        tx.execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = accrued_fees_atomic + ?1
                WHERE direction = 'GoldcoinReserve'",
            [fee],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ----------------------------------------------------------------- queries --

    /// Request ids with a `goldcoin_payouts` row currently in `state`
    /// (`'Built'|'Signed'|'Broadcast'|'Confirmed'|'Completed'`), oldest
    /// first — what [`crate::orchestrator`] polls to drive the Goldcoin
    /// payout lifecycle forward.
    pub fn goldcoin_payouts_in_state(&self, state: &str) -> Result<Vec<i64>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT request_id FROM goldcoin_payouts WHERE state = ?1 ORDER BY request_id",
        )?;
        let rows = stmt
            .query_map([state], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The request's `destination_confirmations` — the operator-facing
    /// mirror of the destination leg's observed confirmation depth (kept
    /// fresh by [`Ledger::update_goldcoin_payout_confirmations`] for as
    /// long as the leg is live, including after `DestinationConfirmed`).
    pub fn destination_confirmations(&self, request_id: i64) -> Result<i64, LedgerError> {
        self.conn
            .query_row(
                "SELECT destination_confirmations FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(LedgerError::RequestNotFound(request_id))
    }

    pub fn requests_by_state(
        &self,
        direction: Direction,
        state: RequestState,
    ) -> Result<Vec<BridgeRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_REQUEST_PREFIX} WHERE direction = ?1 AND state = ?2 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![direction, state], row_to_request)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Per-`RequestState` counts for `direction` — the count-only
    /// equivalent of [`Ledger::requests_by_state`], used by the public
    /// `/stats` read-projection (`api::BridgeApi::stats`) so a large
    /// `bridge_requests` table is aggregated in SQL rather than fetched
    /// row-by-row into memory just to be counted.
    pub fn request_state_counts(
        &self,
        direction: Direction,
    ) -> Result<Vec<(RequestState, i64)>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT state, COUNT(*) FROM bridge_requests WHERE direction = ?1 GROUP BY state",
        )?;
        let rows = stmt
            .query_map([direction], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// One page of `bridge_requests`, newest first — the real,
    /// already-persisted request list behind the public `GET /transfers`
    /// read-projection (`api::BridgeApi::list_transfers`), i.e. a
    /// wallet-scoped "my activity" view. `address` matches a 32-byte
    /// Solana pubkey against whichever column actually carries the
    /// caller's own address for that direction: `recipient` for
    /// `GlcToSol` (the destination the caller chose), `requester` for
    /// `SolToGlc` (the depositor the indexer observed on-chain) — the
    /// two directions never share a matching column, so both are checked
    /// rather than requiring the caller to know which one applies.
    pub fn transfers_page(
        &self,
        address: Option<[u8; 32]>,
        state: Option<RequestState>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<BridgeRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_REQUEST_PREFIX} WHERE \
             (?1 IS NULL OR (direction = 'GlcToSol' AND recipient = ?1) \
                          OR (direction = 'SolToGlc' AND requester = ?1)) \
             AND (?2 IS NULL OR state = ?2) \
             AND (?3 IS NULL OR id < ?3) \
             ORDER BY id DESC LIMIT ?4"
        ))?;
        let address_bytes: Option<Vec<u8>> = address.map(|a| a.to_vec());
        let rows = stmt
            .query_map(
                rusqlite::params![address_bytes, state, before_id, limit as i64],
                row_to_request,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn state_log(&self, request_id: i64) -> Result<Vec<StateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason FROM bridge_request_state_log
             WHERE request_id = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([request_id], |r| {
                let from: Option<String> = r.get(0)?;
                let to: String = r.get(1)?;
                Ok((
                    from.map(|s| s.parse().unwrap()),
                    to.parse().unwrap(),
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // --------------------------------------------------- admin audit log --

    /// Opens the outer transaction an admin mutation and its audit row
    /// share, so the two are ATOMIC: either the mutation commits together
    /// with its audit row, or neither persists. While this scope is open,
    /// every Ledger mutation's own [`WriteTx`] becomes a savepoint nested
    /// inside it — a validated refusal rolls back only the mutation's
    /// writes, and the scope then commits just the failure audit row.
    /// `BEGIN IMMEDIATE`, same write-lock posture as every standalone
    /// mutation. If the caller drops the `Ledger` without committing
    /// (error path, panic), SQLite rolls the whole scope back with the
    /// connection.
    ///
    /// `pub(crate)` on purpose: the only caller is
    /// `admin_api::audited_mutation`, which commits or rolls back on
    /// every path. A leaked open scope would silently downgrade every
    /// later mutation on the connection to a savepoint whose "commit" is
    /// only a RELEASE — never expose this trio for ad-hoc use.
    pub(crate) fn begin_admin_action(&mut self) -> Result<(), LedgerError> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    pub(crate) fn commit_admin_action(&mut self) -> Result<(), LedgerError> {
        self.conn.execute_batch("COMMIT")?;
        Ok(())
    }

    pub(crate) fn rollback_admin_action(&mut self) -> Result<(), LedgerError> {
        self.conn.execute_batch("ROLLBACK")?;
        Ok(())
    }

    /// Appends one admin mutation attempt to the append-only
    /// `admin_audit_log` (schema v15) and returns the new row id. Called
    /// for every privileged mutation ATTEMPT — successes and refusals
    /// alike (`AdminAuditOutcome::Error` carries the operator-visible
    /// failure message). The schema `CHECK`s `actor`/`action`/`note`
    /// non-empty, so a caller that forgets to enforce the mandatory note
    /// fails closed here rather than writing a noteless row. Rows are
    /// never updated or deleted by anything in this crate.
    pub fn append_admin_audit(&mut self, entry: &AdminAuditEntry) -> Result<i64, LedgerError> {
        let (outcome, error) = match &entry.outcome {
            AdminAuditOutcome::Success => ("success", None),
            AdminAuditOutcome::Error(message) => ("error", Some(message.as_str())),
        };
        self.conn.execute(
            "INSERT INTO admin_audit_log
                 (at, actor, action, target, old_value, new_value, note, outcome, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                entry.at,
                entry.actor,
                entry.action,
                entry.target,
                entry.old_value,
                entry.new_value,
                entry.note,
                outcome,
                error,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Reads `admin_audit_log` rows newest-first with keyset pagination
    /// (`filter.before_id` = "rows older than this id"), optionally
    /// restricted to one `action` slug and/or one `actor`. `limit` is
    /// clamped into `1..=200` (the public API's `MAX_PAGE_LIMIT`
    /// discipline; a zero limit would be a permanently empty page that
    /// reads as "no audit rows") and defaults to 50.
    pub fn list_admin_audit(
        &self,
        filter: &AdminAuditFilter,
    ) -> Result<Vec<AdminAuditRow>, LedgerError> {
        let limit = i64::from(filter.limit.unwrap_or(50).clamp(1, 200));
        let mut stmt = self.conn.prepare(
            "SELECT id, at, actor, action, target, old_value, new_value, note, outcome, error
             FROM admin_audit_log
             WHERE (?1 IS NULL OR id < ?1)
               AND (?2 IS NULL OR action = ?2)
               AND (?3 IS NULL OR actor = ?3)
             ORDER BY id DESC
             LIMIT ?4",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![filter.before_id, filter.action, filter.actor, limit],
                |r| {
                    let outcome: String = r.get(8)?;
                    let error: Option<String> = r.get(9)?;
                    Ok(AdminAuditRow {
                        id: r.get(0)?,
                        at: r.get(1)?,
                        actor: r.get(2)?,
                        action: r.get(3)?,
                        target: r.get(4)?,
                        old_value: r.get(5)?,
                        new_value: r.get(6)?,
                        note: r.get(7)?,
                        outcome: if outcome == "success" {
                            AdminAuditOutcome::Success
                        } else {
                            AdminAuditOutcome::Error(error.unwrap_or_default())
                        },
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Direct SQL access for tests that need queries not otherwise exposed.
    /// Kept `pub(crate)` and test-only — production code (including
    /// `reconciliation`) should add a typed method above instead of
    /// reaching for raw SQL.
    #[cfg(test)]
    pub(crate) fn raw(&self) -> &Connection {
        &self.conn
    }

    // ------------------------------------------------------- Goldcoin vault --

    /// Reconciles observed vault UTXOs (from a live `listunspent` read)
    /// against `vault_utxos`: promotes `Unconfirmed -> Available` once
    /// `confirmations >= min_confirmations`, inserts newly-seen outputs,
    /// and marks any previously `Available`/`Unconfirmed` outpoint that no
    /// longer appears as `Spent` (something external moved it — e.g. a
    /// rebalance, or, if it happens unexpectedly, an anomaly reconciliation
    /// should catch separately). Never disturbs a `Reserved` or `Spent`
    /// row — same discipline the old bridge's `sync_vault_utxos` used
    /// (docs/01-reuse-inventory.md).
    pub fn sync_vault_utxos(
        &mut self,
        observed: &[(crate::goldcoin::coin::VaultUtxo, i64, String)], // (utxo, confirmations, script_pubkey_hex)
        min_confirmations: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut still_present = std::collections::HashSet::new();
        for (utxo, confirmations, script_pubkey_hex) in observed {
            still_present.insert((utxo.txid.to_vec(), utxo.vout));
            let state = if *confirmations >= min_confirmations {
                "Available"
            } else {
                "Unconfirmed"
            };
            // Resurrection rule (2026-08-30 review, blocker: one missed
            // snapshot must never permanently destroy accounting state):
            // a row this service itself marked spent (`spent_by_txid` set
            // by a broadcast payout/split — a transaction WE signed) is
            // sticky forever, since offering it to selection again would
            // double-spend our own in-flight transaction. A row the
            // ABSENCE branch below inferred spent (`spent_by_txid` NULL)
            // is a chain observation, and a fresh `listunspent` snapshot
            // reporting the outpoint unspent again (parent re-broadcast
            // after eviction, reorg restored it) is the same class of
            // observation — chain truth wins in both directions.
            tx.execute(
                "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(txid, vout) DO UPDATE SET
                    confirmations = excluded.confirmations,
                    state = CASE
                        WHEN vault_utxos.state = 'Reserved' THEN 'Reserved'
                        WHEN vault_utxos.state = 'Spent' AND vault_utxos.spent_by_txid IS NOT NULL THEN 'Spent'
                        ELSE excluded.state END",
                rusqlite::params![utxo.txid.as_slice(), utxo.vout, utxo.amount_atomic as i64, script_pubkey_hex, confirmations, now, state],
            )?;
        }

        let mut stmt = tx.prepare(
            "SELECT txid, vout FROM vault_utxos WHERE state IN ('Available','Unconfirmed')",
        )?;
        let tracked: Vec<(Vec<u8>, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for (txid, vout) in tracked {
            if !still_present.contains(&(txid.clone(), vout as u32)) {
                // Chunk outputs of a split still in `Broadcast` are
                // exempt from the absence flip: their transaction lives
                // or dies with its mempool acceptance, and the shaping
                // lifecycle owns that fate explicitly (first confirmation
                // -> `Confirmed`; evicted -> re-broadcast the exact
                // stored bytes; inputs gone -> `Abandoned`, which marks
                // these rows `Spent` itself). Flipping them here on one
                // missed snapshot erased the `own_unconfirmed_change`
                // term mid-eviction and auto-paused a healthy reserve
                // (2026-08-30 review). Deliberately NOT extended to
                // 0-conf payout change: its disappearance must keep
                // removing it from the selectable pools immediately, as
                // the zero-conf policy's own tests pin — those rows take
                // the ordinary flip below and rely on the resurrection
                // rule above once the parent is restored.
                tx.execute(
                    "UPDATE vault_utxos SET state = 'Spent'
                     WHERE txid = ?1 AND vout = ?2
                       AND NOT EXISTS (
                         SELECT 1 FROM vault_utxo_splits s
                          WHERE s.txid = vault_utxos.txid AND s.state = 'Broadcast'
                       )",
                    rusqlite::params![txid, vout],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// UTXOs available for coin selection, sorted `(amount DESC, txid ASC,
    /// vout ASC)` — [`crate::goldcoin::coin::select`] requires this exact
    /// order for its selection to be deterministic.
    /// Excludes any UTXO still backing a GlcToSol deposit that has not yet
    /// reached `SourceFinalized` (`DepositObserved`/`Confirming`) — a
    /// SolToGlc payout spending such a UTXO before the deposit's own
    /// confirmation depth is reached would strand that GlcToSol request
    /// (see `mark_glc_deposit_spent_before_finalized`'s fail-closed
    /// backstop for the case that already happened before this exclusion
    /// existed). Ordinary vault change/deposit UTXOs unrelated to any
    /// bridge request are unaffected.
    pub fn available_vault_utxos(
        &self,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT v.txid, v.vout, v.amount_atomic, v.script_pubkey_hex FROM vault_utxos v
             WHERE v.state = 'Available'
               AND NOT EXISTS (
                 SELECT 1 FROM bridge_requests b
                 WHERE b.direction = 'GlcToSol'
                   AND b.source_txid = v.txid
                   AND b.source_vout = v.vout
                   AND b.state IN ('DepositObserved', 'Confirming')
               )
               -- The source of a live vault-UTXO split is CLAIMED from
               -- the instant its `Built` row commits (`record_vault_utxo_
               -- split_built`): a mempooled or about-to-exist split
               -- transaction spends it, so offering it to payout
               -- selection would construct a guaranteed double-spend.
               -- Same join-exclusion idiom as the deposit-backing guard
               -- above; an `Abandoned` split releases the claim.
               AND NOT EXISTS (
                 SELECT 1 FROM vault_utxo_splits s
                 WHERE s.source_txid = v.txid
                   AND s.source_vout = v.vout
                   AND s.state IN ('Built','Signed','Broadcast')
               )
             ORDER BY v.amount_atomic DESC, v.txid ASC, v.vout ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(crate::goldcoin::coin::VaultUtxo {
                    txid: to_array32(&txid),
                    vout: r.get(1)?,
                    amount_atomic: r.get::<_, i64>(2)? as u64,
                    script_pubkey_hex: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The 0-conf-spendability candidate pool (docs/09-runbook.md
    /// "Zero-conf payout change"): vault UTXOs that are (a) still short of
    /// `vault_min_confirmations` (`state = 'Unconfirmed'`), (b)
    /// AUTHORITATIVELY this service's own payout change — an exact
    /// `(txid, vout)` join against `goldcoin_payout_change_outpoints`,
    /// never a script/address heuristic (external deposits, vault-split
    /// outputs, and anything else without a provenance row NEVER
    /// qualify, at any confirmation count below the threshold), (c) not
    /// on a parent-validation hold (`zero_conf_hold_reason IS NULL` —
    /// see `Orchestrator::tick_validate_zero_conf_parents`), and (d)
    /// within the unconfirmed-ancestry cap: at 0 confirmations the
    /// recorded `unconfirmed_ancestor_depth` must be <= `max_depth`;
    /// from 1 confirmation on, every own-chain ancestor is buried under
    /// output's own confirmation, so the depth cap no longer applies
    /// (the row is still below the external threshold, which is exactly
    /// what this policy makes spendable). `max_depth = 0` disables the
    /// policy outright (kill switch) — this returns an empty pool.
    ///
    /// Same deterministic ordering and deposit-backing exclusion as
    /// [`Ledger::available_vault_utxos`]; callers treat this as
    /// ADDITIONAL liquidity, only after confirmed UTXOs alone were
    /// insufficient (`signing::goldcoin_vault`'s two-phase selection).
    pub fn zero_conf_change_vault_utxos(
        &self,
        max_depth: u32,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        if max_depth == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT v.txid, v.vout, v.amount_atomic, v.script_pubkey_hex FROM vault_utxos v
             JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
             WHERE v.state = 'Unconfirmed'
               AND v.zero_conf_hold_reason IS NULL
               AND (v.confirmations >= 1 OR o.unconfirmed_ancestor_depth <= ?1)
               AND NOT EXISTS (
                 SELECT 1 FROM bridge_requests b
                 WHERE b.direction = 'GlcToSol'
                   AND b.source_txid = v.txid
                   AND b.source_vout = v.vout
                   AND b.state IN ('DepositObserved', 'Confirming')
               )
             ORDER BY v.amount_atomic DESC, v.txid ASC, v.vout ASC",
        )?;
        let rows = stmt
            .query_map([max_depth], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(crate::goldcoin::coin::VaultUtxo {
                    txid: to_array32(&txid),
                    vout: r.get(1)?,
                    amount_atomic: r.get::<_, i64>(2)? as u64,
                    script_pubkey_hex: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Distinct parent payout txids whose 0-conf change is currently a
    /// policy candidate (or held) — what
    /// `Orchestrator::tick_validate_zero_conf_parents` re-checks against
    /// the live Goldcoin node every tick before any selection may use the
    /// change. Only 0-confirmation rows: once the parent has >= 1
    /// confirmation the chain itself is the acceptance proof.
    pub fn zero_conf_parent_txids(&self) -> Result<Vec<[u8; 32]>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT v.txid FROM vault_utxos v
             JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
             WHERE v.state = 'Unconfirmed' AND v.confirmations = 0",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(to_array32(&txid))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Places (reason) or clears (`None`) the parent-validation hold on
    /// every unconfirmed change output of parent payout `txid` — the
    /// reversible, persisted exclusion `zero_conf_change_vault_utxos`
    /// honors. A hold means "the configured node does not currently
    /// know/accept this parent transaction" (evicted, conflicted,
    /// replaced, or an RPC failure — all fail closed identically);
    /// clearing happens only after a fresh successful re-validation.
    pub fn set_zero_conf_hold(
        &mut self,
        txid: [u8; 32],
        reason: Option<&str>,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "UPDATE vault_utxos SET zero_conf_hold_reason = ?1
              WHERE txid = ?2 AND state = 'Unconfirmed'",
            rusqlite::params![reason, txid.as_slice()],
        )?;
        Ok(())
    }

    /// Sum of `vault_utxos` rows still short of `vault_min_confirmations`
    /// (`state = 'Unconfirmed'`) — value the vault genuinely holds but that
    /// `total_reserve_balance`/reconciliation's `observed_balance`
    /// deliberately excludes until it matures (see `sync_vault_utxos` and
    /// `Orchestrator::tick_goldcoin_reconciliation`, both of which already
    /// filter by `vault_min_confirmations` before that figure is computed
    /// — this method changes nothing about that; it only reads the
    /// portion already being excluded, for display). Purely observational:
    /// never added to `total_reserve_balance`, never consulted by
    /// `reconcile`'s hard invariant or the auto-pause decision. Exists so
    /// an operator seeing a paused reserve can see whether recovery is
    /// already in flight (a large mature-soon change output) rather than
    /// requiring a genuinely new deposit.
    pub fn immature_vault_utxo_total(&self) -> Result<u64, LedgerError> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(amount_atomic), 0) FROM vault_utxos WHERE state = 'Unconfirmed'",
            [],
            |r| r.get(0),
        )?;
        Ok(total as u64)
    }

    /// Sum of `amount_atomic` for `Unconfirmed` `vault_utxos` rows whose
    /// txid matches a KNOWN `goldcoin_payouts` broadcast OR a known
    /// `vault_utxo_splits` broadcast — i.e. value that is temporarily
    /// invisible to a live chain scan for a reason this service itself
    /// already knows about (its own payout's change, or its own split's
    /// chunk outputs, not yet mature), as opposed to any other
    /// still-maturing deposit. A split's outputs carry exactly the same
    /// authoritative provenance as payout change: the txid was computed by
    /// this service from the bytes it itself broadcast
    /// (`record_vault_utxo_split_broadcast`), never trusted from the node.
    /// Since a real Goldcoin transaction's non-vault outputs (the external
    /// destination) never appear in `vault_utxos` at all (this service
    /// only watches its own vault/deposit addresses), a `vault_utxos` row
    /// matching a payout's txid is unambiguously that payout's OWN change
    /// — never its destination. Grounded entirely in live, currently
    /// observed state (never a payout-lifecycle proxy), so it can never
    /// double-count a change output that has already matured to
    /// `Available` (excluded by the `state = 'Unconfirmed'` filter) or
    /// miss one whose parent payout has moved past `Broadcast` while the
    /// physical output is still genuinely immature. See
    /// `Ledger::pending_destination_settlement_amount`'s use of this
    /// alongside (not instead of) its existing `Broadcast`-state term.
    pub fn own_unconfirmed_change_atomic(&self) -> Result<u64, LedgerError> {
        let total: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(v.amount_atomic), 0) FROM vault_utxos v
             WHERE v.state = 'Unconfirmed'
               AND (EXISTS (SELECT 1 FROM goldcoin_payouts p WHERE p.txid = v.txid)
                 OR EXISTS (SELECT 1 FROM vault_utxo_splits s WHERE s.txid = v.txid AND s.state IN ('Broadcast','Confirmed')))",
            [],
            |r| r.get(0),
        )?;
        Ok(total as u64)
    }

    /// Count of still-immature (`Unconfirmed`) chunk outputs belonging to
    /// this service's own broadcast vault-UTXO splits — the guard
    /// `goldcoin::liquidity::run_shaping_tick` uses to avoid stacking a
    /// second self-transaction while the previous one's liquidity is
    /// already en route to maturity. Split outputs only, deliberately NOT
    /// payout change: under continuous traffic there is nearly always
    /// some immature payout change, and gating shaping on that would
    /// starve it exactly when it is needed.
    pub fn unconfirmed_split_chunk_count(&self) -> Result<u32, LedgerError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Unconfirmed'
               AND EXISTS (SELECT 1 FROM vault_utxo_splits s WHERE s.txid = v.txid AND s.state IN ('Broadcast','Confirmed'))",
            [],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// `mature_available_atomic`: sum of `available_vault_utxos()` — real,
    /// currently spendable reserve value, the same candidate pool coin
    /// selection draws from. `own_unconfirmed_change_atomic`: this
    /// service's own broadcast-but-immature payout change (see that
    /// method's docs) — known, not missing. `available_utxo_count`/
    /// `unconfirmed_change_utxo_count`: the same two categories, counted
    /// rather than summed — a leading indicator distinct from the value
    /// figures (see docs/09-runbook.md's "UTXO liquidity" section): the
    /// accounting can look healthy while the POOL itself is a single
    /// oversized UTXO or a handful of nearly-exhausted ones.
    pub fn utxo_pool_health(&self) -> Result<UtxoPoolHealth, LedgerError> {
        let mature_available_atomic: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(v.amount_atomic), 0) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND NOT EXISTS (
                 SELECT 1 FROM bridge_requests b
                 WHERE b.direction = 'GlcToSol'
                   AND b.source_txid = v.txid
                   AND b.source_vout = v.vout
                   AND b.state IN ('DepositObserved', 'Confirming')
               )",
            [],
            |r| r.get(0),
        )?;
        let available_utxo_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Available'
               AND NOT EXISTS (
                 SELECT 1 FROM bridge_requests b
                 WHERE b.direction = 'GlcToSol'
                   AND b.source_txid = v.txid
                   AND b.source_vout = v.vout
                   AND b.state IN ('DepositObserved', 'Confirming')
               )",
            [],
            |r| r.get(0),
        )?;
        let unconfirmed_change_utxo_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             WHERE v.state = 'Unconfirmed'
               AND (EXISTS (SELECT 1 FROM goldcoin_payouts p WHERE p.txid = v.txid)
                 OR EXISTS (SELECT 1 FROM vault_utxo_splits s WHERE s.txid = v.txid AND s.state IN ('Broadcast','Confirmed')))",
            [],
            |r| r.get(0),
        )?;
        // Zero-conf-policy visibility (docs/09-runbook.md "Zero-conf
        // payout change"): candidates = authoritative payout change still
        // below the confirmed threshold and not on a parent-validation
        // hold — depth-agnostic here (the depth cap is config, applied at
        // selection), so an operator sees the full policy pool alongside,
        // never mixed into, the confirmed figures above.
        let (zero_conf_candidate_atomic, zero_conf_candidate_count): (i64, i64) =
            self.conn.query_row(
                "SELECT COALESCE(SUM(v.amount_atomic), 0), COUNT(*) FROM vault_utxos v
                 JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
                 WHERE v.state = 'Unconfirmed' AND v.zero_conf_hold_reason IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
        let zero_conf_held_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM vault_utxos v
             JOIN goldcoin_payout_change_outpoints o ON o.txid = v.txid AND o.vout = v.vout
             WHERE v.state = 'Unconfirmed' AND v.zero_conf_hold_reason IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(UtxoPoolHealth {
            mature_available_atomic: mature_available_atomic as u64,
            own_unconfirmed_change_atomic: self.own_unconfirmed_change_atomic()?,
            available_utxo_count: available_utxo_count as u32,
            unconfirmed_change_utxo_count: unconfirmed_change_utxo_count as u32,
            zero_conf_change_candidate_atomic: zero_conf_candidate_atomic as u64,
            zero_conf_change_candidate_count: zero_conf_candidate_count as u32,
            zero_conf_change_held_count: zero_conf_held_count as u32,
        })
    }

    /// Atomically reserves `selected` for `request_id`. The guarded
    /// conditional `UPDATE` is the actual concurrency control (SQLite's
    /// write-transaction lock, not an application-level mutex): if a
    /// concurrent reservation already claimed one of these outpoints,
    /// fewer rows match than expected and the whole reservation is rolled
    /// back and reported — never partially reserved.
    ///
    /// A row is reservable when it is `Available` (confirmed at
    /// `vault_min_confirmations`), OR when it satisfies the exact same
    /// 0-conf-payout-change eligibility predicate
    /// [`Ledger::zero_conf_change_vault_utxos`] selects by — re-checked
    /// HERE, inside the reservation's own write transaction, so a row
    /// whose eligibility lapsed between selection and reservation
    /// (parent hold placed, provenance absent) fails the reservation
    /// closed instead of being reserved on stale grounds. An
    /// `Unconfirmed` row without authoritative change provenance can
    /// never be reserved at any `zero_conf_max_depth`.
    pub fn reserve_vault_utxos(
        &mut self,
        request_id: i64,
        selected: &[crate::goldcoin::coin::VaultUtxo],
        zero_conf_max_depth: u32,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let mut reserved_count = 0usize;
        for u in selected {
            let n = tx.execute(
                "UPDATE vault_utxos SET state = 'Reserved', reserved_by = ?1, reserved_at = ?2
                 WHERE txid = ?3 AND vout = ?4
                   AND (
                     state = 'Available'
                     OR (
                       ?5 > 0
                       AND state = 'Unconfirmed'
                       AND zero_conf_hold_reason IS NULL
                       AND EXISTS (
                         SELECT 1 FROM goldcoin_payout_change_outpoints o
                          WHERE o.txid = vault_utxos.txid AND o.vout = vault_utxos.vout
                            AND (vault_utxos.confirmations >= 1
                                 OR o.unconfirmed_ancestor_depth <= ?5)
                       )
                     )
                   )
                   -- Re-checked HERE, inside the reservation's own write
                   -- transaction, exactly like the 0-conf eligibility
                   -- predicate above: a UTXO claimed as a live split's
                   -- source between this payout's selection and its
                   -- reservation fails the reservation closed instead of
                   -- being double-committed (see `available_vault_utxos`'s
                   -- matching exclusion for the rationale).
                   AND NOT EXISTS (
                     SELECT 1 FROM vault_utxo_splits s
                     WHERE s.source_txid = vault_utxos.txid
                       AND s.source_vout = vault_utxos.vout
                       AND s.state IN ('Built','Signed','Broadcast')
                   )",
                rusqlite::params![
                    request_id,
                    now,
                    u.txid.as_slice(),
                    u.vout,
                    zero_conf_max_depth
                ],
            )?;
            reserved_count += n;
        }
        if reserved_count != selected.len() {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoUnavailable {
                requested: selected.len(),
                available: reserved_count,
            });
        }
        tx.commit()?;
        Ok(())
    }

    // ------------------------------------------------------- vault UTXO split --

    /// Read-only lookup of a single `vault_utxos` row — what
    /// [`crate::signing::goldcoin_split`]'s independent re-derivation needs
    /// to confirm a proposed split source is real, mature, and owned by
    /// the script the caller claims, entirely from this ledger's own view
    /// (never trusted from a caller-supplied amount).
    pub fn get_vault_utxo(
        &self,
        txid: [u8; 32],
        vout: u32,
    ) -> Result<Option<VaultUtxoRow>, LedgerError> {
        self.conn
            .query_row(
                "SELECT amount_atomic, script_pubkey_hex, state FROM vault_utxos
                 WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![txid.as_slice(), vout],
                |r| {
                    Ok(VaultUtxoRow {
                        amount_atomic: r.get::<_, i64>(0)? as u64,
                        script_pubkey_hex: r.get(1)?,
                        state: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Read-only lookup of any existing split attempt for a given source
    /// outpoint — the idempotency check `glc-admin split-vault-utxo` runs
    /// before ever contacting a signer: a source outpoint that already has
    /// a row here has already been split (or is mid-flight), and must
    /// never be split again.
    pub fn get_vault_utxo_split(
        &self,
        source_txid: [u8; 32],
        source_vout: u32,
    ) -> Result<Option<VaultUtxoSplitSnapshot>, LedgerError> {
        self.conn
            .query_row(
                "SELECT id, source_amount_atomic, chunk_count, chunk_target_atomic, fee_atomic,
                        unsigned_tx_hex, signed_tx_hex, txid, state
                 FROM vault_utxo_splits
                 WHERE source_txid = ?1 AND source_vout = ?2 AND state != 'Abandoned'",
                rusqlite::params![source_txid.as_slice(), source_vout],
                |r| {
                    let txid_vec: Option<Vec<u8>> = r.get(7)?;
                    Ok(VaultUtxoSplitSnapshot {
                        id: r.get(0)?,
                        source_amount_atomic: r.get::<_, i64>(1)? as u64,
                        chunk_count: r.get(2)?,
                        chunk_target_atomic: r.get::<_, i64>(3)? as u64,
                        fee_atomic: r.get::<_, i64>(4)? as u64,
                        unsigned_tx_hex: r.get(5)?,
                        signed_tx_hex: r.get(6)?,
                        txid: txid_vec.map(|v| to_array32(&v)),
                        state: r.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Records a freshly built (not yet signed) vault UTXO split — and,
    /// with it, the CLAIM on the source outpoint: from the moment this
    /// commits, the source is invisible to payout coin selection and
    /// unreservable by any payout ([`Ledger::available_vault_utxos`] and
    /// [`Ledger::reserve_vault_utxos`] both exclude the source of any
    /// live — non-`Abandoned`, non-`Confirmed` — split row), so a payout
    /// and a split can never contend for the same UTXO no matter how the
    /// two processes interleave (2026-08-30 review, blocker: the
    /// CLI-vs-daemon race). The claim is validated here, inside this same
    /// transaction: the source row must exist and be `Available` (a
    /// `Reserved` source is already promised to a payout — claiming it
    /// would be the same double-spend in the other direction). The
    /// explicit existence check, backed by `ux_vault_utxo_splits_source`'s
    /// structural partial-`UNIQUE(source_txid, source_vout) WHERE state !=
    /// 'Abandoned'` guarantee, is the idempotency boundary — an
    /// `Abandoned` prior attempt never blocks a fresh, legitimate split
    /// of the same outpoint.
    #[allow(clippy::too_many_arguments)]
    pub fn record_vault_utxo_split_built(
        &mut self,
        plan: &crate::goldcoin::split::SplitPlan,
        chunk_target_atomic: u64,
        unsigned_tx_hex: &str,
        note: &str,
        now: i64,
    ) -> Result<i64, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT id FROM vault_utxo_splits
                 WHERE source_txid = ?1 AND source_vout = ?2 AND state != 'Abandoned'",
                rusqlite::params![plan.source.txid.as_slice(), plan.source.vout],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoAlreadySplit {
                txid: plan.source.txid,
                vout: plan.source.vout,
            });
        }
        let source_state: Option<String> = tx
            .query_row(
                "SELECT state FROM vault_utxos WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![plan.source.txid.as_slice(), plan.source.vout],
                |r| r.get(0),
            )
            .optional()?;
        match source_state.as_deref() {
            Some("Available") => {}
            None => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotFound {
                    txid: plan.source.txid,
                    vout: plan.source.vout,
                });
            }
            Some(other) => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotSplittable {
                    txid: plan.source.txid,
                    vout: plan.source.vout,
                    state: other.to_string(),
                });
            }
        }
        tx.execute(
            "INSERT INTO vault_utxo_splits
                (source_txid, source_vout, source_amount_atomic, chunk_count, chunk_target_atomic,
                 fee_atomic, unsigned_tx_hex, state, note, built_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'Built', ?8, ?9)",
            rusqlite::params![
                plan.source.txid.as_slice(),
                plan.source.vout,
                plan.source.amount_atomic as i64,
                plan.output_count() as i64,
                chunk_target_atomic as i64,
                plan.fee_atomic as i64,
                unsigned_tx_hex,
                note,
                now,
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    /// `Built -> Signed`.
    pub fn record_vault_utxo_split_signed(
        &mut self,
        id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE vault_utxo_splits SET signed_tx_hex = ?1, state = 'Signed', signed_at = ?2
             WHERE id = ?3 AND state = 'Built'",
            rusqlite::params![signed_tx_hex, now, id],
        )?;
        if n == 0 {
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        }
        Ok(())
    }

    /// `Signed -> Broadcast`, with EVERY consequence of the broadcast in
    /// the same transaction (2026-08-30 review: the previous two-call
    /// protocol — state transition here, source/chunk bookkeeping in a
    /// separate later call — left a crash window in which the ledger
    /// believed the source was still spendable while the mempool already
    /// spent it):
    ///
    /// 1. the split row itself moves to `Broadcast`;
    /// 2. the source outpoint becomes `Spent` (`spent_by_txid` = the
    ///    split's own txid — the marker [`Ledger::sync_vault_utxos`]'s
    ///    resurrection rule treats as "spent by a transaction this
    ///    service signed", never resurrected);
    /// 3. each chunk output is inserted as an `Unconfirmed` `vault_utxos`
    ///    row (vout = output index) so `own_unconfirmed_change_atomic`
    ///    explains the mature-balance dip with no gap.
    ///
    /// Idempotent: re-recording an already-`Broadcast` split (restart
    /// between broadcast and this call, with a re-broadcast in between)
    /// re-applies effects 2 and 3 harmlessly (`ON CONFLICT DO NOTHING`,
    /// state guards). The amounts/txid come from this service's own
    /// verified [`crate::goldcoin::split::SplitPlan`] and locally-computed
    /// txid — never from node-reported data.
    pub fn record_vault_utxo_split_broadcast(
        &mut self,
        id: i64,
        txid: [u8; 32],
        output_amounts: &[u64],
        vault_script_pubkey_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Vec<u8>, u32, String)> = tx
            .query_row(
                "SELECT source_txid, source_vout, state FROM vault_utxo_splits WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((source_txid, source_vout, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        };
        match state.as_str() {
            "Signed" | "Broadcast" => {}
            other => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoSplitNotRecoverable {
                    id,
                    state: other.to_string(),
                });
            }
        }
        tx.execute(
            "UPDATE vault_utxo_splits SET state = 'Broadcast', txid = ?1,
                    broadcast_at = COALESCE(broadcast_at, ?2)
             WHERE id = ?3",
            rusqlite::params![txid.as_slice(), now, id],
        )?;
        // Source -> Spent. Only Available (or, idempotently, already
        // Spent) is acceptable: the claim placed at `Built` time excludes
        // every other transition, so anything else is bookkeeping drift
        // that must fail loudly — inside the transaction, before commit.
        let source_state: Option<String> = tx
            .query_row(
                "SELECT state FROM vault_utxos WHERE txid = ?1 AND vout = ?2",
                rusqlite::params![source_txid.as_slice(), source_vout],
                |r| r.get(0),
            )
            .optional()?;
        match source_state.as_deref() {
            Some("Available") => {
                tx.execute(
                    "UPDATE vault_utxos SET state = 'Spent', spent_by_txid = ?1
                     WHERE txid = ?2 AND vout = ?3",
                    rusqlite::params![txid.as_slice(), source_txid.as_slice(), source_vout],
                )?;
            }
            Some("Spent") => {} // idempotent re-run
            None => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotFound {
                    txid: to_array32(&source_txid),
                    vout: source_vout,
                });
            }
            Some(other) => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoNotSplittable {
                    txid: to_array32(&source_txid),
                    vout: source_vout,
                    state: other.to_string(),
                });
            }
        }
        for (i, &amount) in output_amounts.iter().enumerate() {
            tx.execute(
                "INSERT INTO vault_utxos
                    (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, 'Unconfirmed')
                 ON CONFLICT(txid, vout) DO NOTHING",
                rusqlite::params![
                    txid.as_slice(),
                    i as u32,
                    amount as i64,
                    vault_script_pubkey_hex,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `Broadcast -> Confirmed` — the split transaction has been observed
    /// with at least one confirmation (via its chunk rows' synced
    /// confirmation counts, this service's own chain view — never a
    /// node-claimed status string). Terminal: a confirmed split needs no
    /// further lifecycle driving; its chunks mature through the ordinary
    /// `sync_vault_utxos` path like any other vault output.
    pub fn record_vault_utxo_split_confirmed(
        &mut self,
        id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE vault_utxo_splits SET state = 'Confirmed', confirmed_at = ?1
             WHERE id = ?2 AND state = 'Broadcast'",
            rusqlite::params![now, id],
        )?;
        if n == 0 {
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        }
        Ok(())
    }

    /// Terminal `-> Abandoned`, from ANY non-terminal state — the
    /// lifecycle's release valve (2026-08-30 review: without one, a
    /// single split whose source became unspendable wedged all automatic
    /// shaping forever, with no non-SQL way out). In one transaction:
    ///
    /// 1. the split row becomes `Abandoned` with the reason on record —
    ///    the row is never deleted (full audit history), but the partial
    ///    unique index stops counting it, so the outpoint can be split
    ///    again later if it genuinely returns;
    /// 2. any chunk rows a `Broadcast` attempt inserted are marked
    ///    `Spent` (they can never exist on-chain — the transaction that
    ///    would have created them is unconfirmable), so no accounting
    ///    term keeps explaining value that is not coming;
    /// 3. the source row is NOT touched: `sync_vault_utxos` already
    ///    reflects its real on-chain fate (spent elsewhere -> `Spent`;
    ///    still unspent after a reorg -> resurrected `Available`, where
    ///    the lifted claim makes it selectable again).
    pub fn abandon_vault_utxo_split(
        &mut self,
        id: i64,
        reason: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(Option<Vec<u8>>, String)> = tx
            .query_row(
                "SELECT txid, state FROM vault_utxo_splits WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((split_txid, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::VaultUtxoSplitNotFound(id));
        };
        match state.as_str() {
            "Built" | "Signed" | "Broadcast" => {}
            "Abandoned" => {
                tx.rollback()?;
                return Ok(()); // idempotent
            }
            other => {
                tx.rollback()?;
                return Err(LedgerError::VaultUtxoSplitNotRecoverable {
                    id,
                    state: other.to_string(),
                });
            }
        }
        tx.execute(
            "UPDATE vault_utxo_splits
             SET state = 'Abandoned', abandoned_at = ?1, abandon_reason = ?2
             WHERE id = ?3",
            rusqlite::params![now, reason, id],
        )?;
        if let Some(txid) = split_txid {
            tx.execute(
                "UPDATE vault_utxos SET state = 'Spent'
                 WHERE txid = ?1 AND state = 'Unconfirmed'",
                rusqlite::params![txid],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every split currently in `Broadcast` — the set the shaping tick's
    /// lifecycle maintenance drives to `Confirmed` (first confirmation
    /// observed), re-broadcasts (evicted from the mempool), or abandons
    /// (inputs genuinely gone). Ordered by id for deterministic
    /// processing.
    pub fn broadcast_vault_utxo_splits(
        &self,
    ) -> Result<Vec<UnconfirmedBroadcastSplit>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, txid, signed_tx_hex FROM vault_utxo_splits
             WHERE state = 'Broadcast' ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(1)?;
                Ok(UnconfirmedBroadcastSplit {
                    id: r.get(0)?,
                    txid: to_array32(&txid),
                    signed_tx_hex: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The highest confirmation count any of `txid`'s outputs currently
    /// carries in `vault_utxos` — how the lifecycle maintenance decides a
    /// broadcast split has been mined (>= 1), from this service's own
    /// synced chain view. `None` when no output row exists at all.
    pub fn max_confirmations_for_txid(&self, txid: [u8; 32]) -> Result<Option<i64>, LedgerError> {
        self.conn
            .query_row(
                "SELECT MAX(confirmations) FROM vault_utxos WHERE txid = ?1
                 AND state IN ('Available','Unconfirmed','Reserved')",
                rusqlite::params![txid.as_slice()],
                |r| r.get::<_, Option<i64>>(0),
            )
            .map_err(LedgerError::from)
    }

    /// Every split not yet `Broadcast` — what the automatic shaping tick
    /// (`goldcoin::liquidity::run_shaping_tick`) resumes before ever
    /// considering a NEW split: a `Signed` row re-broadcasts its exact
    /// stored bytes (`goldcoin::liquidity::resume_pending_split`), a `Built` row re-signs
    /// its exact reconstructed plan. Ordered by id (oldest first) for
    /// deterministic resume order.
    pub fn pending_vault_utxo_splits(&self) -> Result<Vec<PendingVaultUtxoSplit>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_txid, source_vout, state FROM vault_utxo_splits
             WHERE state IN ('Built', 'Signed') ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(1)?;
                Ok(PendingVaultUtxoSplit {
                    id: r.get(0)?,
                    source_txid: to_array32(&txid),
                    source_vout: r.get(2)?,
                    state: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------ Goldcoin payout --

    /// Read-only snapshot of a `goldcoin_payouts` row — what an attestation
    /// signer needs from this service's own database to independently
    /// re-derive a `record_goldcoin_completion` claim
    /// ([`crate::signing::attestation`]), combined with its own live
    /// Solana read of the corresponding `WithdrawalObligation`.
    pub fn get_goldcoin_payout(
        &self,
        request_id: i64,
    ) -> Result<Option<GoldcoinPayoutSnapshot>, LedgerError> {
        self.conn
            .query_row(
                "SELECT payout_atomic, txid, state, confirmations, mined_height, onchain_completion_signature, onchain_completion_submitted_at
                 FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| {
                    let txid_vec: Option<Vec<u8>> = r.get(1)?;
                    let sig_vec: Option<Vec<u8>> = r.get(5)?;
                    Ok(GoldcoinPayoutSnapshot {
                        payout_atomic: r.get::<_, i64>(0)? as u64,
                        txid: txid_vec.map(|v| v.try_into().unwrap()),
                        state: r.get(2)?,
                        confirmations: r.get(3)?,
                        mined_height: r.get(4)?,
                        onchain_completion_signature: sig_vec.map(|v| v.try_into().unwrap()),
                        onchain_completion_submitted_at: r.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)
    }

    /// Records a freshly built (not yet signed) payout, reserving its
    /// inputs' `goldcoin_payout_inputs` rows in the same transaction — the
    /// `UNIQUE(txid, vout)` constraint on that table is the structural
    /// "an outpoint funds at most one payout, ever" guarantee, independent
    /// of `vault_utxos.state` bookkeeping (belt-and-suspenders, same as the
    /// old bridge — docs/01-reuse-inventory.md).
    #[allow(clippy::too_many_arguments)]
    pub fn record_goldcoin_payout_built(
        &mut self,
        request_id: i64,
        plan: &crate::goldcoin::payout::PayoutPlan,
        commitment_hash: [u8; 32],
        unsigned_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT request_id FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_some() {
            tx.rollback()?;
            return Err(LedgerError::PayoutAlreadyExists(request_id));
        }
        tx.execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic, dest_p2pkh_hash,
                 unsigned_tx_hex, state, built_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'Built', ?8)",
            rusqlite::params![
                request_id,
                commitment_hash.as_slice(),
                plan.payout_atomic as i64,
                plan.total_change_atomic() as i64,
                plan.fee_atomic as i64,
                plan.dest_p2pkh_hash.as_slice(),
                unsigned_tx_hex,
                now,
            ],
        )?;
        for (i, input) in plan.inputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO goldcoin_payout_inputs (request_id, input_order, txid, vout, amount_atomic) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![request_id, i as i64, input.txid.as_slice(), input.vout, input.amount_atomic as i64],
            )?;
        }
        for (i, &change_atomic) in plan.change_outputs.iter().enumerate() {
            tx.execute(
                "INSERT INTO goldcoin_payout_change_outputs (request_id, output_order, amount_atomic) VALUES (?1, ?2, ?3)",
                rusqlite::params![request_id, i as i64, change_atomic as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// `Built -> Signed`, and the bridge request `SourceFinalized ->
    /// SettlementAuthorized`: for the Solana->Goldcoin direction, the
    /// threshold of vault-signer partials assembling into a valid signed
    /// transaction IS the settlement authorization (docs/03-architecture.md
    /// — there is no separate Goldcoin-side attestation step, since
    /// Goldcoin has no program layer to attest to).
    pub fn record_goldcoin_payout_signed(
        &mut self,
        request_id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let n = tx.execute(
            "UPDATE goldcoin_payouts SET signed_tx_hex = ?1, state = 'Signed', signed_at = ?2 WHERE request_id = ?3 AND state = 'Built'",
            rusqlite::params![signed_tx_hex, now, request_id],
        )?;
        if n == 0 {
            tx.rollback()?;
            return Err(LedgerError::PayoutNotFound(request_id));
        }
        let state: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        assert_eq!(
            state,
            RequestState::SourceFinalized,
            "record_goldcoin_payout_signed on unexpected bridge_request state"
        );
        tx.execute(
            "UPDATE bridge_requests SET state = 'SettlementAuthorized' WHERE id = ?1",
            [request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(state),
            RequestState::SettlementAuthorized,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Signed -> Broadcast`, `SettlementAuthorized -> DestinationSubmitted`.
    /// Idempotent: broadcasting the identical already-broadcast tx again
    /// (e.g. after a restart) is a no-op, matching the RPC client's own
    /// idempotent-broadcast normalization.
    pub fn record_goldcoin_payout_broadcast(
        &mut self,
        request_id: i64,
        txid: [u8; 32],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let current_state: Option<String> = tx
            .query_row(
                "SELECT state FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        match current_state.as_deref() {
            None => {
                tx.rollback()?;
                return Err(LedgerError::PayoutNotFound(request_id));
            }
            Some("Broadcast") | Some("Confirmed") | Some("Completed") => {
                tx.rollback()?;
                return Ok(()); // already broadcast — idempotent no-op
            }
            Some("Signed") => {}
            Some(other) => {
                panic!("record_goldcoin_payout_broadcast on unexpected payout state {other}")
            }
        }
        tx.execute(
            "UPDATE goldcoin_payouts SET state = 'Broadcast', txid = ?1, broadcast_at = ?2 WHERE request_id = ?3",
            rusqlite::params![txid.as_slice(), now, request_id],
        )?;
        // AUTHORITATIVE change provenance for the 0-conf-spendability
        // policy (schema.rs apply_v14): the payout transaction's outputs
        // are [destination, change_0, change_1, ...] in
        // `goldcoin_payout_change_outputs` order (goldcoin::payout's
        // documented layout), so the change outpoints are exactly
        // (txid, 1..=n). Recorded in this same transaction as the
        // broadcast fact itself, so provenance can never lag the txid and
        // survives restart. `unconfirmed_ancestor_depth` = 1 + the
        // deepest still-unconfirmed zero-conf change input this payout
        // consumed (0-conf chain length through this service's OWN
        // payouts; an upper bound — ancestors confirming later only makes
        // reality shallower, never deeper).
        {
            let parent_depth: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(o.unconfirmed_ancestor_depth), 0)
                       FROM goldcoin_payout_inputs i
                       JOIN goldcoin_payout_change_outpoints o
                         ON o.txid = i.txid AND o.vout = i.vout
                       JOIN goldcoin_payouts p ON p.request_id = o.request_id
                      WHERE i.request_id = ?1 AND p.confirmations = 0",
                    [request_id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let change_amounts: Vec<i64> = {
                let mut stmt = tx.prepare(
                    "SELECT amount_atomic FROM goldcoin_payout_change_outputs
                      WHERE request_id = ?1 ORDER BY output_order",
                )?;
                let rows = stmt
                    .query_map([request_id], |r| r.get(0))?
                    .collect::<Result<Vec<i64>, _>>()?;
                rows
            };
            let change_amounts = if change_amounts.is_empty() {
                // Legacy single-change payout (pre-v12 row shape): the
                // synthesized one-output view `get_goldcoin_payout_full`
                // documents, applied identically here.
                let change_atomic: i64 = tx.query_row(
                    "SELECT change_atomic FROM goldcoin_payouts WHERE request_id = ?1",
                    [request_id],
                    |r| r.get(0),
                )?;
                if change_atomic > 0 {
                    vec![change_atomic]
                } else {
                    Vec::new()
                }
            } else {
                change_amounts
            };
            for (i, amount) in change_amounts.iter().enumerate() {
                tx.execute(
                    "INSERT OR IGNORE INTO goldcoin_payout_change_outpoints
                        (txid, vout, request_id, amount_atomic, unconfirmed_ancestor_depth)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![
                        txid.as_slice(),
                        (i + 1) as i64,
                        request_id,
                        amount,
                        parent_depth + 1
                    ],
                )?;
            }
        }
        let bstate: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        assert_eq!(
            bstate,
            RequestState::SettlementAuthorized,
            "record_goldcoin_payout_broadcast on unexpected bridge_request state"
        );
        tx.execute("UPDATE bridge_requests SET state = 'DestinationSubmitted', destination_txid = ?1 WHERE id = ?2", rusqlite::params![txid.as_slice(), request_id])?;
        log_transition(
            &tx,
            request_id,
            Some(bstate),
            RequestState::DestinationSubmitted,
            now,
            None,
            "system",
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Full read of an existing `goldcoin_payouts` row — everything
    /// [`crate::goldcoin::payout_recovery`] needs to independently
    /// reconstruct and re-verify the exact plan a stuck payout was
    /// originally built from, without selecting anything new. Distinct
    /// from [`Ledger::get_goldcoin_payout`] (which returns only the
    /// narrower [`GoldcoinPayoutSnapshot`] an attestation signer needs)
    /// so that read's shape/call sites are unaffected by this one.
    pub fn get_goldcoin_payout_full(
        &self,
        request_id: i64,
    ) -> Result<Option<GoldcoinPayoutFull>, LedgerError> {
        let row = self
            .conn
            .query_row(
                "SELECT commitment_hash, payout_atomic, change_atomic, fee_atomic,
                        dest_p2pkh_hash, unsigned_tx_hex, signed_tx_hex, state
                 FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| {
                    let commitment_hash: Vec<u8> = r.get(0)?;
                    let dest_p2pkh_hash: Vec<u8> = r.get(4)?;
                    Ok(GoldcoinPayoutFull {
                        commitment_hash: to_array32(&commitment_hash),
                        payout_atomic: r.get::<_, i64>(1)? as u64,
                        change_atomic: r.get::<_, i64>(2)? as u64,
                        change_outputs: Vec::new(), // filled in below
                        fee_atomic: r.get::<_, i64>(3)? as u64,
                        dest_p2pkh_hash: dest_p2pkh_hash.try_into().unwrap(),
                        unsigned_tx_hex: r.get(5)?,
                        signed_tx_hex: r.get(6)?,
                        state: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(LedgerError::from)?;
        let Some(mut payout) = row else {
            return Ok(None);
        };
        payout.change_outputs =
            self.goldcoin_payout_change_outputs(request_id, payout.change_atomic)?;
        Ok(Some(payout))
    }

    /// The deterministic change-output breakdown for `request_id`'s
    /// payout, from `goldcoin_payout_change_outputs` in construction
    /// order — or, if that table has no rows for it (a payout built before
    /// schema v12 introduced fan-out), a single synthesized legacy output
    /// equal to `legacy_change_atomic` (empty if that's `0`). Never
    /// backfills the table itself.
    fn goldcoin_payout_change_outputs(
        &self,
        request_id: i64,
        legacy_change_atomic: u64,
    ) -> Result<Vec<u64>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT amount_atomic FROM goldcoin_payout_change_outputs
             WHERE request_id = ?1 ORDER BY output_order ASC",
        )?;
        let rows: Vec<u64> = stmt
            .query_map([request_id], |r| Ok(r.get::<_, i64>(0)? as u64))?
            .collect::<Result<_, _>>()?;
        if !rows.is_empty() {
            return Ok(rows);
        }
        if legacy_change_atomic > 0 {
            Ok(vec![legacy_change_atomic])
        } else {
            Ok(Vec::new())
        }
    }

    /// The exact inputs an existing payout already reserved, in the exact
    /// order they were built with — never a fresh coin selection (those
    /// UTXOs are no longer `state = 'Available'` and so are structurally
    /// invisible to [`Ledger::available_vault_utxos`] regardless). Fails
    /// closed if any row's backing `vault_utxos` entry no longer reads
    /// `state = 'Reserved'` and `reserved_by = request_id` exactly as
    /// [`Ledger::reserve_vault_utxos`] left it — proof nothing about this
    /// reservation drifted between the original build and now.
    pub fn get_goldcoin_payout_inputs(
        &self,
        request_id: i64,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT i.txid, i.vout, i.amount_atomic, v.script_pubkey_hex, v.state, v.reserved_by
             FROM goldcoin_payout_inputs i
             JOIN vault_utxos v ON v.txid = i.txid AND v.vout = i.vout
             WHERE i.request_id = ?1
             ORDER BY i.input_order ASC",
        )?;
        let rows = stmt
            .query_map([request_id], |r| {
                let txid: Vec<u8> = r.get(0)?;
                let amount_atomic: i64 = r.get(2)?;
                let script_pubkey_hex: String = r.get(3)?;
                let state: String = r.get(4)?;
                let reserved_by: Option<i64> = r.get(5)?;
                Ok((
                    crate::goldcoin::coin::VaultUtxo {
                        txid: to_array32(&txid),
                        vout: r.get(1)?,
                        amount_atomic: amount_atomic as u64,
                        script_pubkey_hex,
                    },
                    state,
                    reserved_by,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            return Err(LedgerError::PayoutNotFound(request_id));
        }
        let mut utxos = Vec::with_capacity(rows.len());
        for (utxo, state, reserved_by) in rows {
            if state != "Reserved" || reserved_by != Some(request_id) {
                return Err(LedgerError::VaultUtxoReservationDrifted {
                    request_id,
                    txid: utxo.txid,
                    vout: utxo.vout,
                });
            }
            utxos.push(utxo);
        }
        Ok(utxos)
    }

    /// Updates a `Signed` payout's `signed_tx_hex` in place after an
    /// operator-triggered recovery re-signs it
    /// ([`crate::goldcoin::payout_recovery`]) — never changes `state`
    /// (still `Signed` either way) and never touches any other column, so
    /// this can never advance a payout that a concurrent process has
    /// already moved past `Signed`. Guarded to `state = 'Signed'` for the
    /// same reason [`Ledger::record_goldcoin_payout_signed`] guards to
    /// `state = 'Built'`: a mismatched row count means the precondition
    /// this caller checked has already changed underneath it.
    pub fn record_goldcoin_payout_resigned(
        &mut self,
        request_id: i64,
        signed_tx_hex: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let n = self.conn.execute(
            "UPDATE goldcoin_payouts SET signed_tx_hex = ?1, signed_at = ?2 WHERE request_id = ?3 AND state = 'Signed'",
            rusqlite::params![signed_tx_hex, now, request_id],
        )?;
        if n == 0 {
            return Err(LedgerError::PayoutNotFound(request_id));
        }
        Ok(())
    }

    /// Updates confirmation depth; at `required_depth` transitions
    /// `Broadcast -> Confirmed` and `DestinationSubmitted ->
    /// DestinationConfirmed`. Returns whether that transition fired on
    /// THIS call (so a caller refreshing an already-`Confirmed` payout
    /// can tell a re-poll apart from the actual confirmation event).
    /// Also mirrors the depth into the request's own
    /// `bridge_requests.destination_confirmations` — the column operators
    /// and the read-projections look at — for as long as the destination
    /// leg is live (`DestinationSubmitted`/`DestinationConfirmed`), not
    /// only until the transition. Both writes are monotonic (`<`-guarded)
    /// so a lagging RPC answer can never walk an observed depth
    /// backwards. Idempotent under repeated ticks. `tip_height`
    /// is the Goldcoin chain tip as observed by the caller at the same
    /// moment `confirmations` was read, used to back out the payout's
    /// mined height (`tip_height - confirmations + 1`) — recorded once,
    /// the first time `confirmations > 0`, since it never changes
    /// afterwards. This is the height threaded into
    /// `record_goldcoin_completion`'s attestation message
    /// (`shared::claim::goldcoin_completion_message`), so it must be the
    /// real mined height, not a derived/estimated one.
    pub fn update_goldcoin_payout_confirmations(
        &mut self,
        request_id: i64,
        confirmations: i64,
        tip_height: i64,
        required_depth: i64,
        now: i64,
    ) -> Result<bool, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "UPDATE goldcoin_payouts SET confirmations = ?1 WHERE request_id = ?2 AND state IN ('Broadcast','Confirmed') AND confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        tx.execute(
            "UPDATE bridge_requests SET destination_confirmations = ?1
                WHERE id = ?2 AND state IN ('DestinationSubmitted','DestinationConfirmed') AND destination_confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        if confirmations > 0 {
            tx.execute(
                "UPDATE goldcoin_payouts SET mined_height = ?1 WHERE request_id = ?2 AND mined_height IS NULL",
                rusqlite::params![tip_height - confirmations + 1, request_id],
            )?;
        }
        let mut transitioned = false;
        if confirmations >= required_depth {
            let n = tx.execute("UPDATE goldcoin_payouts SET state = 'Confirmed' WHERE request_id = ?1 AND state = 'Broadcast'", [request_id])?;
            if n > 0 {
                let bstate: RequestState = tx.query_row(
                    "SELECT state FROM bridge_requests WHERE id = ?1",
                    [request_id],
                    |r| r.get(0),
                )?;
                tx.execute(
                    "UPDATE bridge_requests SET state = 'DestinationConfirmed' WHERE id = ?1",
                    [request_id],
                )?;
                log_transition(
                    &tx,
                    request_id,
                    Some(bstate),
                    RequestState::DestinationConfirmed,
                    now,
                    None,
                    "system",
                )?;
                transitioned = true;
            }
        }
        tx.commit()?;
        Ok(transitioned)
    }

    /// Records that `record_goldcoin_completion` has been submitted to
    /// Solana for this request, carrying the transaction signature.
    /// Deliberately does not touch `bridge_requests.state` or
    /// `goldcoin_payouts.state` — the leg is only truly done once that
    /// submission is confirmed (see [`Ledger::mark_goldcoin_completion_confirmed`]).
    /// Latest-wins: a re-submission (the orchestrator re-sends the
    /// completion when an earlier submission's signature has demonstrably
    /// stopped being observable — dropped transaction / expired
    /// blockhash) REPLACES the recorded signature and timestamp, so the
    /// confirmation poller always tracks the newest in-flight attempt.
    /// The signature is a tracking handle, not the settlement fact — that
    /// fact is only ever established by observing the transaction (or the
    /// obligation's terminal on-chain status) succeed. Idempotent: a
    /// no-op if the request has already reached `Settled`.
    pub fn record_goldcoin_completion_submitted(
        &mut self,
        request_id: i64,
        signature: [u8; 64],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let payout_state: Option<String> = tx
            .query_row(
                "SELECT state FROM goldcoin_payouts WHERE request_id = ?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        match payout_state.as_deref() {
            None => {
                tx.rollback()?;
                return Err(LedgerError::PayoutNotFound(request_id));
            }
            Some("Completed") => {
                tx.rollback()?;
                return Ok(());
            }
            Some("Confirmed") => {}
            Some(other) => {
                panic!("record_goldcoin_completion_submitted on unexpected payout state {other}")
            }
        }
        tx.execute(
            "UPDATE goldcoin_payouts SET onchain_completion_signature = ?1, onchain_completion_submitted_at = ?2
                WHERE request_id = ?3",
            rusqlite::params![signature.as_slice(), now, request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Confirmed -> Completed`, `DestinationConfirmed -> Settled`, gated
    /// on the request's `record_goldcoin_completion` submission having
    /// been recorded first (constants.md/ADR-0018: the completion fact
    /// must be reconstructable from Solana chain state, so this service
    /// never declares a request `Settled` on the strength of its own
    /// database alone). Moves the amount out of
    /// `reserved_liquidity`/`pending_obligations` into
    /// `settled_liquidity_total` (docs/05-reserve-accounting.md) and
    /// spends the reserved vault UTXOs. Idempotent: a no-op if already
    /// `Settled`.
    pub fn mark_goldcoin_completion_confirmed(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let bstate: RequestState = tx.query_row(
            "SELECT state FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )?;
        if bstate == RequestState::Settled {
            tx.rollback()?;
            return Ok(());
        }
        assert_eq!(
            bstate,
            RequestState::DestinationConfirmed,
            "mark_goldcoin_completion_confirmed on unexpected bridge_request state"
        );
        let has_submission: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM goldcoin_payouts WHERE request_id = ?1 AND onchain_completion_signature IS NOT NULL",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        if has_submission.is_none() {
            tx.rollback()?;
            return Err(LedgerError::CompletionNotSubmitted(request_id));
        }
        let (amount, fee): (i64, i64) = tx.query_row(
            "SELECT net_destination_atomic, fee_amount_atomic FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;

        tx.execute("UPDATE goldcoin_payouts SET state = 'Completed', completed_at = ?1, onchain_completed_at = ?1 WHERE request_id = ?2", rusqlite::params![now, request_id])?;
        tx.execute(
            "UPDATE bridge_requests SET state = 'Settled', settled_at = ?1 WHERE id = ?2",
            rusqlite::params![now, request_id],
        )?;
        log_transition(
            &tx,
            request_id,
            Some(bstate),
            RequestState::Settled,
            now,
            None,
            "system",
        )?;
        // See the matching comment in `mark_release_confirmed`: keep the
        // cached balance self-consistent with a settlement this service
        // itself caused, so reconciliation never mistakes it for an
        // unexplained (and pause-triggering) breach.
        tx.execute(
            "UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity - ?1, pending_obligations = pending_obligations - ?1,
                settled_liquidity_total = settled_liquidity_total + ?1, total_reserve_balance = total_reserve_balance - ?1
                WHERE direction = 'GoldcoinReserve'",
            [amount],
        )?;
        // The fee for a SolToGlc settlement is collected on the SOURCE side
        // (Solana) — see the matching comment in `mark_release_confirmed`.
        // Always canonical units regardless of which row it's recorded on
        // (docs/20-bridge-fee.md) — never netted against SolanaReserve's
        // own native-unit balance/reserved/settled columns.
        tx.execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = accrued_fees_atomic + ?1
                WHERE direction = 'SolanaReserve'",
            [fee],
        )?;
        tx.execute(
            "UPDATE vault_utxos SET state = 'Spent' WHERE reserved_by = ?1",
            [request_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    // -------------------------------------------------------- audit trail --

    /// Freezes the exact canonical attestation-claim message bytes a
    /// signer group attested to, so an offline audit
    /// ([`crate::ops`]/`glc-audit`) can later re-verify self-consistency
    /// (does the stored hash still match the stored bytes?) and
    /// recompute-from-scalar-fields consistency (does re-deriving the
    /// message from this request's current data still produce the same
    /// bytes?) — not merely that a message is *re-derivable* today, which
    /// says nothing about whether the frozen record was tampered with.
    /// Idempotent: a no-op if a record already exists for
    /// `(request_id, action_type)` (this service only ever attests each
    /// action once per request — see `orchestrator::Orchestrator`).
    pub fn record_attestation(
        &mut self,
        request_id: i64,
        action_type: &str,
        message: &[u8],
        now: i64,
    ) -> Result<(), LedgerError> {
        let message_hash = Sha256::digest(message);
        self.conn.execute(
            "INSERT OR IGNORE INTO attestation_records (request_id, action_type, canonical_message, message_hash, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![request_id, action_type, message, message_hash.as_slice(), now],
        )?;
        Ok(())
    }

    /// SQLite's own consistency check — `"ok"` is the only passing result;
    /// anything else names actual corruption. What `glc-audit` runs first,
    /// before trusting anything else it reads.
    pub fn integrity_check(&self) -> Result<String, LedgerError> {
        self.conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .map_err(LedgerError::from)
    }

    /// All frozen attestation records, oldest first — what `glc-audit`
    /// walks to recompute-and-diff every one.
    pub fn all_attestation_records(&self) -> Result<Vec<AttestationRecord>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, request_id, action_type, canonical_message, message_hash, created_at
             FROM attestation_records ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AttestationRecord {
                    id: r.get(0)?,
                    request_id: r.get(1)?,
                    action_type: r.get(2)?,
                    canonical_message: r.get(3)?,
                    message_hash: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Appends a signer-identity audit entry (never key material — see
    /// docs/06-schema.md). Best-effort/observability only: never part of
    /// any settlement-safety invariant, so a failure here must never be
    /// allowed to block the action it's logging — callers should log and
    /// continue on error rather than propagate it into a settlement path.
    pub fn record_signature_grant(
        &mut self,
        action_type: &str,
        identity: &str,
        request_id: Option<i64>,
        severity: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.conn.execute(
            "INSERT INTO signature_grant_log (at, action_type, identity, request_id, severity)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![now, action_type, identity, request_id, severity],
        )?;
        Ok(())
    }

    // -------------------------------------------------------- rebalancing --
    //
    // Structurally separate from bridge_requests/settlement accounting
    // (docs/05-reserve-accounting.md, docs/22-production-readiness-review.md
    // P1 "rebalancing"): nothing below ever touches `reserved_liquidity`/
    // `pending_obligations`/`bridge_requests`, only `total_reserve_balance`
    // on the one named reserve, once a request reaches `Confirmed`. This
    // ledger tracks the REQUEST and its approval/execution/audit trail; it
    // never signs, constructs, or broadcasts a real fund-moving transaction
    // — `record_rebalance_executed` only ever records evidence
    // (`tx_reference`) of a transfer an operator already authorized and
    // executed through real custody tooling outside this system.

    /// Creates a new rebalance request in `Proposed`, collecting approvals
    /// from here. `reason` is mandatory (matching every other admin-action
    /// audit trail in this codebase — never a silent/blank justification
    /// for a reserve-balance change).
    #[allow(clippy::too_many_arguments)]
    pub fn propose_rebalance(
        &mut self,
        direction: ReserveDirection,
        kind: RebalanceKind,
        amount_atomic: u64,
        reason: &str,
        requested_by: &str,
        required_approvals: u32,
        now: i64,
    ) -> Result<i64, LedgerError> {
        if amount_atomic == 0 {
            return Err(LedgerError::InvalidRebalanceRequest(
                "amount_atomic must be > 0".to_string(),
            ));
        }
        if required_approvals == 0 {
            return Err(LedgerError::InvalidRebalanceRequest(
                "required_approvals must be > 0".to_string(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "reason must not be empty".to_string(),
            ));
        }
        if requested_by.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "requested_by must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "INSERT INTO rebalance_requests
                (direction, kind, amount_atomic, state, reason, requested_by, requested_at,
                 required_approvals, approved_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '[]')",
            rusqlite::params![
                direction,
                kind,
                amount_atomic as i64,
                RebalanceState::Proposed,
                reason,
                requested_by,
                now,
                required_approvals,
            ],
        )?;
        let id = tx.last_insert_rowid();
        log_rebalance_transition(
            &tx,
            id,
            None,
            RebalanceState::Proposed,
            now,
            Some(reason),
            requested_by,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Records `approver`'s approval. Idempotent per approver (approving
    /// twice does not double-count). Transitions `Proposed -> Approved`
    /// once `required_approvals` distinct identities have approved.
    pub fn approve_rebalance(
        &mut self,
        id: i64,
        approver: &str,
        now: i64,
    ) -> Result<RebalanceApprovalOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(RebalanceState, i64, String)> = tx
            .query_row(
                "SELECT state, required_approvals, approved_by FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, required, approved_json)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Proposed {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Proposed,
                actual: state,
            });
        }
        let mut approvers: Vec<String> = serde_json::from_str(&approved_json).unwrap_or_default();
        if !approvers.iter().any(|a| a == approver) {
            approvers.push(approver.to_string());
        }
        let approved_json =
            serde_json::to_string(&approvers).expect("Vec<String> always serializes");
        let reached = approvers.len() as u32 >= required as u32;
        if reached {
            tx.execute(
                "UPDATE rebalance_requests SET approved_by = ?1, state = ?2, approved_at = ?3 \
                 WHERE id = ?4",
                rusqlite::params![approved_json, RebalanceState::Approved, now, id],
            )?;
            log_rebalance_transition(
                &tx,
                id,
                Some(RebalanceState::Proposed),
                RebalanceState::Approved,
                now,
                Some(&format!(
                    "approval threshold reached ({}/{required})",
                    approvers.len()
                )),
                approver,
            )?;
        } else {
            tx.execute(
                "UPDATE rebalance_requests SET approved_by = ?1 WHERE id = ?2",
                rusqlite::params![approved_json, id],
            )?;
            log_rebalance_transition(
                &tx,
                id,
                Some(RebalanceState::Proposed),
                RebalanceState::Proposed,
                now,
                Some(&format!("approved ({}/{required})", approvers.len())),
                approver,
            )?;
        }
        tx.commit()?;
        Ok(if reached {
            RebalanceApprovalOutcome::ThresholdReached
        } else {
            RebalanceApprovalOutcome::Recorded {
                approvals: approvers.len() as u32,
                required: required as u32,
            }
        })
    }

    /// `Proposed -> Rejected`. Terminal; requires a note (an approver's
    /// reason for declining).
    pub fn reject_rebalance(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_rebalance(
            id,
            &[RebalanceState::Proposed],
            RebalanceState::Rejected,
            note,
            actor,
            now,
        )
    }

    /// `Proposed|Approved -> Cancelled`. Terminal; requires a note.
    pub fn cancel_rebalance(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_rebalance(
            id,
            &[RebalanceState::Proposed, RebalanceState::Approved],
            RebalanceState::Cancelled,
            note,
            actor,
            now,
        )
    }

    fn close_rebalance(
        &mut self,
        id: i64,
        allowed_from: &[RebalanceState],
        to: RebalanceState,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if note.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "a note is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<RebalanceState> = tx
            .query_row(
                "SELECT state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if !allowed_from.contains(&state) {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: allowed_from[0],
                actual: state,
            });
        }
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![to, id],
        )?;
        log_rebalance_transition(&tx, id, Some(state), to, now, Some(note), actor)?;
        tx.commit()?;
        Ok(())
    }

    /// `Approved -> Executed`: records evidence of a real, out-of-band
    /// transfer an operator already authorized and executed — never
    /// broadcasts or signs anything itself. `tx_reference` (a Goldcoin
    /// txid or Solana signature, as text) is UNIQUE across every rebalance
    /// request ever recorded (schema `ux_rebalance_tx_reference`), so
    /// recording the same real transfer twice is a structural, DB-enforced
    /// rejection — the replay guard for this action.
    pub fn record_rebalance_executed(
        &mut self,
        id: i64,
        tx_reference: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if tx_reference.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "tx_reference must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<RebalanceState> = tx
            .query_row(
                "SELECT state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Approved {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Approved,
                actual: state,
            });
        }
        // A duplicate tx_reference fails here on the UNIQUE index —
        // propagated as LedgerError::Sqlite, fail-closed by construction,
        // not by an application-level check that could be forgotten.
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1, tx_reference = ?2, executed_at = ?3 \
             WHERE id = ?4",
            rusqlite::params![RebalanceState::Executed, tx_reference, now, id],
        )?;
        log_rebalance_transition(
            &tx,
            id,
            Some(RebalanceState::Approved),
            RebalanceState::Executed,
            now,
            Some(&format!("tx_reference={tx_reference}")),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Confirmed`: an operator (or an automated reconciliation
    /// cross-check) independently confirms the real balance change the
    /// executed transfer produced. Adjusts `reserve_ledger.
    /// total_reserve_balance` by the observed amount in the same
    /// transaction — mirroring `mark_release_confirmed`'s "keep the cache
    /// self-consistent with a change this service itself caused" rationale
    /// (docs/14-phase6-checkpoint.md bug 3) — so the very next
    /// reconciliation tick sees an already-explained balance rather than
    /// misclassifying an operator-authorized rebalance as an unexplained
    /// breach.
    pub fn confirm_rebalance(
        &mut self,
        id: i64,
        observed_amount_atomic: u64,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(ReserveDirection, RebalanceKind, RebalanceState)> = tx
            .query_row(
                "SELECT direction, kind, state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((direction, kind, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Executed {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1, observed_amount_atomic = ?2, \
             confirmed_at = ?3 WHERE id = ?4",
            rusqlite::params![
                RebalanceState::Confirmed,
                observed_amount_atomic as i64,
                now,
                id
            ],
        )?;
        let delta: i64 = match kind {
            RebalanceKind::Deposit => observed_amount_atomic as i64,
            RebalanceKind::Withdraw => -(observed_amount_atomic as i64),
        };
        tx.execute(
            "UPDATE reserve_ledger SET total_reserve_balance = total_reserve_balance + ?1, \
             balance_refreshed_at = ?2 WHERE direction = ?3",
            rusqlite::params![delta, now, direction],
        )?;
        log_rebalance_transition(
            &tx,
            id,
            Some(RebalanceState::Executed),
            RebalanceState::Confirmed,
            now,
            Some(&format!("observed_amount_atomic={observed_amount_atomic}")),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Failed`: the recorded transfer's expected effect was
    /// never confirmed (or was confirmed wrong) — routed to a state
    /// requiring operator resolution rather than left `Executed` forever,
    /// same discipline as `RequestState::ManualReview`. Deliberately does
    /// NOT touch `total_reserve_balance` — nothing was confirmed to have
    /// happened, so there is nothing to explain away.
    pub fn fail_rebalance(
        &mut self,
        id: i64,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidRebalanceRequest(
                "a failure reason is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<RebalanceState> = tx
            .query_row(
                "SELECT state FROM rebalance_requests WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::RebalanceNotFound(id));
        };
        if state != RebalanceState::Executed {
            tx.rollback()?;
            return Err(LedgerError::RebalanceWrongState {
                id,
                expected: RebalanceState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE rebalance_requests SET state = ?1, failure_reason = ?2 WHERE id = ?3",
            rusqlite::params![RebalanceState::Failed, reason, id],
        )?;
        log_rebalance_transition(
            &tx,
            id,
            Some(RebalanceState::Executed),
            RebalanceState::Failed,
            now,
            Some(reason),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_rebalance(&self, id: i64) -> Result<Option<RebalanceRequest>, LedgerError> {
        self.conn
            .query_row(REBALANCE_SELECT_BY_ID, [id], row_to_rebalance)
            .optional()
            .map_err(LedgerError::from)
    }

    /// All rebalance requests for `direction` (or every direction, if
    /// `None`), optionally restricted to still-open ones
    /// (`RebalanceState::is_open`), newest first.
    pub fn list_rebalances(
        &self,
        direction: Option<ReserveDirection>,
        open_only: bool,
    ) -> Result<Vec<RebalanceRequest>, LedgerError> {
        let mut stmt = self.conn.prepare(REBALANCE_SELECT_ALL)?;
        let rows = stmt
            .query_map([], row_to_rebalance)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|r| direction.is_none() || direction == Some(r.direction))
            .filter(|r| !open_only || r.state.is_open())
            .collect())
    }

    pub fn rebalance_state_log(&self, id: i64) -> Result<Vec<RebalanceStateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason, actor FROM rebalance_state_log \
             WHERE rebalance_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ----------------------------------------------- custody transitions --
    //
    // Generic key-rotation / vault-sweep tooling
    // (docs/22-production-readiness-review.md P1 "key rotation / vault
    // sweep tooling"). Shares the rebalancing state machine's core
    // discipline — this ledger only ever tracks the REQUEST, its
    // approvals, and its audit trail; `record_custody_transition_executed`
    // only ever records evidence (`tx_reference`) of a rotation/sweep an
    // operator already authorized and executed through real custody
    // tooling outside this system — plus two extra gates rebalancing
    // doesn't need: the new identity must be independently verified
    // before any approval can begin, and the relevant reserve(s) must
    // already be paused before execution evidence can be recorded.

    /// Creates a new custody transition in `Proposed`. `new_threshold`
    /// only applies to `CustodyTransitionKind::GoldcoinVaultSweep`; must
    /// be `None` for `AttestationKeyRotation`, which has no threshold
    /// concept.
    #[allow(clippy::too_many_arguments)]
    pub fn propose_custody_transition(
        &mut self,
        kind: CustodyTransitionKind,
        old_identities: &[String],
        new_identities: &[String],
        new_threshold: Option<u32>,
        reason: &str,
        requested_by: &str,
        required_approvals: u32,
        now: i64,
    ) -> Result<i64, LedgerError> {
        if new_identities.is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "new_identities must not be empty".to_string(),
            ));
        }
        if kind == CustodyTransitionKind::AttestationKeyRotation && new_threshold.is_some() {
            return Err(LedgerError::InvalidCustodyTransition(
                "new_threshold does not apply to AttestationKeyRotation".to_string(),
            ));
        }
        if required_approvals == 0 {
            return Err(LedgerError::InvalidCustodyTransition(
                "required_approvals must be > 0".to_string(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "reason must not be empty".to_string(),
            ));
        }
        if requested_by.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "requested_by must not be empty".to_string(),
            ));
        }
        let old_json =
            serde_json::to_string(old_identities).expect("Vec<String> always serializes");
        let new_json =
            serde_json::to_string(new_identities).expect("Vec<String> always serializes");
        let tx = write_tx(&mut self.conn)?;
        tx.execute(
            "INSERT INTO custody_transitions
                (kind, state, old_identities, new_identities, new_threshold, reason,
                 requested_by, requested_at, required_approvals, approved_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, '[]')",
            rusqlite::params![
                kind,
                CustodyTransitionState::Proposed,
                old_json,
                new_json,
                new_threshold,
                reason,
                requested_by,
                now,
                required_approvals,
            ],
        )?;
        let id = tx.last_insert_rowid();
        log_custody_transition(
            &tx,
            id,
            None,
            CustodyTransitionState::Proposed,
            now,
            Some(reason),
            requested_by,
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// `Proposed -> IdentityVerified`: records that `verifier`
    /// independently checked the claimed new identity (e.g. a signed
    /// challenge against the claimed pubkey/vault descriptor) before any
    /// approval may begin. Required gate, not advisory — `approve_
    /// custody_transition` rejects anything still in `Proposed`.
    pub fn verify_new_identity(
        &mut self,
        id: i64,
        verifier: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if verifier.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "verifier must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Proposed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Proposed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, identity_verified_by = ?2, \
             identity_verified_at = ?3 WHERE id = ?4",
            rusqlite::params![CustodyTransitionState::IdentityVerified, verifier, now, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Proposed),
            CustodyTransitionState::IdentityVerified,
            now,
            Some("new identity independently verified"),
            verifier,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Records `approver`'s approval. Idempotent per approver. Only valid
    /// once the new identity has been verified (`IdentityVerified`).
    /// Transitions `IdentityVerified -> Approved` once
    /// `required_approvals` distinct identities have approved.
    pub fn approve_custody_transition(
        &mut self,
        id: i64,
        approver: &str,
        now: i64,
    ) -> Result<CustodyApprovalOutcome, LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(CustodyTransitionState, i64, String)> = tx
            .query_row(
                "SELECT state, required_approvals, approved_by FROM custody_transitions \
                 WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, required, approved_json)) = row else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::IdentityVerified {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::IdentityVerified,
                actual: state,
            });
        }
        let mut approvers: Vec<String> = serde_json::from_str(&approved_json).unwrap_or_default();
        if !approvers.iter().any(|a| a == approver) {
            approvers.push(approver.to_string());
        }
        let approved_json =
            serde_json::to_string(&approvers).expect("Vec<String> always serializes");
        let reached = approvers.len() as u32 >= required as u32;
        if reached {
            tx.execute(
                "UPDATE custody_transitions SET approved_by = ?1, state = ?2, approved_at = ?3 \
                 WHERE id = ?4",
                rusqlite::params![approved_json, CustodyTransitionState::Approved, now, id],
            )?;
            log_custody_transition(
                &tx,
                id,
                Some(CustodyTransitionState::IdentityVerified),
                CustodyTransitionState::Approved,
                now,
                Some(&format!(
                    "approval threshold reached ({}/{required})",
                    approvers.len()
                )),
                approver,
            )?;
        } else {
            tx.execute(
                "UPDATE custody_transitions SET approved_by = ?1 WHERE id = ?2",
                rusqlite::params![approved_json, id],
            )?;
            log_custody_transition(
                &tx,
                id,
                Some(CustodyTransitionState::IdentityVerified),
                CustodyTransitionState::IdentityVerified,
                now,
                Some(&format!("approved ({}/{required})", approvers.len())),
                approver,
            )?;
        }
        tx.commit()?;
        Ok(if reached {
            CustodyApprovalOutcome::ThresholdReached
        } else {
            CustodyApprovalOutcome::Recorded {
                approvals: approvers.len() as u32,
                required: required as u32,
            }
        })
    }

    /// `Proposed|IdentityVerified -> Rejected`. Terminal; requires a note.
    pub fn reject_custody_transition(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_custody_transition(
            id,
            &[
                CustodyTransitionState::Proposed,
                CustodyTransitionState::IdentityVerified,
            ],
            CustodyTransitionState::Rejected,
            note,
            actor,
            now,
        )
    }

    /// `Proposed|IdentityVerified|Approved -> Cancelled`. Terminal;
    /// requires a note.
    pub fn cancel_custody_transition(
        &mut self,
        id: i64,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        self.close_custody_transition(
            id,
            &[
                CustodyTransitionState::Proposed,
                CustodyTransitionState::IdentityVerified,
                CustodyTransitionState::Approved,
            ],
            CustodyTransitionState::Cancelled,
            note,
            actor,
            now,
        )
    }

    fn close_custody_transition(
        &mut self,
        id: i64,
        allowed_from: &[CustodyTransitionState],
        to: CustodyTransitionState,
        note: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if note.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "a note is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if !allowed_from.contains(&state) {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: allowed_from[0],
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1 WHERE id = ?2",
            rusqlite::params![to, id],
        )?;
        log_custody_transition(&tx, id, Some(state), to, now, Some(note), actor)?;
        tx.commit()?;
        Ok(())
    }

    /// `Approved -> Executed`: records evidence of a real, out-of-band
    /// rotation/sweep an operator already authorized and executed —
    /// never performs the rotation/sweep itself. Enforces the "pause
    /// requirements" invariant: the relevant reserve(s) must already be
    /// paused (`GoldcoinReserve` for `GoldcoinVaultSweep`; BOTH reserves
    /// for `AttestationKeyRotation`, since attestation authorizes both
    /// bridge directions) — an actual precondition, not documentation.
    /// `tx_reference` is UNIQUE across every custody transition ever
    /// recorded (schema `ux_custody_transitions_tx_reference`), the
    /// structural replay guard for this action.
    pub fn record_custody_transition_executed(
        &mut self,
        id: i64,
        tx_reference: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if tx_reference.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "tx_reference must not be empty".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let row: Option<(CustodyTransitionKind, CustodyTransitionState)> = tx
            .query_row(
                "SELECT kind, state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((kind, state)) = row else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Approved {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Approved,
                actual: state,
            });
        }
        let required_paused: &[ReserveDirection] = match kind {
            CustodyTransitionKind::GoldcoinVaultSweep => &[ReserveDirection::GoldcoinReserve],
            CustodyTransitionKind::AttestationKeyRotation => &[
                ReserveDirection::GoldcoinReserve,
                ReserveDirection::SolanaReserve,
            ],
        };
        for direction in required_paused {
            let paused: i64 = tx.query_row(
                "SELECT paused FROM reserve_ledger WHERE direction = ?1",
                [direction],
                |r| r.get(0),
            )?;
            if paused == 0 {
                tx.rollback()?;
                return Err(LedgerError::CustodyTransitionRequiresPause {
                    id,
                    direction: *direction,
                });
            }
        }
        // A duplicate tx_reference fails here on the UNIQUE index —
        // propagated as LedgerError::Sqlite, fail-closed by construction.
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, tx_reference = ?2, executed_at = ?3 \
             WHERE id = ?4",
            rusqlite::params![CustodyTransitionState::Executed, tx_reference, now, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Approved),
            CustodyTransitionState::Executed,
            now,
            Some(&format!("tx_reference={tx_reference}")),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Confirmed`: an operator independently confirms the
    /// new custody identity is active and correct post-transition —
    /// terminal success. Deliberately does not touch reserve pause state
    /// or balance; unpausing after a rotation/sweep is a distinct,
    /// deliberate operator action (`set_paused`), never automatic.
    pub fn confirm_custody_transition(
        &mut self,
        id: i64,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Executed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, confirmed_at = ?2 WHERE id = ?3",
            rusqlite::params![CustodyTransitionState::Confirmed, now, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Executed),
            CustodyTransitionState::Confirmed,
            now,
            Some("new custody identity confirmed active"),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Executed -> Failed`: the recorded rotation/sweep's expected new
    /// identity was never confirmed (or confirmed wrong) — requires
    /// operator resolution rather than left `Executed` forever.
    pub fn fail_custody_transition(
        &mut self,
        id: i64,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "a failure reason is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Executed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Executed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, failure_reason = ?2 WHERE id = ?3",
            rusqlite::params![CustodyTransitionState::Failed, reason, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Executed),
            CustodyTransitionState::Failed,
            now,
            Some(reason),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// `Failed -> RolledBack`: records that a failed transition's
    /// real-world effect was reverted back to the old identity, out of
    /// band. Only ever an audit marker of a rollback already performed —
    /// this service never performs the rollback itself.
    pub fn rollback_custody_transition(
        &mut self,
        id: i64,
        reason: &str,
        actor: &str,
        now: i64,
    ) -> Result<(), LedgerError> {
        if reason.trim().is_empty() {
            return Err(LedgerError::InvalidCustodyTransition(
                "a rollback reason is required".to_string(),
            ));
        }
        let tx = write_tx(&mut self.conn)?;
        let state: Option<CustodyTransitionState> = tx
            .query_row(
                "SELECT state FROM custody_transitions WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionNotFound(id));
        };
        if state != CustodyTransitionState::Failed {
            tx.rollback()?;
            return Err(LedgerError::CustodyTransitionWrongState {
                id,
                expected: CustodyTransitionState::Failed,
                actual: state,
            });
        }
        tx.execute(
            "UPDATE custody_transitions SET state = ?1, rolled_back_at = ?2, rollback_reason = ?3 \
             WHERE id = ?4",
            rusqlite::params![CustodyTransitionState::RolledBack, now, reason, id],
        )?;
        log_custody_transition(
            &tx,
            id,
            Some(CustodyTransitionState::Failed),
            CustodyTransitionState::RolledBack,
            now,
            Some(reason),
            actor,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_custody_transition(
        &self,
        id: i64,
    ) -> Result<Option<CustodyTransition>, LedgerError> {
        self.conn
            .query_row(CUSTODY_SELECT_BY_ID, [id], row_to_custody_transition)
            .optional()
            .map_err(LedgerError::from)
    }

    /// All custody transitions for `kind` (or every kind, if `None`),
    /// optionally restricted to still-open ones
    /// (`CustodyTransitionState::is_open`), newest first.
    pub fn list_custody_transitions(
        &self,
        kind: Option<CustodyTransitionKind>,
        open_only: bool,
    ) -> Result<Vec<CustodyTransition>, LedgerError> {
        let mut stmt = self.conn.prepare(CUSTODY_SELECT_ALL)?;
        let rows = stmt
            .query_map([], row_to_custody_transition)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter(|r| kind.is_none() || kind == Some(r.kind))
            .filter(|r| !open_only || r.state.is_open())
            .collect())
    }

    pub fn custody_transition_state_log(
        &self,
        id: i64,
    ) -> Result<Vec<CustodyTransitionStateLogEntry>, LedgerError> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, reason, actor FROM custody_transition_state_log \
             WHERE transition_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map([id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

/// One row of `reconciliation_findings`. See
/// [`Ledger::reconciliation_findings_page`].
#[derive(Debug, Clone)]
pub struct ReconciliationFindingRow {
    pub id: i64,
    pub direction: ReserveDirection,
    pub detected_at: i64,
    pub expected: i64,
    pub observed: i64,
    pub delta: i64,
    pub classification: String,
    pub auto_paused: bool,
}

/// One row of `bridge_request_state_log`, joined to its request's
/// direction. See [`Ledger::explorer_events_page`].
#[derive(Debug, Clone)]
pub struct ExplorerEventRow {
    pub id: i64,
    pub request_id: i64,
    pub direction: Direction,
    pub from_state: Option<RequestState>,
    pub to_state: RequestState,
    pub at: i64,
    pub reason: Option<String>,
}

/// See [`Ledger::all_attestation_records`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttestationRecord {
    pub id: i64,
    pub request_id: i64,
    pub action_type: String,
    pub canonical_message: Vec<u8>,
    pub message_hash: Vec<u8>,
    pub created_at: i64,
}

const SELECT_REQUEST_PREFIX: &str =
    "SELECT id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic, \
    net_amount_atomic, net_destination_atomic, recipient, requester, \
    created_at, reserved_at, reservation_expires_at, source_txid, source_vout, \
    source_obligation_index, source_block_height, source_block_hash, source_confirmations, \
    source_finalized_at, failure_reason, manual_review_note FROM bridge_requests";
const SELECT_REQUEST: &str =
    "SELECT id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic, \
    net_amount_atomic, net_destination_atomic, recipient, requester, \
    created_at, reserved_at, reservation_expires_at, source_txid, source_vout, \
    source_obligation_index, source_block_height, source_block_hash, source_confirmations, \
    source_finalized_at, failure_reason, manual_review_note FROM bridge_requests WHERE id = ?1";

fn row_to_request(r: &rusqlite::Row) -> rusqlite::Result<BridgeRequest> {
    let recipient_vec: Vec<u8> = r.get(8)?;
    let requester_vec: Option<Vec<u8>> = r.get(9)?;
    let source_txid_vec: Option<Vec<u8>> = r.get(13)?;
    let source_block_hash_vec: Option<Vec<u8>> = r.get(17)?;
    Ok(BridgeRequest {
        id: r.get(0)?,
        direction: r.get(1)?,
        state: r.get(2)?,
        gross_amount_atomic: r.get::<_, i64>(3)? as u64,
        fee_bps: r.get::<_, i64>(4)? as u64,
        fee_amount_atomic: r.get::<_, i64>(5)? as u64,
        net_amount_atomic: r.get::<_, i64>(6)? as u64,
        net_destination_atomic: r.get::<_, i64>(7)? as u64,
        recipient: recipient_vec,
        requester: requester_vec.map(|v| to_array32(&v)),
        created_at: r.get(10)?,
        reserved_at: r.get(11)?,
        reservation_expires_at: r.get(12)?,
        source_txid: source_txid_vec.map(|v| to_array32(&v)),
        source_vout: r.get::<_, Option<i64>>(14)?.map(|v| v as u32),
        source_obligation_index: r.get::<_, Option<i64>>(15)?.map(|v| v as u64),
        source_block_height: r.get(16)?,
        source_block_hash: source_block_hash_vec.map(|v| to_array32(&v)),
        source_confirmations: r.get(18)?,
        source_finalized_at: r.get(19)?,
        failure_reason: r.get(20)?,
        manual_review_note: r.get(21)?,
    })
}

fn to_array32(v: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let n = v.len().min(32);
    out[..n].copy_from_slice(&v[..n]);
    out
}

fn log_transition(
    conn: &Connection,
    request_id: i64,
    from: Option<RequestState>,
    to: RequestState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            request_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

fn log_rebalance_transition(
    conn: &Connection,
    rebalance_id: i64,
    from: Option<RebalanceState>,
    to: RebalanceState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO rebalance_state_log (rebalance_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            rebalance_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

const REBALANCE_SELECT_BY_ID: &str = "SELECT id, direction, kind, amount_atomic, state, reason, \
     requested_by, requested_at, required_approvals, approved_by, approved_at, tx_reference, \
     executed_at, observed_amount_atomic, confirmed_at, failure_reason \
     FROM rebalance_requests WHERE id = ?1";

const REBALANCE_SELECT_ALL: &str = "SELECT id, direction, kind, amount_atomic, state, reason, \
     requested_by, requested_at, required_approvals, approved_by, approved_at, tx_reference, \
     executed_at, observed_amount_atomic, confirmed_at, failure_reason \
     FROM rebalance_requests ORDER BY id DESC";

fn row_to_rebalance(r: &rusqlite::Row) -> rusqlite::Result<RebalanceRequest> {
    let approved_by_json: String = r.get(9)?;
    let approved_by: Vec<String> = serde_json::from_str(&approved_by_json).unwrap_or_default();
    Ok(RebalanceRequest {
        id: r.get(0)?,
        direction: r.get(1)?,
        kind: r.get(2)?,
        amount_atomic: r.get::<_, i64>(3)? as u64,
        state: r.get(4)?,
        reason: r.get(5)?,
        requested_by: r.get(6)?,
        requested_at: r.get(7)?,
        required_approvals: r.get::<_, i64>(8)? as u32,
        approved_by,
        approved_at: r.get(10)?,
        tx_reference: r.get(11)?,
        executed_at: r.get(12)?,
        observed_amount_atomic: r.get::<_, Option<i64>>(13)?.map(|v| v as u64),
        confirmed_at: r.get(14)?,
        failure_reason: r.get(15)?,
    })
}

fn log_custody_transition(
    conn: &Connection,
    transition_id: i64,
    from: Option<CustodyTransitionState>,
    to: CustodyTransitionState,
    at: i64,
    reason: Option<&str>,
    actor: &str,
) -> Result<(), LedgerError> {
    conn.execute(
        "INSERT INTO custody_transition_state_log
            (transition_id, from_state, to_state, at, reason, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            transition_id,
            from.map(|s| s.as_str()),
            to.as_str(),
            at,
            reason,
            actor
        ],
    )?;
    Ok(())
}

const CUSTODY_SELECT_BY_ID: &str = "SELECT id, kind, state, old_identities, new_identities, \
     new_threshold, reason, requested_by, requested_at, required_approvals, approved_by, \
     approved_at, identity_verified_by, identity_verified_at, tx_reference, executed_at, \
     confirmed_at, failure_reason, rolled_back_at, rollback_reason \
     FROM custody_transitions WHERE id = ?1";

const CUSTODY_SELECT_ALL: &str = "SELECT id, kind, state, old_identities, new_identities, \
     new_threshold, reason, requested_by, requested_at, required_approvals, approved_by, \
     approved_at, identity_verified_by, identity_verified_at, tx_reference, executed_at, \
     confirmed_at, failure_reason, rolled_back_at, rollback_reason \
     FROM custody_transitions ORDER BY id DESC";

fn row_to_custody_transition(r: &rusqlite::Row) -> rusqlite::Result<CustodyTransition> {
    let old_identities_json: String = r.get(3)?;
    let old_identities: Vec<String> =
        serde_json::from_str(&old_identities_json).unwrap_or_default();
    let new_identities_json: String = r.get(4)?;
    let new_identities: Vec<String> =
        serde_json::from_str(&new_identities_json).unwrap_or_default();
    let approved_by_json: String = r.get(10)?;
    let approved_by: Vec<String> = serde_json::from_str(&approved_by_json).unwrap_or_default();
    Ok(CustodyTransition {
        id: r.get(0)?,
        kind: r.get(1)?,
        state: r.get(2)?,
        old_identities,
        new_identities,
        new_threshold: r.get::<_, Option<i64>>(5)?.map(|v| v as u32),
        reason: r.get(6)?,
        requested_by: r.get(7)?,
        requested_at: r.get(8)?,
        required_approvals: r.get::<_, i64>(9)? as u32,
        approved_by,
        approved_at: r.get(11)?,
        identity_verified_by: r.get(12)?,
        identity_verified_at: r.get(13)?,
        tx_reference: r.get(14)?,
        executed_at: r.get(15)?,
        confirmed_at: r.get(16)?,
        failure_reason: r.get(17)?,
        rolled_back_at: r.get(18)?,
        rollback_reason: r.get(19)?,
    })
}

#[cfg(test)]
mod tests;
