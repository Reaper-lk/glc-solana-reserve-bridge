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
    pub amount_atomic: u64,
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
