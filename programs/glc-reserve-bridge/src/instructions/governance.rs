//! Threshold-gated, timelocked governance: attestation-key rotation.
//!
//! # Why this cannot be admin-gated
//!
//! The attestation key set is what makes "no single key can release
//! reserves" true. An admin able to rotate it unilaterally could install
//! attacker-controlled keys and mint an effectively single-key release
//! path through the back door — exactly the property the approved trust
//! model (docs/02-trust-model.md, docs/12-management-decisions.md item 1)
//! rules out. Rotation therefore requires the same threshold proof as a
//! reserve release, and must sit visibly in a timelock before it takes
//! effect, so a compromised subset attempting a hostile rotation gives
//! operators a window to notice and pause.
//!
//! Reused near-verbatim from the old bridge's governance module
//! (docs/01-reuse-inventory.md: the propose/timelock/permissionless-execute
//! pattern is authority-agnostic and directly reusable) — the
//! ed25519-precompile verification path is the same one `release_from_reserve`
//! uses, so governance inherits every property that path is tested for
//! rather than introducing a second authorization mechanism.
//!
//! # Flow
//!
//! ```text
//!   propose_attestation_key_rotation   (threshold proof over the CURRENT epoch)
//!            |  creates the singleton PendingGovernanceAction { eta }
//!            v
//!     ... timelock window: publicly observable, cancellable ...
//!            |
//!   execute_attestation_key_rotation   (permissionless once eta has passed)
//!            |  applies the set, increments the epoch, closes the account
//!            v
//!   cancel_attestation_key_rotation    (threshold proof, any time before execution)
//! ```
//!
//! Execution is deliberately permissionless: the authorization was the
//! threshold proof at proposal time, and the delay is what gives observers
//! their window.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_lang::solana_program::sysvar::instructions::{
    get_instruction_relative, ID as INSTRUCTIONS_SYSVAR_ID,
};
use glc_reserve_bridge_shared::governance::{
    cancel_params, governance_message, rotation_params, ACTION_CANCEL_ROTATION,
    ACTION_PROPOSE_ROTATION,
};

use crate::constants::{SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_GOVERNANCE_ACTION};
use crate::errors::BridgeError;
use crate::events::{
    GovernanceActionCancelled, GovernanceActionExecuted, GovernanceActionProposed,
};
use crate::state::{AttestationKeySet, BridgeConfig, PendingGovernanceAction};
use crate::validation::validate_attestation_key_set;
use crate::verification::count_unique_attestation_signers;

/// Verifies that the instruction immediately before this one is an ed25519
/// precompile instruction carrying at least `threshold` unique current
/// attestation-key signatures over `expected_message`. Byte-for-byte the
/// same check `release_from_reserve` performs, extracted here so both
/// paths provably share one implementation.
fn require_threshold_approval(
    instructions_sysvar: &AccountInfo,
    key_set: &AttestationKeySet,
    expected_message: &[u8],
) -> Result<()> {
    let verification_ix = get_instruction_relative(-1, instructions_sysvar)
        .map_err(|_| BridgeError::MissingSignatureVerification)?;
    require!(
        verification_ix.program_id == anchor_lang::solana_program::ed25519_program::ID,
        BridgeError::MissingSignatureVerification
    );
    let signer_count =
        count_unique_attestation_signers(&verification_ix.data, expected_message, &key_set.keys)?;
    require!(
        signer_count >= usize::from(key_set.threshold),
        BridgeError::InsufficientSignatures
    );
    Ok(())
}

// ---------------------------------------------------------------- propose --

#[derive(Accounts)]
pub struct ProposeGovernanceAction<'info> {
    /// Any fee payer; funds the pending-action account's rent. Confers no
    /// authority — the threshold proof is the only authorization.
    #[account(mut)]
    pub proposer: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// Singleton: `init` fails if an action is already pending.
    #[account(
        init,
        payer = proposer,
        space = PendingGovernanceAction::SPACE,
        seeds = [SEED_GOVERNANCE_ACTION],
        bump
    )]
    pub pending_action: Account<'info, PendingGovernanceAction>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

/// Queues an attestation-key rotation behind the timelock. Parameters are
/// validated here, at proposal time — an invalid set must never sit in the
/// queue looking legitimate.
pub fn propose_attestation_key_rotation(
    ctx: Context<ProposeGovernanceAction>,
    keys: Vec<Pubkey>,
    threshold: u8,
) -> Result<()> {
    validate_attestation_key_set(&keys, threshold)?;

    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;

    // The signed message commits to a hash of the parameters rather than
    // the parameters themselves, keeping the signed message a fixed length
    // regardless of key count (see shared::governance module docs).
    let raw: Vec<[u8; 32]> = keys.iter().map(|k| k.to_bytes()).collect();
    let params_commitment = hash(&rotation_params(threshold, &raw)).to_bytes();
    let expected_message = governance_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        ACTION_PROPOSE_ROTATION,
        &params_commitment,
    );
    require_threshold_approval(
        &ctx.accounts.instructions_sysvar.to_account_info(),
        key_set,
        &expected_message,
    )?;

    let eta = Clock::get()?
        .unix_timestamp
        .checked_add(config.governance_timelock_seconds)
        .ok_or(BridgeError::ArithmeticOverflow)?;

    let key_count = keys.len() as u8;
    let pending = &mut ctx.accounts.pending_action;
    pending.action = ACTION_PROPOSE_ROTATION;
    pending.proposed_under_epoch = key_set.epoch;
    pending.eta = eta;
    pending.threshold = threshold;
    pending.keys = keys;
    pending.bump = ctx.bumps.pending_action;
    pending.reserved = [0u8; 24];

    emit!(GovernanceActionProposed {
        action: ACTION_PROPOSE_ROTATION,
        proposed_under_epoch: pending.proposed_under_epoch,
        eta,
        threshold,
        attestation_key_count: key_count,
    });
    Ok(())
}

// ---------------------------------------------------------------- execute --

#[derive(Accounts)]
pub struct ExecuteGovernanceAction<'info> {
    /// Permissionless: rent is refunded here. Confers no authority.
    #[account(mut)]
    pub executor: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(mut, seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    #[account(
        mut,
        close = executor,
        seeds = [SEED_GOVERNANCE_ACTION],
        bump = pending_action.bump
    )]
    pub pending_action: Account<'info, PendingGovernanceAction>,
}

/// Applies a queued rotation once its timelock has elapsed.
pub fn execute_attestation_key_rotation(ctx: Context<ExecuteGovernanceAction>) -> Result<()> {
    let pending = &ctx.accounts.pending_action;
    require!(
        pending.action == ACTION_PROPOSE_ROTATION,
        BridgeError::WrongGovernanceAction
    );
    require!(
        Clock::get()?.unix_timestamp >= pending.eta,
        BridgeError::GovernanceTimelockNotElapsed
    );

    let set = &mut ctx.accounts.attestation_key_set;
    require!(
        set.epoch == pending.proposed_under_epoch,
        BridgeError::StaleGovernanceProposal
    );

    // Re-validate at execution: invariants were checked at proposal time,
    // but this is the last point before the key set actually changes.
    validate_attestation_key_set(&pending.keys, pending.threshold)?;

    set.epoch = set
        .epoch
        .checked_add(1)
        .ok_or(BridgeError::ArithmeticOverflow)?;
    set.threshold = pending.threshold;
    set.keys = pending.keys.clone();

    emit!(GovernanceActionExecuted {
        action: ACTION_PROPOSE_ROTATION,
        new_epoch: set.epoch,
    });
    Ok(())
}

// ----------------------------------------------------------------- cancel --

#[derive(Accounts)]
pub struct CancelGovernanceAction<'info> {
    /// Rent refund recipient. Confers no authority — cancellation is
    /// authorized by the threshold proof alone.
    #[account(mut)]
    pub canceller: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    #[account(
        mut,
        close = canceller,
        seeds = [SEED_GOVERNANCE_ACTION],
        bump = pending_action.bump
    )]
    pub pending_action: Account<'info, PendingGovernanceAction>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

/// Cancels the pending action, freeing the singleton slot. Requires a
/// fresh threshold proof binding the specific action and `eta` being
/// cancelled, so a cancel signature cannot be replayed against a later
/// re-proposal.
pub fn cancel_attestation_key_rotation(ctx: Context<CancelGovernanceAction>) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;
    let pending = &ctx.accounts.pending_action;

    let params_commitment = hash(&cancel_params(pending.action, pending.eta)).to_bytes();
    let expected_message = governance_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        ACTION_CANCEL_ROTATION,
        &params_commitment,
    );
    require_threshold_approval(
        &ctx.accounts.instructions_sysvar.to_account_info(),
        key_set,
        &expected_message,
    )?;

    emit!(GovernanceActionCancelled {
        action: pending.action,
        eta: pending.eta,
    });
    Ok(())
}
