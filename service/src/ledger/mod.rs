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
    BridgeRequest, CustodyTransition, CustodyTransitionKind, CustodyTransitionState, Direction,
    RebalanceKind, RebalanceRequest, RebalanceState, RequestAmounts, RequestState,
    ReserveDirection,
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
}

pub struct Ledger {
    conn: Connection,
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
                rusqlite::Error::QueryReturnedNoRows => {
                    LedgerError::Sqlite(rusqlite::Error::QueryReturnedNoRows)
                }
                other => LedgerError::Sqlite(other),
            })?;
        Ok(paused != 0)
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(Direction, RequestState, i64, Option<Vec<u8>>)> = tx
            .query_row(
                "SELECT direction, state, gross_amount_atomic, source_txid FROM bridge_requests WHERE id = ?1",
                [request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((direction, state, reserved_amount, existing_txid)) = row else {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::NoMatchingRequest);
        };

        if (state == RequestState::DepositObserved || state == RequestState::Confirming)
            && existing_txid.as_deref() == Some(txid.as_slice())
        {
            tx.rollback()?;
            return Ok(GlcObservationOutcome::AlreadyRecorded);
        }
        if direction != Direction::GlcToSol || state != RequestState::AwaitingDeposit {
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
        Ok(GlcObservationOutcome::Recorded)
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
        self.conn.execute(
            "CREATE TABLE IF NOT EXISTS unmatched_goldcoin_deposits (
                id INTEGER PRIMARY KEY, txid BLOB NOT NULL, vout INTEGER NOT NULL,
                amount_atomic INTEGER NOT NULL, block_height INTEGER NOT NULL,
                reason TEXT NOT NULL, discovered_at INTEGER NOT NULL,
                UNIQUE(txid, vout)
             )",
            [],
        )?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

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
        let paused: i64 = tx.query_row(
            "SELECT paused FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| r.get(0),
        )?;
        let (balance, protected_minimum, reserved): (i64, i64, i64) = tx.query_row(
            "SELECT total_reserve_balance, protected_minimum, reserved_liquidity
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let available = balance - protected_minimum - reserved;
        let capacity_ok = paused == 0 && (amounts.net_destination_atomic as i64) <= available;

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
                    Some("insufficient_capacity_at_fold")
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut still_present = std::collections::HashSet::new();
        for (utxo, confirmations, script_pubkey_hex) in observed {
            still_present.insert((utxo.txid.to_vec(), utxo.vout));
            let state = if *confirmations >= min_confirmations {
                "Available"
            } else {
                "Unconfirmed"
            };
            tx.execute(
                "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(txid, vout) DO UPDATE SET
                    confirmations = excluded.confirmations,
                    state = CASE WHEN vault_utxos.state IN ('Reserved','Spent') THEN vault_utxos.state ELSE excluded.state END",
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
                tx.execute(
                    "UPDATE vault_utxos SET state = 'Spent' WHERE txid = ?1 AND vout = ?2",
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
    pub fn available_vault_utxos(
        &self,
    ) -> Result<Vec<crate::goldcoin::coin::VaultUtxo>, LedgerError> {
        let mut stmt = self
            .conn
            .prepare("SELECT txid, vout, amount_atomic FROM vault_utxos WHERE state = 'Available' ORDER BY amount_atomic DESC, txid ASC, vout ASC")?;
        let rows = stmt
            .query_map([], |r| {
                let txid: Vec<u8> = r.get(0)?;
                Ok(crate::goldcoin::coin::VaultUtxo {
                    txid: to_array32(&txid),
                    vout: r.get(1)?,
                    amount_atomic: r.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Atomically reserves `selected` for `request_id`. The guarded
    /// `UPDATE ... WHERE state = 'Available'` is the actual concurrency
    /// control (SQLite's write-transaction lock, not an application-level
    /// mutex): if a concurrent reservation already claimed one of these
    /// outpoints, fewer rows match than expected and the whole reservation
    /// is rolled back and reported — never partially reserved.
    pub fn reserve_vault_utxos(
        &mut self,
        request_id: i64,
        selected: &[crate::goldcoin::coin::VaultUtxo],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut reserved_count = 0usize;
        for u in selected {
            let n = tx.execute(
                "UPDATE vault_utxos SET state = 'Reserved', reserved_by = ?1, reserved_at = ?2
                 WHERE txid = ?3 AND vout = ?4 AND state = 'Available'",
                rusqlite::params![request_id, now, u.txid.as_slice(), u.vout],
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
                "SELECT payout_atomic, txid, state, confirmations, mined_height, onchain_completion_signature
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
                plan.change_atomic as i64,
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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

    /// Updates confirmation depth; at `required_depth` transitions
    /// `Broadcast -> Confirmed` and `DestinationSubmitted ->
    /// DestinationConfirmed`. Idempotent under repeated ticks. `tip_height`
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
    ) -> Result<(), LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE goldcoin_payouts SET confirmations = ?1 WHERE request_id = ?2 AND state IN ('Broadcast','Confirmed') AND confirmations < ?1",
            rusqlite::params![confirmations, request_id],
        )?;
        if confirmations > 0 {
            tx.execute(
                "UPDATE goldcoin_payouts SET mined_height = ?1 WHERE request_id = ?2 AND mined_height IS NULL",
                rusqlite::params![tip_height - confirmations + 1, request_id],
            )?;
        }
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
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Records that `record_goldcoin_completion` has been submitted to
    /// Solana for this request, carrying the transaction signature.
    /// Deliberately does not touch `bridge_requests.state` or
    /// `goldcoin_payouts.state` — the leg is only truly done once that
    /// submission is confirmed (see [`Ledger::mark_goldcoin_completion_confirmed`]).
    /// Idempotent: a no-op if a signature is already recorded, or if the
    /// request has already reached `Settled`.
    pub fn record_goldcoin_completion_submitted(
        &mut self,
        request_id: i64,
        signature: [u8; 64],
        now: i64,
    ) -> Result<(), LedgerError> {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
                WHERE request_id = ?3 AND onchain_completion_signature IS NULL",
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
