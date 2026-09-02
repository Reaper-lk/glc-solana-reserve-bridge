//! Governance of the reserve rebalance policy: the treasury-destination
//! allowlist and the dedicated withdrawal limits that
//! [`crate::instructions::treasury_withdraw`] enforces.
//!
//! # Why this is threshold-gated and not admin-gated
//!
//! Exactly the reasoning [`crate::instructions::governance`] gives for
//! attestation-key rotation, applied to the other thing that would make the
//! threshold model decorative if one key could change it.
//!
//! An allowlist an admin could edit alone is not an allowlist. A compromised
//! admin would simply add a token account they control, wait, and then take
//! the ordinary treasury-withdrawal path — fully audited, fully attested,
//! and fully useless as a control. The 2026-09-02 incident is precisely the
//! shape of an attacker taking a legitimate path with legitimate
//! credentials, so the mitigation had to be something legitimate
//! credentials do not reach.
//!
//! The same argument covers the limits. A per-withdrawal ceiling the admin
//! can raise is not a ceiling; it is a speed bump with a documented bypass.
//!
//! So: no admin signature authorizes anything in this module. Creation
//! requires a threshold attestation. Every subsequent change requires a
//! threshold attestation AND `BridgeConfig.governance_timelock_seconds` of
//! publicly observable delay, during which any quorum can cancel it.
//!
//! # Why creation is not timelocked but every change is
//!
//! [`initialize_rebalance_policy`] can only ever move the bridge from "no
//! allowlist exists, so `treasury_withdraw` refuses every destination" to
//! "these destinations are permitted". It cannot loosen anything, because
//! before it runs nothing is permitted at all. A timelock on it would delay
//! the restoration of a legitimate operational capability without
//! protecting anything.
//!
//! [`propose_rebalance_policy`] CAN loosen things — it can add a
//! destination or raise a limit — so it gets the full timelock. Its action
//! byte is deliberately distinct from initialization's, so an approval to
//! create the first policy can never be replayed as an approval to replace
//! an existing one.
//!
//! # Flow
//!
//! ```text
//!   initialize_rebalance_policy   (threshold proof; one-time; not timelocked)
//!            |
//!            v
//!   propose_rebalance_policy      (threshold proof over the CURRENT epoch)
//!            |  creates the singleton PendingRebalancePolicy { eta }
//!            v
//!     ... timelock window: publicly observable, cancellable ...
//!            |
//!   execute_rebalance_policy      (permissionless once eta has passed)
//!            |  applies the policy, increments version, closes the account
//!            v
//!   cancel_rebalance_policy       (fresh threshold proof, any time before execution)
//! ```
//!
//! Execution is permissionless for the same reason it is in
//! `instructions::governance`: the authorization was the threshold proof at
//! proposal time, and the delay is what gave observers their window.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_lang::solana_program::sysvar::instructions::{
    get_instruction_relative, ID as INSTRUCTIONS_SYSVAR_ID,
};
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use glc_reserve_bridge_shared::governance::{
    cancel_params, governance_message, rebalance_policy_params, ACTION_CANCEL_REBALANCE_POLICY,
    ACTION_INITIALIZE_REBALANCE_POLICY, ACTION_PROPOSE_REBALANCE_POLICY,
};

use crate::constants::{
    MAX_TREASURY_DESTINATIONS, SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG,
    SEED_PENDING_REBALANCE_POLICY, SEED_REBALANCE_POLICY, SEED_RESERVE_AUTHORITY,
};
use crate::errors::BridgeError;
use crate::events::{
    RebalancePolicyCancelled, RebalancePolicyExecuted, RebalancePolicyInitialized,
    RebalancePolicyProposed,
};
use crate::state::{AttestationKeySet, BridgeConfig, PendingRebalancePolicy, RebalancePolicy};
use crate::validation::validate_rebalance_policy;
use crate::verification::count_unique_attestation_signers;

/// Byte-for-byte the same threshold check `release_from_reserve` and
/// `instructions::governance` perform. Duplicated as a small private
/// helper rather than imported across modules only because
/// `governance::require_threshold_approval` is private to that module;
/// the underlying `count_unique_attestation_signers` — where all the
/// actual parsing rigour lives — is the single shared implementation.
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

/// Copies a validated allowlist into the fixed-size on-chain array,
/// zeroing the unused tail so a shrinking policy leaves no stale address
/// behind. `RebalancePolicy::is_allowlisted` only consults the first
/// `treasury_count` entries anyway, but leaving a live treasury address
/// sitting in the tail of a governed account is the kind of thing that
/// eventually gets read by something that forgets the count.
fn pack_treasuries(treasuries: &[Pubkey]) -> [Pubkey; MAX_TREASURY_DESTINATIONS] {
    let mut packed = [Pubkey::default(); MAX_TREASURY_DESTINATIONS];
    for (slot, t) in packed.iter_mut().zip(treasuries.iter()) {
        *slot = *t;
    }
    packed
}

// ------------------------------------------------------------- initialize --

#[derive(Accounts)]
pub struct InitializeRebalancePolicy<'info> {
    /// Any fee payer; funds the policy account's rent. Confers NO
    /// authority — the threshold proof is the only authorization, exactly
    /// as in `propose_attestation_key_rotation`. Deliberately not required
    /// to be the admin: tying this to the admin key would put the creation
    /// of the allowlist inside the blast radius the allowlist exists to
    /// contain.
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.reserve_token_mint != Pubkey::default()
            @ BridgeError::ReserveNotConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// Singleton: `init` fails if a policy already exists, so this can
    /// never be used to replace one (that is what the timelocked
    /// propose/execute path is for).
    #[account(
        init,
        payer = payer,
        space = RebalancePolicy::SPACE,
        seeds = [SEED_REBALANCE_POLICY],
        bump
    )]
    pub rebalance_policy: Account<'info, RebalancePolicy>,

    #[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]
    pub reserve_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: data-less PDA, sole authority over the reserve token account.
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    /// Present only so the reserve vault's address can be derived and
    /// refused as a treasury destination.
    #[account(
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
        associated_token::token_program = token_program,
    )]
    pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// One-time creation of the reserve rebalance policy. Until this succeeds,
/// `treasury_withdraw` refuses every destination.
pub fn initialize_rebalance_policy(
    ctx: Context<InitializeRebalancePolicy>,
    treasuries: Vec<Pubkey>,
    rolling_limit: u64,
    rolling_window_seconds: i64,
) -> Result<()> {
    // The reserve vault address is DERIVED from the account context's own
    // `associated_token::` constraints (reserve authority PDA + configured
    // mint + configured token program), never accepted as an argument, so
    // a caller cannot dodge `validate_rebalance_policy`'s refusal to
    // allowlist the vault itself by naming some other account here.
    let reserve_token_account = ctx.accounts.reserve_token_account.key();
    validate_rebalance_policy(
        &treasuries,
        rolling_limit,
        rolling_window_seconds,
        &reserve_token_account,
    )?;

    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;

    let raw: Vec<[u8; 32]> = treasuries.iter().map(|t| t.to_bytes()).collect();
    let params_commitment = hash(&rebalance_policy_params(
        &raw,
        rolling_limit,
        rolling_window_seconds,
    ))
    .to_bytes();
    let expected_message = governance_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        ACTION_INITIALIZE_REBALANCE_POLICY,
        &params_commitment,
    );
    require_threshold_approval(
        &ctx.accounts.instructions_sysvar.to_account_info(),
        key_set,
        &expected_message,
    )?;

    let now = Clock::get()?.unix_timestamp;
    let policy = &mut ctx.accounts.rebalance_policy;
    policy.version = 0;
    policy.bump = ctx.bumps.rebalance_policy;
    policy.treasury_count = treasuries.len() as u8;
    policy.treasuries = pack_treasuries(&treasuries);
    policy.rolling_limit = rolling_limit;
    policy.rolling_window_seconds = rolling_window_seconds;
    // The budget starts full, from the real on-chain clock — never a
    // caller-supplied timestamp.
    policy.window_start = now;
    policy.window_total = 0;
    policy.reserved = [0u8; 64];

    emit!(RebalancePolicyInitialized {
        version: policy.version,
        treasuries,
        rolling_limit,
        rolling_window_seconds,
    });
    Ok(())
}

// ---------------------------------------------------------------- propose --

#[derive(Accounts)]
pub struct ProposeRebalancePolicy<'info> {
    /// Any fee payer; funds the pending account's rent. Confers no
    /// authority.
    #[account(mut)]
    pub proposer: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// The policy being replaced must already exist — a proposal to change
    /// something that was never created is a mistake, and treating it as
    /// an implicit initialization would bypass the distinct action byte
    /// that keeps the two approvals apart.
    #[account(seeds = [SEED_REBALANCE_POLICY], bump = rebalance_policy.bump)]
    pub rebalance_policy: Account<'info, RebalancePolicy>,

    /// Singleton: `init` fails if a policy change is already pending, so a
    /// briefly-compromised quorum cannot queue a backlog of allowlist
    /// changes that mature later.
    #[account(
        init,
        payer = proposer,
        space = PendingRebalancePolicy::SPACE,
        seeds = [SEED_PENDING_REBALANCE_POLICY],
        bump
    )]
    pub pending_rebalance_policy: Account<'info, PendingRebalancePolicy>,

    #[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]
    pub reserve_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: data-less PDA, sole authority over the reserve token account.
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    #[account(
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
        associated_token::token_program = token_program,
    )]
    pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

/// Queues a policy replacement behind the governance timelock. Parameters
/// are validated here, at proposal time — an invalid policy must never sit
/// in the queue looking legitimate.
pub fn propose_rebalance_policy(
    ctx: Context<ProposeRebalancePolicy>,
    treasuries: Vec<Pubkey>,
    rolling_limit: u64,
    rolling_window_seconds: i64,
) -> Result<()> {
    // The reserve vault address is DERIVED from the account context's own
    // `associated_token::` constraints (reserve authority PDA + configured
    // mint + configured token program), never accepted as an argument, so
    // a caller cannot dodge `validate_rebalance_policy`'s refusal to
    // allowlist the vault itself by naming some other account here.
    let reserve_token_account = ctx.accounts.reserve_token_account.key();
    validate_rebalance_policy(
        &treasuries,
        rolling_limit,
        rolling_window_seconds,
        &reserve_token_account,
    )?;

    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;
    require!(
        config.governance_timelock_seconds > 0,
        BridgeError::ZeroGovernanceTimelock
    );

    let raw: Vec<[u8; 32]> = treasuries.iter().map(|t| t.to_bytes()).collect();
    let params_commitment = hash(&rebalance_policy_params(
        &raw,
        rolling_limit,
        rolling_window_seconds,
    ))
    .to_bytes();
    let expected_message = governance_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        ACTION_PROPOSE_REBALANCE_POLICY,
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

    let current_version = ctx.accounts.rebalance_policy.version;
    let pending = &mut ctx.accounts.pending_rebalance_policy;
    pending.proposed_under_epoch = key_set.epoch;
    pending.eta = eta;
    pending.treasury_count = treasuries.len() as u8;
    pending.treasuries = pack_treasuries(&treasuries);
    pending.rolling_limit = rolling_limit;
    pending.rolling_window_seconds = rolling_window_seconds;
    pending.bump = ctx.bumps.pending_rebalance_policy;
    pending.reserved = [0u8; 32];

    emit!(RebalancePolicyProposed {
        current_version,
        eta,
        treasuries,
        rolling_limit,
        rolling_window_seconds,
    });
    Ok(())
}

// ---------------------------------------------------------------- execute --

#[derive(Accounts)]
pub struct ExecuteRebalancePolicy<'info> {
    /// Permissionless: rent from the closed pending account is refunded
    /// here. Confers no authority — the threshold proof at proposal time
    /// was the authorization, and the delay was the safeguard.
    #[account(mut)]
    pub executor: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    #[account(mut, seeds = [SEED_REBALANCE_POLICY], bump = rebalance_policy.bump)]
    pub rebalance_policy: Account<'info, RebalancePolicy>,

    #[account(
        mut,
        close = executor,
        seeds = [SEED_PENDING_REBALANCE_POLICY],
        bump = pending_rebalance_policy.bump
    )]
    pub pending_rebalance_policy: Account<'info, PendingRebalancePolicy>,

    #[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]
    pub reserve_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: data-less PDA, sole authority over the reserve token account.
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    #[account(
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
        associated_token::token_program = token_program,
    )]
    pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,

    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
}

/// Applies a queued policy replacement once its timelock has elapsed.
pub fn execute_rebalance_policy(ctx: Context<ExecuteRebalancePolicy>) -> Result<()> {
    let pending = &ctx.accounts.pending_rebalance_policy;
    require!(
        Clock::get()?.unix_timestamp >= pending.eta,
        BridgeError::RebalancePolicyTimelockNotElapsed
    );
    // A rotation of the attestation keys invalidates every queued policy
    // change: the quorum that approved this is not necessarily the quorum
    // that exists now.
    require!(
        ctx.accounts.attestation_key_set.epoch == pending.proposed_under_epoch,
        BridgeError::StaleRebalancePolicyProposal
    );

    let count = usize::from(pending.treasury_count).min(MAX_TREASURY_DESTINATIONS);
    let treasuries: Vec<Pubkey> = pending.treasuries[..count].to_vec();
    // Re-validate at execution: the invariants were checked at proposal
    // time, but this is the last point before the allowlist actually
    // changes, and the reserve vault address is re-derived from live
    // accounts rather than remembered from the proposal.
    validate_rebalance_policy(
        &treasuries,
        pending.rolling_limit,
        pending.rolling_window_seconds,
        &ctx.accounts.reserve_token_account.key(),
    )?;

    let (rolling_limit, rolling_window_seconds) =
        (pending.rolling_limit, pending.rolling_window_seconds);

    let policy = &mut ctx.accounts.rebalance_policy;
    let previous_version = policy.version;
    policy.version = policy
        .version
        .checked_add(1)
        .ok_or(BridgeError::ArithmeticOverflow)?;
    policy.treasury_count = count as u8;
    policy.treasuries = pack_treasuries(&treasuries);
    policy.rolling_limit = rolling_limit;
    policy.rolling_window_seconds = rolling_window_seconds;
    // `window_start`/`window_total` are deliberately NOT reset. A policy
    // update is not a budget top-up: resetting here would let a quorum
    // refill an exhausted withdrawal budget by re-approving the policy it
    // already has, which is the one thing the rolling limit exists to
    // prevent. The window only ever ages out on its own.

    emit!(RebalancePolicyExecuted {
        previous_version,
        new_version: policy.version,
        treasuries,
        rolling_limit,
        rolling_window_seconds,
    });
    Ok(())
}

// ----------------------------------------------------------------- cancel --

#[derive(Accounts)]
pub struct CancelRebalancePolicy<'info> {
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
        seeds = [SEED_PENDING_REBALANCE_POLICY],
        bump = pending_rebalance_policy.bump
    )]
    pub pending_rebalance_policy: Account<'info, PendingRebalancePolicy>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,
}

/// Cancels the pending policy change, freeing the singleton slot. Requires
/// a fresh threshold proof binding the specific `eta` being cancelled, so a
/// cancel signature cannot be replayed against a later re-proposal.
pub fn cancel_rebalance_policy(ctx: Context<CancelRebalancePolicy>) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;
    let pending = &ctx.accounts.pending_rebalance_policy;

    let params_commitment =
        hash(&cancel_params(ACTION_PROPOSE_REBALANCE_POLICY, pending.eta)).to_bytes();
    let expected_message = governance_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        ACTION_CANCEL_REBALANCE_POLICY,
        &params_commitment,
    );
    require_threshold_approval(
        &ctx.accounts.instructions_sysvar.to_account_info(),
        key_set,
        &expected_message,
    )?;

    emit!(RebalancePolicyCancelled { eta: pending.eta });
    Ok(())
}
