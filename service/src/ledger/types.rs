//! Core types for the reserve ledger (docs/04-state-machines.md,
//! docs/05-reserve-accounting.md).

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// Bridge settlement direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Goldcoin deposit confirmed -> Solana reserve release.
    GlcToSol,
    /// Solana deposit confirmed -> Goldcoin reserve release.
    SolToGlc,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::GlcToSol => "GlcToSol",
            Direction::SolToGlc => "SolToGlc",
        }
    }

    /// The reserve a settlement in this direction draws down. Capacity for
    /// a direction depends only on the DESTINATION reserve
    /// (docs/05-reserve-accounting.md).
    pub fn destination_reserve(self) -> ReserveDirection {
        match self {
            Direction::GlcToSol => ReserveDirection::SolanaReserve,
            Direction::SolToGlc => ReserveDirection::GoldcoinReserve,
        }
    }
}

impl std::str::FromStr for Direction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GlcToSol" => Ok(Direction::GlcToSol),
            "SolToGlc" => Ok(Direction::SolToGlc),
            other => Err(format!("unknown direction {other:?}")),
        }
    }
}

impl ToSql for Direction {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for Direction {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Which physical reserve a quantity belongs to (docs/05-reserve-accounting.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReserveDirection {
    GoldcoinReserve,
    SolanaReserve,
}

impl ReserveDirection {
    pub fn as_str(self) -> &'static str {
        match self {
            ReserveDirection::GoldcoinReserve => "GoldcoinReserve",
            ReserveDirection::SolanaReserve => "SolanaReserve",
        }
    }
}

impl std::str::FromStr for ReserveDirection {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "GoldcoinReserve" => Ok(ReserveDirection::GoldcoinReserve),
            "SolanaReserve" => Ok(ReserveDirection::SolanaReserve),
            other => Err(format!("unknown reserve direction {other:?}")),
        }
    }
}

impl ToSql for ReserveDirection {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for ReserveDirection {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Bridge-request lifecycle state (docs/04-state-machines.md). This phase's
/// code (chain plumbing + ledger, no signing client yet) only ever produces
/// states up to and including `SourceFinalized`, plus the error states
/// reachable before that point (`Expired`, `Cancelled`, `Reorged`,
/// `ManualReview`). `SettlementAuthorized` onward is a later phase's work
/// (attestation signing clients / orchestrator) — the states are defined
/// here in full because they are part of one continuous state machine, not
/// because this phase reaches them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    LiquidityReserved,
    AwaitingDeposit,
    DepositObserved,
    Confirming,
    SourceFinalized,
    SettlementAuthorized,
    DestinationSubmitted,
    DestinationConfirmed,
    Settled,
    Expired,
    Cancelled,
    Reorged,
    InsufficientReserveAtSettlement,
    DestinationSubmissionFailed,
    ManualReview,
    Failed,
    /// A `SolToGlc` refund lifecycle has begun for this request
    /// (`Ledger::begin_solana_refund`): a `solana_refunds` row exists, the
    /// on-chain refund transaction has NOT been broadcast yet. From this
    /// state on the request is permanently ineligible for resume and for
    /// any Goldcoin payout — the refund lifecycle is one-way
    /// (`RefundPending -> RefundBroadcast -> Refunded`), never back to
    /// `ManualReview`.
    RefundPending,
    /// The refund's `rebalance_withdraw` transaction has been signed and
    /// recorded (and broadcast, or is about to be — the record is written
    /// BEFORE the send so a crash between the two is recoverable from the
    /// deterministic refund nonce, never by building a second transfer).
    RefundBroadcast,
    /// Terminal: the refund transaction confirmed at `finalized`
    /// commitment and the deposited amount was returned to the original
    /// depositor's own token account. Like `Settled`, nothing ever
    /// transitions out of this state.
    Refunded,
}

impl RequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestState::LiquidityReserved => "LiquidityReserved",
            RequestState::AwaitingDeposit => "AwaitingDeposit",
            RequestState::DepositObserved => "DepositObserved",
            RequestState::Confirming => "Confirming",
            RequestState::SourceFinalized => "SourceFinalized",
            RequestState::SettlementAuthorized => "SettlementAuthorized",
            RequestState::DestinationSubmitted => "DestinationSubmitted",
            RequestState::DestinationConfirmed => "DestinationConfirmed",
            RequestState::Settled => "Settled",
            RequestState::Expired => "Expired",
            RequestState::Cancelled => "Cancelled",
            RequestState::Reorged => "Reorged",
            RequestState::InsufficientReserveAtSettlement => "InsufficientReserveAtSettlement",
            RequestState::DestinationSubmissionFailed => "DestinationSubmissionFailed",
            RequestState::ManualReview => "ManualReview",
            RequestState::Failed => "Failed",
            RequestState::RefundPending => "RefundPending",
            RequestState::RefundBroadcast => "RefundBroadcast",
            RequestState::Refunded => "Refunded",
        }
    }

    /// Non-terminal states whose reserved amount still counts against
    /// `reserved_liquidity` (docs/05-reserve-accounting.md). The refund
    /// states (`RefundPending`/`RefundBroadcast`/`Refunded`) are
    /// deliberately NOT here: a refundable request was fold-parked in
    /// `ManualReview` before any reservation was ever applied
    /// (`Ledger::begin_solana_refund` proves this per-request rather than
    /// assuming it), so no refund state ever holds Goldcoin-side
    /// liquidity.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            RequestState::LiquidityReserved
                | RequestState::AwaitingDeposit
                | RequestState::DepositObserved
                | RequestState::Confirming
                | RequestState::SourceFinalized
                | RequestState::SettlementAuthorized
                | RequestState::DestinationSubmitted
        )
    }

    /// True for the three refund-lifecycle states. A request in any of
    /// them (or with a `solana_refunds` row at all — the stronger check
    /// [`super::Ledger::resume_manual_review_sol_to_glc`] performs) can
    /// never be resumed and never receive a Goldcoin payout.
    pub fn is_refund_lifecycle(self) -> bool {
        matches!(
            self,
            RequestState::RefundPending | RequestState::RefundBroadcast | RequestState::Refunded
        )
    }
}

impl std::str::FromStr for RequestState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "LiquidityReserved" => RequestState::LiquidityReserved,
            "AwaitingDeposit" => RequestState::AwaitingDeposit,
            "DepositObserved" => RequestState::DepositObserved,
            "Confirming" => RequestState::Confirming,
            "SourceFinalized" => RequestState::SourceFinalized,
            "SettlementAuthorized" => RequestState::SettlementAuthorized,
            "DestinationSubmitted" => RequestState::DestinationSubmitted,
            "DestinationConfirmed" => RequestState::DestinationConfirmed,
            "Settled" => RequestState::Settled,
            "Expired" => RequestState::Expired,
            "Cancelled" => RequestState::Cancelled,
            "Reorged" => RequestState::Reorged,
            "InsufficientReserveAtSettlement" => RequestState::InsufficientReserveAtSettlement,
            "DestinationSubmissionFailed" => RequestState::DestinationSubmissionFailed,
            "ManualReview" => RequestState::ManualReview,
            "Failed" => RequestState::Failed,
            "RefundPending" => RequestState::RefundPending,
            "RefundBroadcast" => RequestState::RefundBroadcast,
            "Refunded" => RequestState::Refunded,
            other => return Err(format!("unknown request state {other:?}")),
        })
    }
}

impl ToSql for RequestState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RequestState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `bridge_requests`.
///
/// `recipient` is variable-length, NOT a fixed 32 bytes: for `GlcToSol` it
/// is a 32-byte Solana pubkey, but for `SolToGlc` it is an opaque ASCII
/// Goldcoin address (up to 64 bytes, same `MAX_GLC_ADDRESS_LEN` convention
/// as the on-chain `WithdrawalObligation.glc_address` — see
/// `programs/glc-reserve-bridge/src/constants.rs`). A fixed `[u8; 32]` here
/// would silently truncate a real Goldcoin address; this was caught during
/// implementation of the Solana-side fold and fixed before it shipped (see
/// IMPLEMENTATION_LOG.md).
#[derive(Debug, Clone)]
pub struct BridgeRequest {
    pub id: i64,
    pub direction: Direction,
    pub state: RequestState,
    /// What the user declared/deposited, in the ledger's canonical
    /// accounting unit (8 decimals — `amount_conversion::CanonicalAtomic`;
    /// docs/20-bridge-fee.md). NOT what actually settles — see
    /// [`BridgeRequest::net_amount_atomic`].
    pub gross_amount_atomic: u64,
    /// The fee rate actually applied to this request, in basis points —
    /// the fee-POLICY SNAPSHOT taken at creation/fold time
    /// (`amount_conversion::BRIDGE_FEE_BPS` as of that moment), immutable
    /// historical accounting thereafter. Every settlement/attestation/
    /// recovery path validates and settles the request at THIS rate, not
    /// the currently compiled-in one (`amount_conversion::
    /// verify_fee_breakdown`), so an in-flight request survives a fee-rate
    /// change; the snapshot is only accepted if it is a rate the protocol
    /// actually charged at some point (`amount_conversion::
    /// HISTORICAL_FEE_BPS`), and the stored fee/net must still reconcile
    /// exactly against it — docs/20-bridge-fee.md's fee-bypass
    /// protections, unweakened.
    pub fee_bps: u64,
    /// Canonical units. `gross_amount_atomic == fee_amount_atomic +
    /// net_amount_atomic` always holds (`amount_conversion::compute_fee`).
    pub fee_amount_atomic: u64,
    /// Canonical units — the real-world GLC entitlement actually delivered
    /// (destination payout before chain-specific unit conversion).
    pub net_amount_atomic: u64,
    /// Same net entitlement as [`BridgeRequest::net_amount_atomic`], but in
    /// the DESTINATION reserve's own native chain unit — the amount
    /// actually reserved/settled against `reserve_ledger`'s capacity
    /// counters and, for `GlcToSol`, the exact amount
    /// `release_from_reserve` transfers on Solana.
    pub net_destination_atomic: u64,
    pub recipient: Vec<u8>,
    pub requester: Option<[u8; 32]>,
    pub created_at: i64,
    pub reserved_at: Option<i64>,
    pub reservation_expires_at: Option<i64>,
    pub source_txid: Option<[u8; 32]>,
    pub source_vout: Option<u32>,
    pub source_obligation_index: Option<u64>,
    pub source_block_height: Option<i64>,
    pub source_block_hash: Option<[u8; 32]>,
    pub source_confirmations: i64,
    pub source_finalized_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub manual_review_note: Option<String>,
}

/// The full gross/fee/net breakdown for one new bridge request, as the
/// caller (`api.rs` for `GlcToSol`, `solana::indexer` for `SolToGlc`) must
/// compute it via `amount_conversion::compute_fee` before calling
/// [`super::Ledger::create_request`]/[`super::Ledger::fold_sol_deposit`] —
/// the ledger itself never computes a conversion or a fee; it only stores
/// and enforces capacity against what it's given (docs/20-bridge-fee.md).
/// All fields are canonical EXCEPT `net_destination_atomic` — see
/// [`BridgeRequest::net_destination_atomic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestAmounts {
    pub gross_atomic: u64,
    pub fee_bps: u64,
    pub fee_atomic: u64,
    pub net_atomic: u64,
    pub net_destination_atomic: u64,
}

/// Which direction a rebalance moves real, already-existing funds
/// (docs/05-reserve-accounting.md, docs/22-production-readiness-review.md
/// P1 "rebalancing"). Structurally distinct from `Direction`
/// (`GlcToSol`/`SolToGlc`, user settlements) — a rebalance never touches
/// `bridge_requests`, `reserved_liquidity`, or `pending_obligations`, only
/// `total_reserve_balance` on the ONE named reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceKind {
    /// Real funds moved INTO the named reserve from outside it (a
    /// treasury top-up, or funds swept from the other reserve's own
    /// excess by whatever real transfer the operator actually executes).
    Deposit,
    /// Real funds moved OUT of the named reserve (e.g. sweeping surplus
    /// to cold storage, or funding the other reserve's shortfall).
    Withdraw,
}

impl RebalanceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RebalanceKind::Deposit => "Deposit",
            RebalanceKind::Withdraw => "Withdraw",
        }
    }
}

impl std::str::FromStr for RebalanceKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Deposit" => Ok(RebalanceKind::Deposit),
            "Withdraw" => Ok(RebalanceKind::Withdraw),
            other => Err(format!("unknown rebalance kind {other:?}")),
        }
    }
}

impl ToSql for RebalanceKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RebalanceKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Rebalance-request lifecycle (docs/22-production-readiness-review.md P1
/// "rebalancing"). Deliberately never reaches a state that implies THIS
/// service broadcast or signed a real fund-moving transaction — the
/// transition into `Executed` only ever records evidence (`tx_reference`)
/// of a transfer some operator authorized and executed entirely out of
/// band, through whatever real custody tooling holds the actual keys
/// (docs/02-trust-model.md). This ledger tracks the REQUEST, its
/// approvals, and its audit trail; it never moves funds itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebalanceState {
    /// Created; collecting the configured number of approvals.
    Proposed,
    /// Approval threshold reached; awaiting out-of-band execution.
    Approved,
    /// An operator recorded a real `tx_reference` for a transfer they
    /// already authorized and executed outside this system.
    Executed,
    /// The resulting real balance change was independently confirmed
    /// (operator-reported observation, cross-checked against the next
    /// live reconciliation read) — terminal success.
    Confirmed,
    /// An approver declined before execution — terminal.
    Rejected,
    /// Withdrawn by an operator before execution — terminal.
    Cancelled,
    /// Execution was recorded but the expected effect was never
    /// confirmed (or was confirmed to be wrong) — routed here rather than
    /// silently left `Executed` forever; requires operator resolution,
    /// same discipline as `RequestState::ManualReview`.
    Failed,
}

impl RebalanceState {
    pub fn as_str(self) -> &'static str {
        match self {
            RebalanceState::Proposed => "Proposed",
            RebalanceState::Approved => "Approved",
            RebalanceState::Executed => "Executed",
            RebalanceState::Confirmed => "Confirmed",
            RebalanceState::Rejected => "Rejected",
            RebalanceState::Cancelled => "Cancelled",
            RebalanceState::Failed => "Failed",
        }
    }

    /// Non-terminal — still expected to move forward or be explicitly
    /// closed out by an operator.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            RebalanceState::Proposed | RebalanceState::Approved | RebalanceState::Executed
        )
    }
}

impl std::str::FromStr for RebalanceState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Proposed" => RebalanceState::Proposed,
            "Approved" => RebalanceState::Approved,
            "Executed" => RebalanceState::Executed,
            "Confirmed" => RebalanceState::Confirmed,
            "Rejected" => RebalanceState::Rejected,
            "Cancelled" => RebalanceState::Cancelled,
            "Failed" => RebalanceState::Failed,
            other => return Err(format!("unknown rebalance state {other:?}")),
        })
    }
}

impl ToSql for RebalanceState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for RebalanceState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `rebalance_requests`. `amount_atomic` is always in
/// `direction`'s own native chain unit (Goldcoin-native or the Solana
/// reserve mint's live decimals) — a rebalance never involves a
/// cross-chain conversion, since it moves one already-existing asset
/// within one chain (into or out of that chain's own reserve), unlike a
/// bridge settlement.
#[derive(Debug, Clone)]
pub struct RebalanceRequest {
    pub id: i64,
    pub direction: ReserveDirection,
    pub kind: RebalanceKind,
    pub amount_atomic: u64,
    pub state: RebalanceState,
    pub reason: String,
    pub requested_by: String,
    pub requested_at: i64,
    pub required_approvals: u32,
    /// JSON array of approving identities, never key material.
    pub approved_by: Vec<String>,
    pub approved_at: Option<i64>,
    pub tx_reference: Option<String>,
    pub executed_at: Option<i64>,
    pub observed_amount_atomic: Option<u64>,
    pub confirmed_at: Option<i64>,
    pub failure_reason: Option<String>,
}

/// Which custody surface a transition rotates
/// (docs/22-production-readiness-review.md P1 "key rotation / vault
/// sweep tooling").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyTransitionKind {
    /// Retiring one or more ed25519 attestation signer identities in
    /// favor of a new set (`signing::attestation`). Authorizes BOTH
    /// bridge directions, so `record_custody_transition_executed`
    /// requires both reserves paused first.
    AttestationKeyRotation,
    /// Sweeping the Goldcoin P2SH multisig vault
    /// (`signing::goldcoin_vault`) to a new vault identity/threshold.
    /// Only the Goldcoin reserve need be paused first.
    GoldcoinVaultSweep,
}

impl CustodyTransitionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CustodyTransitionKind::AttestationKeyRotation => "AttestationKeyRotation",
            CustodyTransitionKind::GoldcoinVaultSweep => "GoldcoinVaultSweep",
        }
    }
}

impl std::str::FromStr for CustodyTransitionKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "AttestationKeyRotation" => Ok(CustodyTransitionKind::AttestationKeyRotation),
            "GoldcoinVaultSweep" => Ok(CustodyTransitionKind::GoldcoinVaultSweep),
            other => Err(format!("unknown custody transition kind {other:?}")),
        }
    }
}

impl ToSql for CustodyTransitionKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for CustodyTransitionKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// Custody-transition lifecycle
/// (docs/22-production-readiness-review.md P1 "key rotation / vault
/// sweep tooling"). Extends the rebalance shape with one required extra
/// gate: a new identity must be independently verified BEFORE any
/// approval can be recorded, modeling "verification of new signer
/// identity before activation" as enforced, not advisory. Like
/// `RebalanceState`, `Executed` only ever records evidence of a real
/// rotation/sweep executed out of band — this service never generates
/// keys, signs, or broadcasts the transition itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyTransitionState {
    /// Created; new identity not yet verified.
    Proposed,
    /// The new signer identity has been independently verified
    /// (e.g. a signed challenge checked against the claimed public
    /// key/vault descriptor) — required before approvals may begin.
    IdentityVerified,
    /// Approval threshold reached; awaiting out-of-band execution.
    Approved,
    /// An operator recorded a real `tx_reference`/rotation evidence for
    /// a transition already authorized and executed outside this
    /// system. Requires the relevant reserve(s) already paused.
    Executed,
    /// The new custody identity was independently confirmed active and
    /// correct post-transition — terminal success.
    Confirmed,
    /// An approver declined before execution — terminal.
    Rejected,
    /// Withdrawn by an operator before execution — terminal.
    Cancelled,
    /// Execution was recorded but the expected new-identity state was
    /// never confirmed (or confirmed wrong) — requires operator
    /// resolution, same discipline as `RebalanceState::Failed`.
    Failed,
    /// An operator recorded that a `Failed` transition's real-world
    /// effect was reverted back to the old identity out of band. Only
    /// ever an audit marker of a real rollback already performed — this
    /// service never performs the rollback itself.
    RolledBack,
}

impl CustodyTransitionState {
    pub fn as_str(self) -> &'static str {
        match self {
            CustodyTransitionState::Proposed => "Proposed",
            CustodyTransitionState::IdentityVerified => "IdentityVerified",
            CustodyTransitionState::Approved => "Approved",
            CustodyTransitionState::Executed => "Executed",
            CustodyTransitionState::Confirmed => "Confirmed",
            CustodyTransitionState::Rejected => "Rejected",
            CustodyTransitionState::Cancelled => "Cancelled",
            CustodyTransitionState::Failed => "Failed",
            CustodyTransitionState::RolledBack => "RolledBack",
        }
    }

    /// Non-terminal — still expected to move forward or be explicitly
    /// closed out by an operator.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            CustodyTransitionState::Proposed
                | CustodyTransitionState::IdentityVerified
                | CustodyTransitionState::Approved
                | CustodyTransitionState::Executed
        )
    }
}

impl std::str::FromStr for CustodyTransitionState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Proposed" => CustodyTransitionState::Proposed,
            "IdentityVerified" => CustodyTransitionState::IdentityVerified,
            "Approved" => CustodyTransitionState::Approved,
            "Executed" => CustodyTransitionState::Executed,
            "Confirmed" => CustodyTransitionState::Confirmed,
            "Rejected" => CustodyTransitionState::Rejected,
            "Cancelled" => CustodyTransitionState::Cancelled,
            "Failed" => CustodyTransitionState::Failed,
            "RolledBack" => CustodyTransitionState::RolledBack,
            other => return Err(format!("unknown custody transition state {other:?}")),
        })
    }
}

impl ToSql for CustodyTransitionState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for CustodyTransitionState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `custody_transitions`. `old_identities`/`new_identities` are
/// JSON arrays of opaque public identity strings (pubkeys/vault
/// descriptors) — never key material. `new_threshold` only applies to
/// `GoldcoinVaultSweep` (a new multisig M-of-N); left `None` for
/// `AttestationKeyRotation`, which has no threshold concept.
#[derive(Debug, Clone)]
pub struct CustodyTransition {
    pub id: i64,
    pub kind: CustodyTransitionKind,
    pub state: CustodyTransitionState,
    pub old_identities: Vec<String>,
    pub new_identities: Vec<String>,
    pub new_threshold: Option<u32>,
    pub reason: String,
    pub requested_by: String,
    pub requested_at: i64,
    pub required_approvals: u32,
    pub approved_by: Vec<String>,
    pub approved_at: Option<i64>,
    pub identity_verified_by: Option<String>,
    pub identity_verified_at: Option<i64>,
    pub tx_reference: Option<String>,
    pub executed_at: Option<i64>,
    pub confirmed_at: Option<i64>,
    pub failure_reason: Option<String>,
    pub rolled_back_at: Option<i64>,
    pub rollback_reason: Option<String>,
}

/// How an admin mutation attempt ended, for the `admin_audit_log`
/// (`Ledger::append_admin_audit`). Failed attempts are recorded too —
/// "an operator tried and was refused" is itself audit-relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAuditOutcome {
    Success,
    /// The refusal/failure message shown to the operator (a `LedgerError`
    /// display string, typically) — never internal paths or secrets.
    Error(String),
}

/// One admin mutation attempt to append via [`crate::ledger::Ledger::
/// append_admin_audit`]. `old_value`/`new_value` are small JSON or plain
/// display snapshots of the mutated setting, captured by the caller
/// BEFORE and after (or as-requested) the mutation.
#[derive(Debug, Clone)]
pub struct AdminAuditEntry {
    pub at: i64,
    /// Operator identity: the admin-API operator name the bearer token
    /// resolved to, or `cli:<user>` for `glc-admin` invocations.
    pub actor: String,
    /// Machine-readable action slug: `pause`, `unpause`,
    /// `admission_open`, `admission_close`, `resume_manual_review`,
    /// `rebalance_propose`, ...
    pub action: String,
    /// What was acted on: a direction, request id, or rebalance id.
    pub target: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    /// Mandatory operator-supplied reason; the schema `CHECK`s it
    /// non-empty.
    pub note: String,
    pub outcome: AdminAuditOutcome,
}

/// A stored `admin_audit_log` row ([`crate::ledger::Ledger::
/// list_admin_audit`]).
#[derive(Debug, Clone)]
pub struct AdminAuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub action: String,
    pub target: Option<String>,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub note: String,
    pub outcome: AdminAuditOutcome,
}

/// Keyset-paginated filter for [`crate::ledger::Ledger::
/// list_admin_audit`]: rows with `id < before_id` (newest first), capped
/// at `limit`, optionally restricted to one action slug and/or actor.
#[derive(Debug, Clone, Default)]
pub struct AdminAuditFilter {
    pub before_id: Option<i64>,
    pub limit: Option<u32>,
    pub action: Option<String>,
    pub actor: Option<String>,
}

/// Lifecycle state of one `solana_refunds` row. Mirrors the owning
/// request's own `RefundPending`/`RefundBroadcast`/`Refunded` states 1:1
/// — the request state drives visibility/gating in every daemon loop, the
/// refund row carries the artifact (nonce, amounts, destination,
/// signature) and the structural one-refund-per-request guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolanaRefundState {
    /// Row created; nothing signed or broadcast yet. Safe to re-run the
    /// refund command — it resumes from attestation.
    Pending,
    /// The refund transaction's signature has been recorded (recorded
    /// BEFORE the send, so a crash between record and broadcast is
    /// recoverable). At most one such transaction can ever land: the
    /// deterministic refund nonce's `rebalance_withdrawal` PDA is the
    /// on-chain replay guard.
    Broadcast,
    /// Terminal: confirmed at `finalized` commitment; the request is
    /// `Refunded`.
    Confirmed,
}

impl SolanaRefundState {
    pub fn as_str(self) -> &'static str {
        match self {
            SolanaRefundState::Pending => "Pending",
            SolanaRefundState::Broadcast => "Broadcast",
            SolanaRefundState::Confirmed => "Confirmed",
        }
    }
}

impl std::str::FromStr for SolanaRefundState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "Pending" => SolanaRefundState::Pending,
            "Broadcast" => SolanaRefundState::Broadcast,
            "Confirmed" => SolanaRefundState::Confirmed,
            other => return Err(format!("unknown solana refund state {other:?}")),
        })
    }
}

impl ToSql for SolanaRefundState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for SolanaRefundState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        s.parse().map_err(|_| FromSqlError::InvalidType)
    }
}

/// A row of `solana_refunds` — the full audit/idempotency record of one
/// ManualReview refund lifecycle. `request_id` is the table's PRIMARY KEY
/// (at most one refund lifecycle per request, structurally), `nonce` and
/// `obligation_index` are additionally UNIQUE, and the nonce doubles as
/// the on-chain replay guard: `rebalance_withdraw`'s `rebalance_withdrawal`
/// PDA `init` makes a second transfer under the same nonce impossible on
/// chain, independent of any database state.
#[derive(Debug, Clone)]
pub struct SolanaRefund {
    pub request_id: i64,
    /// The on-chain `WithdrawalObligation.index` this refund returns —
    /// the canonical identity of the original, finalized Solana deposit.
    pub obligation_index: u64,
    /// `refund domain bit | request_id` — see
    /// [`crate::ledger::Ledger::solana_refund_nonce`].
    pub nonce: u64,
    /// Exact gross deposited amount, in the reserve mint's own native
    /// atomic units (the on-chain `WithdrawalObligation.amount`). No fee
    /// is deducted: for SolToGlc the bridge fee only ever accrues at
    /// settlement, which a refunded request never reaches.
    pub amount_solana_atomic: u64,
    /// The original depositor's wallet (32 bytes) — copied from
    /// `bridge_requests.requester`, itself decoded from the on-chain
    /// obligation's `requester` field, and re-verified against a fresh
    /// finalized read of that obligation before any transfer.
    pub requester: [u8; 32],
    /// The canonical ATA of (`requester`, reserve mint, reserve token
    /// program) — always derived, never operator-supplied.
    pub destination_token_account: [u8; 32],
    pub reserve_mint: [u8; 32],
    pub token_program: [u8; 32],
    /// Frozen copy of the request's `manual_review_note` at refund-begin
    /// time (the request row's own copy is preserved too — never
    /// overwritten by the refund lifecycle).
    pub manual_review_reason: String,
    pub note: String,
    pub created_by: String,
    pub state: SolanaRefundState,
    pub attestation_epoch: Option<u64>,
    /// Base58 transaction signature of the (latest) refund broadcast.
    /// Never key material.
    pub refund_signature: Option<String>,
    /// The (latest) broadcast transaction's recent blockhash — recovery
    /// uses it to POSITIVELY determine the transaction can no longer land
    /// before ever rebuilding under the same nonce.
    pub recent_blockhash: Option<String>,
    pub created_at: i64,
    pub broadcast_at: Option<i64>,
    pub confirmed_at: Option<i64>,
}
