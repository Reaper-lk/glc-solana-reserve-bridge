//! Anchor events.
//!
//! Design rule (reused from the old bridge, docs/01-reuse-inventory.md):
//! events are a UX/indexing convenience ONLY. Anything the bridge must not
//! lose is stored in persistent accounts, because Solana log delivery is
//! best-effort. Every fact below is recoverable from account state
//! (constraint 10: auditable reserve accounting must be preserved).

use anchor_lang::prelude::*;

use crate::state::Direction;

#[event]
pub struct BridgeInitialized {
    pub admin: Pubkey,
    pub protocol_version: u8,
    pub threshold: u8,
    pub attestation_key_count: u8,
}

#[event]
pub struct ReserveVaultInitialized {
    pub reserve_token_mint: Pubkey,
    pub reserve_token_account: Pubkey,
    pub reserve_authority: Pubkey,
}

#[event]
pub struct PauseStateChanged {
    pub paused: bool,
    pub release_paused: bool,
    pub deposit_paused: bool,
}

/// Emitted by `instructions::admin::reset_rolling_volume_window` — an
/// explicit administrative override of the rolling-volume anti-drain
/// protection for one direction, only ever callable while the bridge is
/// globally paused. Carries both the pre- and post-reset window state (not
/// just the new one) so an indexer/log reviewer can see exactly how much
/// volume, and how much window life, was discarded by this action.
#[event]
pub struct RollingVolumeWindowReset {
    pub admin: Pubkey,
    pub direction: Direction,
    pub previous_window_start: i64,
    pub previous_window_total: u64,
    pub new_window_start: i64,
    pub new_window_total: u64,
    pub timestamp: i64,
    pub slot: u64,
}

#[event]
pub struct AdminTransferInitiated {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
}

#[event]
pub struct AdminTransferred {
    pub previous_admin: Pubkey,
    pub new_admin: Pubkey,
}

/// A Goldcoin deposit was released 1:1 from the Solana reserve. Advisory
/// only — the `DepositClaim` account is the authoritative record.
#[event]
pub struct ReserveReleased {
    pub txid: [u8; 32],
    pub vout: u32,
    pub recipient: Pubkey,
    pub amount: u64,
    pub attestation_epoch: u64,
}

/// Existing Solana GLC deposited into the reserve and a payout obligation
/// recorded. Advisory only — the `WithdrawalObligation` account is
/// authoritative.
#[event]
pub struct ReserveDeposited {
    pub index: u64,
    pub requester: Pubkey,
    pub amount: u64,
    pub glc_address: Vec<u8>,
}

#[event]
pub struct GovernanceActionProposed {
    pub action: u8,
    pub proposed_under_epoch: u64,
    pub eta: i64,
    pub threshold: u8,
    pub attestation_key_count: u8,
}

#[event]
pub struct GovernanceActionExecuted {
    pub action: u8,
    pub new_epoch: u64,
}

#[event]
pub struct GovernanceActionCancelled {
    pub action: u8,
    pub eta: i64,
}

/// A withdrawal obligation was threshold-confirmed as paid on Goldcoin.
/// Carries the payout identity so an observer can verify the claim against
/// the Goldcoin chain independently (constraint 4).
#[event]
pub struct ObligationCompleted {
    pub index: u64,
    pub payout_txid: [u8; 32],
    pub payout_height: u64,
    pub amount: u64,
    pub attestation_epoch: u64,
}

/// Limits/pause thresholds changed. `field` names which `BridgeConfig`
/// value changed, so a single event type covers all of them without
/// proliferating near-identical structs.
#[event]
pub struct LimitsChanged {
    pub field: String,
    pub previous: u64,
    pub current: u64,
}

// NOTE: rebalance_deposit remains deliberately deferred past this initial
// Phase 2 pass — see IMPLEMENTATION_LOG.md. rebalance_withdraw landed below
// (docs/05-reserve-accounting.md), authorizing an intentional,
// operator-initiated reserve withdrawal, structurally distinct from
// `release_from_reserve` (no Goldcoin deposit is being settled).

/// An intentional, operator-initiated reserve rebalance withdrawal
/// executed. Advisory only — the `RebalanceWithdrawal` account is the
/// authoritative record.
#[event]
pub struct RebalanceWithdrawalExecuted {
    pub nonce: u64,
    pub destination: Pubkey,
    pub amount: u64,
    pub attestation_epoch: u64,
    pub admin: Pubkey,
    pub reserve_balance_after: u64,
}

/// Real, on-chain upgrade authority was handed from `previous_authority` to
/// this program's own `upgrade_authority_pda`
/// ([`crate::constants::SEED_UPGRADE_AUTHORITY`]) — the one-time act that
/// actually arms the timelock mechanism in `instructions::upgrade_timelock`.
/// Advisory only; the loader's own `ProgramData.upgrade_authority_address`
/// is authoritative.
#[event]
pub struct UpgradeAuthorityAccepted {
    pub previous_authority: Pubkey,
    pub upgrade_authority_pda: Pubkey,
}

#[event]
pub struct ProgramUpgradeProposed {
    pub buffer_address: Pubkey,
    pub eta: i64,
    pub proposed_by: Pubkey,
}

/// The real upgrade CPI succeeded. Advisory only — a chain observer should
/// treat the new deployed bytecode itself as authoritative.
#[event]
pub struct ProgramUpgradeExecuted {
    pub buffer_address: Pubkey,
}

#[event]
pub struct ProgramUpgradeCancelled {
    pub buffer_address: Pubkey,
    pub eta: i64,
}
