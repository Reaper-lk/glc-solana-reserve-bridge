//! Program account state.
//!
//! Every account documents its byte layout exactly; the `SPACE` constants
//! are the single source of truth for allocation and are asserted against
//! real borsh serialization in the `space` test module below. Any layout
//! change here is a `PROTOCOL_VERSION` bump.
//!
//! Adapted from the old bridge's `state.rs` (docs/01-reuse-inventory.md):
//! the PDA design pattern (singleton config, per-item seeded records, bump
//! storage, reserved-byte padding for future fields) is reused unchanged.
//! `ValidatorSet` becomes `AttestationKeySet` (small, fixed, internal
//! threshold custody per docs/02-trust-model.md — NOT a federation).
//! `mint_wrapped`'s replay-guard `DepositClaim` is reused verbatim in
//! mechanism. `WithdrawalRequest` becomes `WithdrawalObligation` (records a
//! reserve-release obligation, not a burn). `RollingVolumeWindow` has no
//! old-repo analog (docs/01-reuse-inventory.md classifies it "E — new").

use anchor_lang::prelude::*;

use crate::constants::MAX_ATTESTATION_KEYS;

/// Which settlement direction an amount/limit applies to.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    /// Goldcoin deposit confirmed -> Solana reserve release.
    GoldcoinToSolana,
    /// Solana deposit confirmed -> Goldcoin reserve release (off-chain).
    SolanaToGoldcoin,
}

/// Singleton bridge configuration (PDA: [`crate::constants::SEED_BRIDGE_CONFIG`]).
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field                       | type             | bytes |
/// |------------------------------|-----------------|-------|
/// | `protocol_version`           | `u8`             | 1     |
/// | `admin`                      | `Pubkey`         | 32    |
/// | `pending_admin`               | `Option<Pubkey>` | 1+32  |
/// | `paused`                      | `bool`           | 1     |
/// | `release_paused`              | `bool`           | 1     |
/// | `deposit_paused`              | `bool`           | 1     |
/// | `bump`                        | `u8`             | 1     |
/// | `reserve_token_mint`          | `Pubkey`         | 32    |
/// | `reserve_authority_bump`      | `u8`             | 1     |
/// | `obligation_count`            | `u64`            | 8     |
/// | `governance_timelock_seconds` | `i64`            | 8     |
/// | `min_transfer_amount`         | `u64`            | 8     |
/// | `per_transfer_limit`          | `u64`            | 8     |
/// | `protected_minimum`           | `u64`            | 8     |
/// | `rolling_volume_limit`        | `u64`            | 8     |
/// | `rolling_window_seconds`      | `i64`            | 8     |
/// | `upgrade_timelock_seconds`    | `i64`            | 8     |
/// | `reserved`                    | `[u8; 32]`       | 32    |
#[account]
pub struct BridgeConfig {
    /// [`crate::constants::PROTOCOL_VERSION`] at initialization; bumped by
    /// future migrations.
    pub protocol_version: u8,
    /// Governance authority for pause and admin handover. Attestation-key
    /// rotation is NOT gated by this alone — it requires the timelocked,
    /// threshold-approved governance path (see [`PendingGovernanceAction`]),
    /// deliberately, so the admin alone can never install attacker keys.
    /// Limit/pause changes ARE currently admin-gated-immediate (see
    /// `docs/12-management-decisions.md` and the implementation log) — a
    /// known interim posture, not the target end state.
    pub admin: Pubkey,
    /// Set by `transfer_admin`, consumed by `accept_admin` (two-step
    /// handover so a typoed key cannot brick governance).
    pub pending_admin: Option<Pubkey>,
    /// Global emergency circuit breaker. Blocks both settlement directions;
    /// admin instructions remain callable while paused (otherwise
    /// un-pausing would be impossible).
    pub paused: bool,
    /// Directional circuit breaker: Goldcoin -> Solana releases.
    pub release_paused: bool,
    /// Directional circuit breaker: Solana -> Goldcoin deposits.
    pub deposit_paused: bool,
    /// Canonical PDA bump.
    pub bump: u8,
    /// The existing Solana GLC token mint this bridge holds reserves of.
    /// This is NOT a wrapped/bridge-minted token — the reserve model uses
    /// the existing token directly (docs/00-executive-summary.md).
    /// `Pubkey::default()` means "reserve vault not yet configured"; every
    /// reserve-touching path explicitly rejects that sentinel.
    pub reserve_token_mint: Pubkey,
    /// Which SPL token program actually owns `reserve_token_mint` — legacy
    /// SPL Token or Token-2022 — captured once at `initialize_reserve_vault`
    /// from whatever program the admin's supplied mint actually belongs to,
    /// and pinned via an `address` constraint on every later
    /// deposit/release instruction's `token_program` account
    /// (docs/18-token-2022-support.md). This is what makes "wrong token
    /// program" a structural, on-chain-enforced rejection rather than an
    /// off-chain assumption: once a mint is configured, no other program
    /// — including the *other* legitimate SPL token program — can ever be
    /// substituted for it.
    pub reserve_token_program: Pubkey,
    /// Canonical bump of the reserve-authority PDA
    /// ([`crate::constants::SEED_RESERVE_AUTHORITY`]), stored at
    /// `initialize_reserve_vault` for `invoke_signed`.
    pub reserve_authority_bump: u8,
    /// Monotonic seed counter for `WithdrawalObligation` PDAs.
    pub obligation_count: u64,
    /// Delay, in seconds, between proposing a governance action (currently:
    /// attestation-key rotation) and its earliest execution. No built-in
    /// default: refused at zero, exactly as the old bridge refused it
    /// (ADR-0014) — the safe value is a live ops decision.
    pub governance_timelock_seconds: i64,
    /// Dust/DoS floor for deposits and releases. 0 = disabled.
    pub min_transfer_amount: u64,
    /// Hard ceiling on a single transfer's amount, either direction. Never
    /// zero: a cap of zero would have to mean either "nothing allowed" or
    /// "unlimited", and the program refuses to start without a deliberate
    /// value (same precedent as the old bridge's `max_wrapped_supply`).
    pub per_transfer_limit: u64,
    /// Reserve floor: releases that would take the reserve token account
    /// below this are refused (constraint 6: reserve insufficiency fails
    /// closed). May be zero (no protection) — this is a deliberate risk
    /// the admin can choose, unlike `per_transfer_limit`, which cannot ever
    /// be zero/unlimited.
    pub protected_minimum: u64,
    /// Rolling-volume cap enforced against [`RollingVolumeWindow`]. Never
    /// zero, same reasoning as `per_transfer_limit`.
    pub rolling_volume_limit: u64,
    /// Width, in seconds, of the fixed bucket `RollingVolumeWindow` resets
    /// on. A fixed-bucket window, not a true sliding window — a documented
    /// simplification (see the implementation log); it is strictly
    /// conservative in the sense that it never UNDER-counts volume inside a
    /// bucket, but a burst spanning a bucket boundary can exceed the limit
    /// within a short combined window. Acceptable for the current
    /// development phase; flagged for hardening before production sizing.
    pub rolling_window_seconds: i64,
    /// Delay, in seconds, between proposing a program upgrade
    /// (`instructions::upgrade_timelock::propose_upgrade`) and its
    /// earliest execution. Same no-built-in-default discipline as
    /// `governance_timelock_seconds` — refused at zero. A separate field
    /// rather than reusing `governance_timelock_seconds`: upgrade-authority
    /// timing and attestation-key-rotation timing are different policy
    /// questions with no reason to share one value
    /// (docs/12-management-decisions.md items 1 and 3 are separate
    /// decisions).
    pub upgrade_timelock_seconds: i64,
}

impl BridgeConfig {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 1 // protocol_version
        + 32 // admin
        + (1 + 32) // pending_admin
        + 1 // paused
        + 1 // release_paused
        + 1 // deposit_paused
        + 1 // bump
        + 32 // reserve_token_mint
        + 32 // reserve_token_program
        + 1 // reserve_authority_bump
        + 8 // obligation_count
        + 8 // governance_timelock_seconds
        + 8 // min_transfer_amount
        + 8 // per_transfer_limit
        + 8 // protected_minimum
        + 8 // rolling_volume_limit
        + 8 // rolling_window_seconds
        + 8; // upgrade_timelock_seconds
}

/// Singleton internal attestation-key set (PDA:
/// [`crate::constants::SEED_ATTESTATION_KEY_SET`]).
///
/// **Not a federation validator set.** Per the approved trust model
/// (docs/02-trust-model.md Option 6, docs/12-management-decisions.md item
/// 1): a small, fixed set of keys held in genuinely separate internal
/// custody domains (HSM/KMS-backed in production), operated by the single
/// bridge operator. Threshold-signing here is an internal dual-control
/// security measure, not a trust-distribution protocol across independent
/// organizations.
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field       | type          | bytes        |
/// |-------------|---------------|--------------|
/// | `epoch`     | `u64`         | 8            |
/// | `threshold` | `u8`          | 1            |
/// | `bump`      | `u8`          | 1            |
/// | `keys`      | `Vec<Pubkey>` | 4 + 32 × len |
/// | `reserved`  | `[u8; 32]`    | 32           |
#[account]
pub struct AttestationKeySet {
    /// Revision counter, incremented (checked) on every rotation. Claims
    /// bind to the epoch they were signed under, so a rotation invalidates
    /// in-flight signatures.
    pub epoch: u64,
    /// M of M-of-N: minimum unique attestation-key signatures required.
    /// Invariant (enforced on every write): `2 <= threshold <= keys.len()`
    /// — the lower bound of 2 is load-bearing: a threshold of 1 would mean
    /// a single key can release reserves, which is exactly what the
    /// approved trust model rules out (constraint: no single operator or
    /// single hot key capable of releasing reserves).
    pub threshold: u8,
    /// Canonical PDA bump.
    pub bump: u8,
    /// Attestation-signer public keys (ed25519). Invariants (enforced on
    /// every write): non-empty, no duplicates, no all-zero (default) keys,
    /// `len() <= MAX_ATTESTATION_KEYS`.
    pub keys: Vec<Pubkey>,
    /// Expansion space. Must be all zeroes until a migration assigns
    /// meaning.
    pub reserved: [u8; 32],
}

impl AttestationKeySet {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 8 // epoch
        + 1 // threshold
        + 1 // bump
        + (4 + 32 * MAX_ATTESTATION_KEYS) // keys (Vec length prefix + keys)
        + 32; // reserved
}

/// Governance action type discriminants for [`PendingGovernanceAction::action`].
/// `0` is deliberately never valid.
pub const GOVERNANCE_ACTION_ROTATE_ATTESTATION_KEYS: u8 = 1;

/// A governance action that has been threshold-approved but is still inside
/// its timelock window (PDA: [`crate::constants::SEED_GOVERNANCE_ACTION`]).
///
/// A singleton: at most one action may be pending at a time, so "what is
/// about to happen to this bridge?" is always a single account read, and an
/// attacker who briefly meets threshold cannot queue a backlog that matures
/// later. Currently governs attestation-key rotation only; limit/threshold
/// governance is deferred (see the implementation log) and, for now, is
/// admin-gated-immediate rather than timelocked.
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field                   | type          | bytes        |
/// |--------------------------|---------------|--------------|
/// | `action`                 | `u8`          | 1            |
/// | `proposed_under_epoch`   | `u64`         | 8            |
/// | `eta`                    | `i64`         | 8            |
/// | `threshold`              | `u8`          | 1            |
/// | `keys`                   | `Vec<Pubkey>` | 4 + 32 × len |
/// | `bump`                   | `u8`          | 1            |
/// | `reserved`               | `[u8; 24]`    | 24           |
#[account]
pub struct PendingGovernanceAction {
    /// Currently always [`GOVERNANCE_ACTION_ROTATE_ATTESTATION_KEYS`].
    pub action: u8,
    /// The attestation-key epoch this proposal was signed under. Execution
    /// requires the epoch to still match.
    pub proposed_under_epoch: u64,
    /// Earliest Unix timestamp at which execution is permitted.
    pub eta: i64,
    /// Proposed threshold.
    pub threshold: u8,
    /// Proposed attestation-key set, in the exact order it will be stored.
    pub keys: Vec<Pubkey>,
    /// Canonical PDA bump.
    pub bump: u8,
    /// Expansion space.
    pub reserved: [u8; 24],
}

impl PendingGovernanceAction {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 1 // action
        + 8 // proposed_under_epoch
        + 8 // eta
        + 1 // threshold
        + (4 + 32 * MAX_ATTESTATION_KEYS) // keys
        + 1 // bump
        + 24; // reserved
}

/// A proposed program upgrade currently inside its timelock window (PDA:
/// [`crate::constants::SEED_PENDING_UPGRADE`]).
///
/// A singleton, same reasoning as [`PendingGovernanceAction`]: at most one
/// upgrade may be queued at a time. Admin-gated to propose/cancel
/// (`instructions::upgrade_timelock` module docs explain why this is
/// admin-gated rather than threshold-gated, unlike attestation-key
/// rotation), permissionless to execute once `eta` has passed.
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field              | type     | bytes |
/// |----------------------|----------|-------|
/// | `buffer_address`      | `Pubkey` | 32    |
/// | `proposed_at`          | `i64`    | 8     |
/// | `eta`                  | `i64`    | 8     |
/// | `proposed_by`          | `Pubkey` | 32    |
/// | `bump`                 | `u8`     | 1     |
/// | `reserved`             | `[u8; 16]` | 16  |
#[account]
pub struct PendingProgramUpgrade {
    /// The BPF-loader-v3 buffer account holding the proposed new program
    /// bytecode. Not itself validated here — the loader's own `Upgrade`
    /// instruction is what actually checks it (buffer authority, program
    /// size headroom, etc.) when `execute_upgrade` CPIs into it.
    pub buffer_address: Pubkey,
    pub proposed_at: i64,
    /// Earliest Unix timestamp at which execution is permitted.
    pub eta: i64,
    /// The admin identity that proposed this upgrade — audit trail only;
    /// confers no special execution rights (execution is permissionless
    /// once `eta` has passed, same as governance actions).
    pub proposed_by: Pubkey,
    /// Canonical PDA bump.
    pub bump: u8,
    /// Expansion space.
    pub reserved: [u8; 16],
}

impl PendingProgramUpgrade {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 32 // buffer_address
        + 8 // proposed_at
        + 8 // eta
        + 32 // proposed_by
        + 1 // bump
        + 16; // reserved
}

/// One processed Goldcoin deposit (PDA: [`crate::constants::SEED_DEPOSIT_CLAIM`]
/// + `txid` + `vout.to_le_bytes()`). The account's existence is the
/// on-chain replay guard: a second release for the same `(txid, vout)`
/// fails at account creation (constraint 5: prevent replay/double-release).
/// Doubles as the permanent audit record of the deposit.
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field               | type       | bytes |
/// |----------------------|------------|-------|
/// | `txid`                | `[u8; 32]` | 32    |
/// | `vout`                 | `u32`      | 4     |
/// | `amount`               | `u64`      | 8     |
/// | `recipient`            | `Pubkey`   | 32    |
/// | `attestation_epoch`    | `u64`      | 8     |
/// | `protocol_version`     | `u8`       | 1     |
/// | `slot_created`         | `u64`      | 8     |
/// | `bump`                 | `u8`       | 1     |
/// | `reserved`             | `[u8; 16]` | 16    |
#[account]
pub struct DepositClaim {
    /// Goldcoin transaction id, 32 bytes used verbatim as supplied in the
    /// claim.
    pub txid: [u8; 32],
    /// Output index within the transaction.
    pub vout: u32,
    /// Released amount in the reserve mint's own atomic units — its
    /// `decimals` field, read live from chain state at release time, not a
    /// hardcoded assumption (docs/18-token-2022-support.md). 1:1 with the
    /// GLC quantity independently verified on the Goldcoin deposit
    /// (constraint 1) once converted to this mint's precision by the
    /// off-chain service (`amount_conversion::goldcoin_to_solana_atomic`)
    /// before this instruction is ever submitted.
    pub amount: u64,
    /// Solana wallet the reserve GLC was released to.
    pub recipient: Pubkey,
    /// Attestation-key epoch this claim was authorized under.
    pub attestation_epoch: u64,
    /// `BridgeConfig.protocol_version` at release time.
    pub protocol_version: u8,
    /// Solana slot in which the claim was created.
    pub slot_created: u64,
    /// Canonical PDA bump.
    pub bump: u8,
    /// Expansion space.
    pub reserved: [u8; 16],
}

impl DepositClaim {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 32 // txid
        + 4 // vout
        + 8 // amount
        + 32 // recipient
        + 8 // attestation_epoch
        + 1 // protocol_version
        + 8 // slot_created
        + 1 // bump
        + 16; // reserved
}

/// One executed, intentional, operator-initiated reserve rebalance
/// withdrawal (PDA: [`crate::constants::SEED_REBALANCE_WITHDRAWAL`] +
/// `nonce.to_le_bytes()`). The account's existence is the on-chain replay
/// guard — a given nonce can authorize at most one withdrawal, ever (same
/// discipline as [`DepositClaim`]). Doubles as the permanent, auditable
/// record of every intentional reserve withdrawal — distinct from, and
/// never created by, `release_from_reserve`.
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field               | type       | bytes |
/// |----------------------|------------|-------|
/// | `nonce`                | `u64`      | 8     |
/// | `amount`               | `u64`      | 8     |
/// | `destination`          | `Pubkey`   | 32    |
/// | `admin`                | `Pubkey`   | 32    |
/// | `attestation_epoch`    | `u64`      | 8     |
/// | `protocol_version`     | `u8`       | 1     |
/// | `slot_created`         | `u64`      | 8     |
/// | `bump`                 | `u8`       | 1     |
/// | `reserved`             | `[u8; 16]` | 16    |
#[account]
pub struct RebalanceWithdrawal {
    /// Operator-supplied replay-guard value; also the PDA seed suffix.
    pub nonce: u64,
    /// Withdrawn amount, in the reserve mint's own atomic units.
    pub amount: u64,
    /// The token account the withdrawal was sent to.
    pub destination: Pubkey,
    /// The admin identity that co-authorized this specific withdrawal —
    /// audit trail only; the threshold attestation is the real authority
    /// (see `instructions::rebalance_withdraw` module docs).
    pub admin: Pubkey,
    /// Attestation-key epoch this withdrawal was authorized under.
    pub attestation_epoch: u64,
    /// `BridgeConfig.protocol_version` at withdrawal time.
    pub protocol_version: u8,
    /// Solana slot in which the withdrawal executed.
    pub slot_created: u64,
    /// Canonical PDA bump.
    pub bump: u8,
    /// Expansion space.
    pub reserved: [u8; 16],
}

impl RebalanceWithdrawal {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 8 // nonce
        + 8 // amount
        + 32 // destination
        + 32 // admin
        + 8 // attestation_epoch
        + 1 // protocol_version
        + 8 // slot_created
        + 1 // bump
        + 16; // reserved
}

/// Lifecycle of a withdrawal obligation. Borsh encodes the variant tag as
/// one byte (Pending = 0, Broadcast = 1, Completed = 2).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum WithdrawalStatus {
    /// Deposit-to-reserve executed on Solana; Goldcoin payout not yet
    /// broadcast.
    Pending,
    /// Payout transaction broadcast to the Goldcoin network.
    Broadcast,
    /// Payout confirmed at the required Goldcoin depth, recorded on-chain
    /// via `record_goldcoin_completion`. Terminal.
    Completed,
}

/// Persistent withdrawal-obligation record (PDA:
/// [`crate::constants::SEED_WITHDRAWAL_OBLIGATION`] + `index.to_le_bytes()`).
/// Created atomically with the user's SPL transfer into the reserve by
/// `deposit_to_reserve` — the authoritative payout-obligation record the
/// off-chain service scans, not the event stream (same discipline as the
/// old bridge's `WithdrawalRequest`, ADR-0006).
///
/// **This direction's replay guard has no on-chain backstop** beyond this
/// record's `status` field and the off-chain service's own database
/// constraint (docs/02-trust-model.md asymmetry note, docs/10-threat-model.md)
/// — Goldcoin has no program layer to enforce it independently.
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field               | type               | bytes |
/// |-----------------------|--------------------|-------|
/// | `index`                | `u64`              | 8     |
/// | `amount`               | `u64`              | 8     |
/// | `requester`            | `Pubkey`           | 32    |
/// | `glc_address`          | `[u8; 64]`         | 64    |
/// | `glc_address_len`      | `u8`               | 1     |
/// | `status`               | `WithdrawalStatus` | 1     |
/// | `requested_at_slot`    | `u64`              | 8     |
/// | `protocol_version`     | `u8`               | 1     |
/// | `bump`                 | `u8`               | 1     |
/// | `reserved`             | `[u8; 48]`         | 48    |
#[account]
pub struct WithdrawalObligation {
    /// Monotonic index from `BridgeConfig.obligation_count`; also the PDA
    /// seed suffix.
    pub index: u64,
    /// Deposited amount in atomic GLC units — the payout obligation
    /// (constraint 2: reverse transfers preserve the same 1:1 invariant).
    pub amount: u64,
    /// The wallet that deposited into the reserve; the payout dispute/audit
    /// anchor.
    pub requester: Pubkey,
    /// Opaque ASCII Goldcoin destination, left-justified, zero-padded. Not
    /// semantically validated on-chain (no base58 decoder here — see
    /// `record_goldcoin_completion`'s destination-commitment design).
    pub glc_address: [u8; 64],
    /// Number of meaningful bytes in `glc_address` (1..=64).
    pub glc_address_len: u8,
    /// Always `Pending` at creation.
    pub status: WithdrawalStatus,
    /// Solana slot in which the reserve deposit executed.
    pub requested_at_slot: u64,
    /// `BridgeConfig.protocol_version` at deposit time.
    pub protocol_version: u8,
    /// Canonical PDA bump.
    pub bump: u8,
    /// Expansion space — sized so the payout record (Goldcoin payout txid
    /// 32B + confirmation height 8B) fits without migration, same layout
    /// trick as the old bridge's `WithdrawalRequest::reserved`.
    pub reserved: [u8; 48],
}

const PAYOUT_TXID_OFFSET: usize = 0;
const PAYOUT_TXID_LEN: usize = 32;
const PAYOUT_HEIGHT_OFFSET: usize = 32;
const PAYOUT_HEIGHT_LEN: usize = 8;
const PAYOUT_RECORD_LEN: usize = PAYOUT_TXID_LEN + PAYOUT_HEIGHT_LEN;

impl WithdrawalObligation {
    /// The recorded Goldcoin payout, or `None` while not completed.
    pub fn payout_record(&self) -> Option<([u8; 32], u64)> {
        if self.status != WithdrawalStatus::Completed {
            return None;
        }
        let mut txid = [0u8; PAYOUT_TXID_LEN];
        txid.copy_from_slice(
            &self.reserved[PAYOUT_TXID_OFFSET..PAYOUT_TXID_OFFSET + PAYOUT_TXID_LEN],
        );
        let mut height = [0u8; PAYOUT_HEIGHT_LEN];
        height.copy_from_slice(
            &self.reserved[PAYOUT_HEIGHT_OFFSET..PAYOUT_HEIGHT_OFFSET + PAYOUT_HEIGHT_LEN],
        );
        Some((txid, u64::from_le_bytes(height)))
    }

    /// Whether the payout region is still untouched.
    pub fn payout_record_is_unset(&self) -> bool {
        self.reserved[..PAYOUT_RECORD_LEN].iter().all(|b| *b == 0)
    }

    /// Writes the payout record. Callers must have verified the attestation
    /// and that the record is unset.
    pub fn set_payout_record(&mut self, payout_txid: &[u8; 32], payout_height: u64) {
        self.reserved[PAYOUT_TXID_OFFSET..PAYOUT_TXID_OFFSET + PAYOUT_TXID_LEN]
            .copy_from_slice(payout_txid);
        self.reserved[PAYOUT_HEIGHT_OFFSET..PAYOUT_HEIGHT_OFFSET + PAYOUT_HEIGHT_LEN]
            .copy_from_slice(&payout_height.to_le_bytes());
    }

    pub const SPACE: usize = 8 // Anchor discriminator
        + 8 // index
        + 8 // amount
        + 32 // requester
        + 64 // glc_address
        + 1 // glc_address_len
        + 1 // status
        + 8 // requested_at_slot
        + 1 // protocol_version
        + 1 // bump
        + 48; // reserved
}

/// Per-direction rolling volume tracker (PDA:
/// [`crate::constants::SEED_ROLLING_VOLUME_WINDOW`] + one direction byte).
///
/// Fixed-bucket window (see `BridgeConfig::rolling_window_seconds` doc): a
/// documented simplification of a true sliding window, sized conservatively
/// (docs/09-runbook.md, implementation log).
///
/// Byte layout (borsh, after the 8-byte Anchor discriminator):
///
/// | field           | type   | bytes |
/// |-------------------|--------|-------|
/// | `direction`         | `u8` (enum) | 1     |
/// | `window_start`      | `i64`  | 8     |
/// | `window_total`      | `u64`  | 8     |
/// | `bump`              | `u8`   | 1     |
/// | `reserved`          | `[u8; 16]` | 16 |
#[account]
pub struct RollingVolumeWindow {
    pub direction: Direction,
    /// Unix timestamp the current bucket started.
    pub window_start: i64,
    /// Cumulative amount settled in the current bucket.
    pub window_total: u64,
    pub bump: u8,
    pub reserved: [u8; 16],
}

impl RollingVolumeWindow {
    pub const SPACE: usize = 8 // Anchor discriminator
        + 1 // direction
        + 8 // window_start
        + 8 // window_total
        + 1 // bump
        + 16; // reserved
}

#[cfg(test)]
mod space {
    use super::*;

    #[test]
    fn bridge_config_space_matches_serialized_max() {
        let max = BridgeConfig {
            protocol_version: u8::MAX,
            admin: Pubkey::new_unique(),
            pending_admin: Some(Pubkey::new_unique()),
            paused: true,
            release_paused: true,
            deposit_paused: true,
            bump: u8::MAX,
            reserve_token_mint: Pubkey::new_unique(),
            reserve_token_program: Pubkey::new_unique(),
            reserve_authority_bump: u8::MAX,
            obligation_count: u64::MAX,
            governance_timelock_seconds: i64::MAX,
            min_transfer_amount: u64::MAX,
            per_transfer_limit: u64::MAX,
            protected_minimum: u64::MAX,
            rolling_volume_limit: u64::MAX,
            rolling_window_seconds: i64::MAX,
            upgrade_timelock_seconds: i64::MAX,
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), BridgeConfig::SPACE);
    }

    #[test]
    fn attestation_key_set_space_matches_serialized_max() {
        let max = AttestationKeySet {
            epoch: u64::MAX,
            threshold: u8::MAX,
            bump: u8::MAX,
            keys: vec![Pubkey::new_unique(); MAX_ATTESTATION_KEYS],
            reserved: [0u8; 32],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), AttestationKeySet::SPACE);
    }

    #[test]
    fn pending_governance_action_space_matches_serialized_max() {
        let max = PendingGovernanceAction {
            action: u8::MAX,
            proposed_under_epoch: u64::MAX,
            eta: i64::MAX,
            threshold: u8::MAX,
            keys: vec![Pubkey::new_unique(); MAX_ATTESTATION_KEYS],
            bump: u8::MAX,
            reserved: [0u8; 24],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), PendingGovernanceAction::SPACE);
    }

    #[test]
    fn pending_program_upgrade_space_matches_serialized_max() {
        let max = PendingProgramUpgrade {
            buffer_address: Pubkey::new_unique(),
            proposed_at: i64::MAX,
            eta: i64::MAX,
            proposed_by: Pubkey::new_unique(),
            bump: u8::MAX,
            reserved: [0u8; 16],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), PendingProgramUpgrade::SPACE);
    }

    #[test]
    fn deposit_claim_space_matches_serialized_max() {
        let max = DepositClaim {
            txid: [u8::MAX; 32],
            vout: u32::MAX,
            amount: u64::MAX,
            recipient: Pubkey::new_unique(),
            attestation_epoch: u64::MAX,
            protocol_version: u8::MAX,
            slot_created: u64::MAX,
            bump: u8::MAX,
            reserved: [0u8; 16],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), DepositClaim::SPACE);
    }

    #[test]
    fn rebalance_withdrawal_space_matches_serialized_max() {
        let max = RebalanceWithdrawal {
            nonce: u64::MAX,
            amount: u64::MAX,
            destination: Pubkey::new_unique(),
            admin: Pubkey::new_unique(),
            attestation_epoch: u64::MAX,
            protocol_version: u8::MAX,
            slot_created: u64::MAX,
            bump: u8::MAX,
            reserved: [0u8; 16],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), RebalanceWithdrawal::SPACE);
    }

    #[test]
    fn withdrawal_obligation_space_matches_serialized_max() {
        let max = WithdrawalObligation {
            index: u64::MAX,
            amount: u64::MAX,
            requester: Pubkey::new_unique(),
            glc_address: [u8::MAX; 64],
            glc_address_len: u8::MAX,
            status: WithdrawalStatus::Completed,
            requested_at_slot: u64::MAX,
            protocol_version: u8::MAX,
            bump: u8::MAX,
            reserved: [0u8; 48],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), WithdrawalObligation::SPACE);
    }

    #[test]
    fn rolling_volume_window_space_matches_serialized_max() {
        let max = RollingVolumeWindow {
            direction: Direction::SolanaToGoldcoin,
            window_start: i64::MAX,
            window_total: u64::MAX,
            bump: u8::MAX,
            reserved: [0u8; 16],
        };
        let serialized = max.try_to_vec().unwrap();
        assert_eq!(8 + serialized.len(), RollingVolumeWindow::SPACE);
    }

    #[test]
    fn withdrawal_status_borsh_tags_are_stable() {
        assert_eq!(WithdrawalStatus::Pending.try_to_vec().unwrap(), vec![0]);
        assert_eq!(WithdrawalStatus::Broadcast.try_to_vec().unwrap(), vec![1]);
        assert_eq!(WithdrawalStatus::Completed.try_to_vec().unwrap(), vec![2]);
    }

    #[test]
    fn payout_record_round_trips_through_reserved_bytes() {
        let mut obligation = WithdrawalObligation {
            index: 1,
            amount: 100,
            requester: Pubkey::new_unique(),
            glc_address: [0u8; 64],
            glc_address_len: 0,
            status: WithdrawalStatus::Completed,
            requested_at_slot: 1,
            protocol_version: 1,
            bump: 1,
            reserved: [0u8; 48],
        };
        assert!(obligation.payout_record_is_unset());
        let txid = [0x42u8; 32];
        obligation.set_payout_record(&txid, 12345);
        assert!(!obligation.payout_record_is_unset());
        let (recorded_txid, recorded_height) = obligation.payout_record().unwrap();
        assert_eq!(recorded_txid, txid);
        assert_eq!(recorded_height, 12345);
        // Only the first 40 of 48 reserved bytes are used; the rest stay zero.
        assert!(obligation.reserved[40..].iter().all(|b| *b == 0));
    }
}
