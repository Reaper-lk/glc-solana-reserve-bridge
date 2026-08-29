//! # glc-reserve-bridge — Solana side of the Goldcoin reserve-backed bridge
//!
//! Program-enforced reserve release, authorized by an internal 2-of-3
//! threshold attestation across three genuinely separate custody domains
//! (docs/02-trust-model.md, approved 2026-08-14 — **not federation**; see
//! docs/12-management-decisions.md item 1). No mint, no burn: this program
//! holds a reserve of the EXISTING Solana GLC SPL token and releases it 1:1
//! against independently-verified Goldcoin deposits.
//!
//! Adapted from the old (federated, mint/burn) bridge's `glc_bridge`
//! program — see docs/01-reuse-inventory.md for the component-by-component
//! reuse/replace audit and docs/08-migration-strategy.md for how code
//! moved from that repository into this one.
//!
//! ## Instruction set
//! - [`initialize`] — create `BridgeConfig`, `AttestationKeySet`, and both
//!   `RollingVolumeWindow` accounts; only the program upgrade authority may
//!   call, exactly once.
//! - [`initialize_reserve_vault`] — one-time creation of the reserve token
//!   account for the existing Solana GLC mint; admin-only.
//! - [`set_paused`], [`set_limit`], [`transfer_admin`]/[`accept_admin`] —
//!   admin-gated governance (limit/pause changes are an interim
//!   admin-immediate posture — see IMPLEMENTATION_LOG.md).
//! - [`reset_rolling_volume_window`] — admin-gated, requires the bridge to
//!   already be globally paused; manually reopens ONE direction's rolling
//!   24h volume window without waiting out its remainder — see
//!   `instructions::admin` module docs.
//! - [`propose_attestation_key_rotation`]/[`execute_attestation_key_rotation`]/
//!   [`cancel_attestation_key_rotation`] — threshold-gated, timelocked
//!   attestation-key rotation. Never admin-gated: see
//!   `instructions::governance` module docs.
//! - [`release_from_reserve`] — verify a threshold attestation for a
//!   confirmed Goldcoin deposit `(txid, vout)`, create the per-claim
//!   `DepositClaim` PDA (replay guard), transfer reserve GLC 1:1.
//! - [`deposit_to_reserve`] — user transfers existing Solana GLC into the
//!   reserve; atomically records a `WithdrawalObligation` PDA.
//! - [`record_goldcoin_completion`] — threshold-attested record that an
//!   obligation was paid on Goldcoin. Terminal and irreversible.
//! - [`accept_upgrade_authority`]/[`propose_upgrade`]/[`execute_upgrade`]/
//!   [`cancel_upgrade`] — timelocked program-upgrade mechanism
//!   (docs/12-management-decisions.md item 3, option (c)). Admin-gated to
//!   propose/cancel; permissionless to execute once the timelock has
//!   elapsed; see `instructions::upgrade_timelock` module docs for why
//!   shipping this code does not itself change any real deployment's
//!   actual upgrade authority.
//! - [`rebalance_withdraw`] — intentional, operator-initiated reserve
//!   withdrawal to an explicit destination, structurally distinct from
//!   [`release_from_reserve`]: requires the bridge to already be globally
//!   paused, requires BOTH admin's signature AND a threshold attestation
//!   proof (never admin alone), preserves `protected_minimum`, and is
//!   replay-guarded by an operator-supplied nonce — see
//!   `instructions::rebalance_withdraw` module docs.

use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod limits;
pub mod state;
pub mod token_extensions;
pub mod validation;
pub mod verification;

use instructions::admin::{LimitField, PauseScope};
use instructions::*;
use state::Direction;

// This MUST equal the program's actual deployed mainnet address —
// `glc-reserve-bridge-shared::PROGRAM_ID_BYTES` is the single
// authoritative source of truth for that address (see its own docs for
// why `declare_id!` still needs its own literal rather than referencing
// that constant directly, and docs/22-production-readiness-review.md
// P0-6 for why this matters: this value is baked into the compiled
// binary and used, among other things, as the domain separator for every
// attestation-message signature this program verifies
// (`verification.rs`/`release_from_reserve.rs`/
// `complete_goldcoin_payout.rs`/`governance.rs`'s `crate::ID.to_bytes()`
// calls) — it does NOT need to equal the address this binary happens to
// be deployed at for PDA/account validation to work (Anchor's `seeds =
// [...]` constraint codegen uses the runtime-supplied program id, not
// this compile-time constant, for that), but it DOES need to match
// whatever the off-chain service independently computes the same
// domain separator from, and — for the off-chain service's PDA
// derivations to find the right accounts at all — it should also equal
// the real deployed address. See the `program_id_matches_shared_source_
// of_truth` test below, enforced on every `cargo test`.
declare_id!("6tmLSP2j2thito2RpByqgfKHuVRSLcNd9c5FkrLJMjja");

#[cfg(test)]
mod program_id_tests {
    /// Fails closed (build-breaking on `cargo test`, part of the on-chain
    /// job's CI step) if this file's `declare_id!` literal ever drifts
    /// from `glc-reserve-bridge-shared::PROGRAM_ID_BYTES` — the two
    /// cannot be unified into one literal (see `declare_id!`'s own
    /// comment above), so this test is what actually prevents the drift
    /// docs/22-production-readiness-review.md P0-6 describes from
    /// recurring silently.
    #[test]
    fn program_id_matches_shared_source_of_truth() {
        assert_eq!(
            crate::ID.to_bytes(),
            glc_reserve_bridge_shared::PROGRAM_ID_BYTES
        );
    }
}

#[program]
pub mod glc_reserve_bridge {
    use super::*;

    /// One-time creation of `BridgeConfig`, `AttestationKeySet`, and both
    /// `RollingVolumeWindow` accounts. Caller must be the program upgrade
    /// authority and becomes the initial admin.
    #[allow(clippy::too_many_arguments)]
    pub fn initialize(
        ctx: Context<Initialize>,
        attestation_keys: Vec<Pubkey>,
        threshold: u8,
        governance_timelock_seconds: i64,
        min_transfer_amount: u64,
        per_transfer_limit: u64,
        protected_minimum: u64,
        rolling_volume_limit: u64,
        rolling_window_seconds: i64,
        upgrade_timelock_seconds: i64,
    ) -> Result<()> {
        instructions::initialize::initialize(
            ctx,
            attestation_keys,
            threshold,
            governance_timelock_seconds,
            min_transfer_amount,
            per_transfer_limit,
            protected_minimum,
            rolling_volume_limit,
            rolling_window_seconds,
            upgrade_timelock_seconds,
        )
    }

    /// One-time reserve-vault creation for the existing Solana GLC mint.
    /// Admin-only.
    pub fn initialize_reserve_vault(ctx: Context<InitializeReserveVault>) -> Result<()> {
        instructions::reserve_vault::initialize_reserve_vault(ctx)
    }

    /// Admin-gated circuit breaker (global, release-direction, or
    /// deposit-direction).
    pub fn set_paused(ctx: Context<AdminConfig>, scope: PauseScope, paused: bool) -> Result<()> {
        instructions::admin::set_paused(ctx, scope, paused)
    }

    /// Admin-gated, immediate limit change. See `instructions::admin`
    /// module docs for the interim-posture caveat.
    pub fn set_limit(ctx: Context<AdminConfig>, field: LimitField, new_value: u64) -> Result<()> {
        instructions::admin::set_limit(ctx, field, new_value)
    }

    /// Admin-only step 1 of the two-step admin handover.
    pub fn transfer_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        instructions::admin::transfer_admin(ctx, new_admin)
    }

    /// Step 2 of the handover; only the pending admin may call.
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        instructions::admin::accept_admin(ctx)
    }

    /// Admin-gated administrative override of the rolling-volume anti-drain
    /// protection for one direction. Requires the bridge to already be
    /// globally paused; see `instructions::admin::reset_rolling_volume_window`
    /// doc comment for the full rule.
    pub fn reset_rolling_volume_window(
        ctx: Context<ResetRollingVolumeWindow>,
        direction: Direction,
    ) -> Result<()> {
        instructions::admin::reset_rolling_volume_window(ctx, direction)
    }

    /// Queues an attestation-key rotation behind the governance timelock,
    /// authorized by a threshold attestation proof.
    pub fn propose_attestation_key_rotation(
        ctx: Context<ProposeGovernanceAction>,
        keys: Vec<Pubkey>,
        threshold: u8,
    ) -> Result<()> {
        instructions::governance::propose_attestation_key_rotation(ctx, keys, threshold)
    }

    /// Applies a queued rotation once its timelock has elapsed.
    /// Permissionless: the threshold proof at proposal time was the
    /// authorization.
    pub fn execute_attestation_key_rotation(ctx: Context<ExecuteGovernanceAction>) -> Result<()> {
        instructions::governance::execute_attestation_key_rotation(ctx)
    }

    /// Cancels the pending governance action; requires a fresh threshold
    /// proof.
    pub fn cancel_attestation_key_rotation(ctx: Context<CancelGovernanceAction>) -> Result<()> {
        instructions::governance::cancel_attestation_key_rotation(ctx)
    }

    /// Releases reserve GLC 1:1 for a confirmed Goldcoin deposit, authorized
    /// by a threshold attestation carried in a preceding ed25519
    /// verification instruction. Any fee payer may submit.
    pub fn release_from_reserve(
        ctx: Context<ReleaseFromReserve>,
        txid: [u8; 32],
        vout: u32,
        amount: u64,
        attestation_epoch: u64,
    ) -> Result<()> {
        instructions::release_from_reserve::release_from_reserve(
            ctx,
            txid,
            vout,
            amount,
            attestation_epoch,
        )
    }

    /// Deposits existing Solana GLC into the reserve and atomically records
    /// the payout obligation.
    pub fn deposit_to_reserve(
        ctx: Context<DepositToReserve>,
        amount: u64,
        glc_address: Vec<u8>,
    ) -> Result<()> {
        instructions::deposit_to_reserve::deposit_to_reserve(ctx, amount, glc_address)
    }

    /// Records, under a threshold attestation, that a withdrawal obligation
    /// was paid on Goldcoin. Terminal and irreversible.
    pub fn record_goldcoin_completion(
        ctx: Context<CompleteGoldcoinPayout>,
        index: u64,
        payout_txid: [u8; 32],
        payout_height: u64,
        amount: u64,
        attestation_epoch: u64,
    ) -> Result<()> {
        instructions::complete_goldcoin_payout::record_goldcoin_completion(
            ctx,
            index,
            payout_txid,
            payout_height,
            amount,
            attestation_epoch,
        )
    }

    /// One-time handoff of this program's REAL upgrade authority to its
    /// own signing PDA, arming the timelock mechanism below. Only the
    /// program's current real upgrade authority may call this — see
    /// `instructions::upgrade_timelock` module docs for why nothing in
    /// this codebase ever calls it on a live deployment's behalf.
    pub fn accept_upgrade_authority(ctx: Context<AcceptUpgradeAuthority>) -> Result<()> {
        instructions::upgrade_timelock::accept_upgrade_authority(ctx)
    }

    /// Admin-gated: queues a program upgrade to `buffer_address` behind
    /// `BridgeConfig.upgrade_timelock_seconds`.
    pub fn propose_upgrade(ctx: Context<ProposeUpgrade>, buffer_address: Pubkey) -> Result<()> {
        instructions::upgrade_timelock::propose_upgrade(ctx, buffer_address)
    }

    /// Permissionless once the timelock has elapsed: performs the real
    /// upgrade CPI. Fails closed if `accept_upgrade_authority` was never
    /// called for this deployment.
    pub fn execute_upgrade(ctx: Context<ExecuteUpgrade>) -> Result<()> {
        instructions::upgrade_timelock::execute_upgrade(ctx)
    }

    /// Admin-gated: cancels the pending upgrade at any point before
    /// execution.
    pub fn cancel_upgrade(ctx: Context<CancelUpgrade>) -> Result<()> {
        instructions::upgrade_timelock::cancel_upgrade(ctx)
    }

    /// Intentional, operator-initiated reserve withdrawal to an explicit
    /// destination token account. Requires the bridge to already be
    /// globally paused, admin's signature, AND a threshold attestation
    /// proof over the withdrawal's exact nonce/amount/destination (never
    /// admin alone) — see `instructions::rebalance_withdraw` module docs.
    pub fn rebalance_withdraw(
        ctx: Context<RebalanceWithdraw>,
        nonce: u64,
        amount: u64,
        attestation_epoch: u64,
    ) -> Result<()> {
        instructions::rebalance_withdraw::rebalance_withdraw(ctx, nonce, amount, attestation_epoch)
    }
}
