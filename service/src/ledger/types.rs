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
        }
    }

    /// Non-terminal states whose reserved amount still counts against
    /// `reserved_liquidity` (docs/05-reserve-accounting.md).
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
    /// The fee rate actually applied to this request, in basis points
    /// (always `amount_conversion::BRIDGE_FEE_BPS` today). Persisted for
    /// audit/display; never trusted as an input to signing — attestation
    /// always recomputes the fee from `gross_amount_atomic` via the fixed,
    /// compiled-in rate (docs/20-bridge-fee.md's fee-bypass protections).
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
