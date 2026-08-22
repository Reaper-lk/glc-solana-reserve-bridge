//! Intentional, operator-initiated reserve withdrawal — structurally
//! distinct from [`crate::instructions::release_from_reserve`]: no Goldcoin
//! deposit is being settled, there is no recipient bound by prior
//! independent chain observation, and this action requires the bridge to
//! already be globally paused before it can even be attempted.
//!
//! **Authorization is deliberately NOT a single admin key.** Two
//! independent factors are both required, reusing the exact same
//! mechanisms this program already uses elsewhere rather than inventing a
//! new authorization primitive:
//!
//! 1. `admin` must sign the transaction (accountability: someone specific
//!    is on record as having initiated this withdrawal) — but admin's
//!    signature alone authorizes nothing here, unlike `set_paused`/
//!    `set_limit`.
//! 2. The transaction must additionally carry a valid `attestation_key_set
//!    .threshold`-of-N ed25519 threshold-attestation proof (the identical
//!    mechanism [`crate::instructions::release_from_reserve`] uses),
//!    verified over the canonical
//!    [`glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message`]
//!    — a message a distinct, third-party set of attestation signers must
//!    independently sign, binding the exact nonce/amount/destination/mint
//!    being authorized. Admin cannot forge these signatures; the
//!    attestation signers cannot move funds without admin's participation
//!    either (admin's own signature is still checked). Neither party alone
//!    is sufficient.
//!
//! `BridgeConfig.paused` (the global circuit breaker) must already be
//! `true` — set via the pre-existing, separately-audited `set_paused`
//! instruction — before this instruction will even attempt authorization.
//! This is a deliberate two-step: pausing is its own visible, logged
//! action distinct from the withdrawal itself.
//!
//! Protected accounting is preserved exactly as
//! [`crate::instructions::release_from_reserve`] preserves it: the same
//! [`crate::limits::enforce_protected_minimum`] function is called with the
//! same live on-chain balance, so a rebalance withdrawal can never take the
//! reserve below `protected_minimum` any more than a bridge settlement can.
//!
//! Replay is prevented by the same mechanism as [`crate::state::DepositClaim`]:
//! the `rebalance_withdrawal` PDA's `init` constraint makes reusing a given
//! `nonce` structurally impossible.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    get_instruction_relative, ID as INSTRUCTIONS_SYSVAR_ID,
};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use glc_reserve_bridge_shared::claim::rebalance_withdraw_claim_message;

use crate::constants::{
    SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_REBALANCE_WITHDRAWAL, SEED_RESERVE_AUTHORITY,
};
use crate::errors::BridgeError;
use crate::events::RebalanceWithdrawalExecuted;
use crate::limits::enforce_protected_minimum;
use crate::state::{AttestationKeySet, BridgeConfig, RebalanceWithdrawal};
use crate::token_extensions::{validate_mint_extensions, validate_token_account_extensions};
use crate::verification::count_unique_attestation_signers;

#[derive(Accounts)]
#[instruction(nonce: u64)]
pub struct RebalanceWithdraw<'info> {
    /// Co-authorizes this specific withdrawal and pays the withdrawal
    /// record's rent. Signature alone confers no authority — a valid
    /// threshold attestation is additionally required (module docs above).
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin,
        constraint = bridge_config.reserve_token_mint != Pubkey::default()
            @ BridgeError::ReserveNotConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// Existence of this account is the on-chain replay guard: reusing a
    /// `nonce` fails right here at `init`, exactly like `DepositClaim`.
    #[account(
        init,
        payer = admin,
        space = RebalanceWithdrawal::SPACE,
        seeds = [SEED_REBALANCE_WITHDRAWAL, &nonce.to_le_bytes()],
        bump
    )]
    pub rebalance_withdrawal: Account<'info, RebalanceWithdrawal>,

    #[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]
    pub reserve_mint: InterfaceAccount<'info, Mint>,

    /// CHECK: data-less PDA, sole authority over the reserve token account.
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
        associated_token::token_program = token_program,
    )]
    pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,

    /// The withdrawal's destination. Deliberately a plain `token::` account
    /// constraint, not `associated_token::` — an operator recovery
    /// destination is not required to be an ATA of any particular wallet,
    /// only a real, existing token account for the reserve mint under the
    /// configured token program. This structurally rejects the mint
    /// address (wrong account layout/size) and the program id (wrong
    /// owning program) as destinations — neither can deserialize as a
    /// `TokenAccount` owned by `token_program` — without a manual address
    /// blocklist.
    #[account(
        mut,
        token::mint = reserve_mint,
        token::token_program = token_program,
    )]
    pub destination_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn rebalance_withdraw(
    ctx: Context<RebalanceWithdraw>,
    nonce: u64,
    amount: u64,
    attestation_epoch: u64,
) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;

    // The bridge must already be paused — a separate, independently
    // logged/audited action (`set_paused`) — before a rebalance withdrawal
    // can even be attempted. Not merely "paused implies no concurrent
    // settlement race"; it is a deliberate precondition the operator must
    // consciously satisfy first.
    require!(config.paused, BridgeError::BridgeNotPaused);
    require!(
        attestation_epoch == key_set.epoch,
        BridgeError::StaleAttestationEpoch
    );
    require!(amount > 0, BridgeError::ZeroRebalanceAmount);
    require!(
        ctx.accounts.destination_token_account.key() != ctx.accounts.reserve_token_account.key(),
        BridgeError::RebalanceDestinationIsReserveItself
    );

    // Re-reviewed on every call, not just at vault setup — same discipline
    // as `release_from_reserve`.
    validate_mint_extensions(&ctx.accounts.reserve_mint.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.reserve_token_account.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.destination_token_account.to_account_info())?;

    // Protected accounting (constraint 6): identical function, identical
    // live-balance read, as `release_from_reserve` — an operator withdrawal
    // can no more breach `protected_minimum` than a bridge settlement can.
    let reserve_balance_before = ctx.accounts.reserve_token_account.amount;
    enforce_protected_minimum(reserve_balance_before, config.protected_minimum, amount)?;

    // The threshold attestation: the instruction directly before this one
    // must be the ed25519 precompile carrying >= threshold unique current
    // attestation-key signatures over exactly the canonical withdrawal
    // claim bytes — the same verification mechanism, and the same
    // threshold, `release_from_reserve` uses, applied to a message that
    // can never be confused with a release claim (distinct action byte and
    // distinct total length; see `shared::claim` module docs).
    let verification_ix =
        get_instruction_relative(-1, &ctx.accounts.instructions_sysvar.to_account_info())
            .map_err(|_| BridgeError::MissingSignatureVerification)?;
    require!(
        verification_ix.program_id == anchor_lang::solana_program::ed25519_program::ID,
        BridgeError::MissingSignatureVerification
    );
    let expected_message = rebalance_withdraw_claim_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        nonce,
        amount,
        &ctx.accounts.destination_token_account.key().to_bytes(),
        &config.reserve_token_mint.to_bytes(),
    );
    let signer_count =
        count_unique_attestation_signers(&verification_ix.data, &expected_message, &key_set.keys)?;
    require!(
        signer_count >= usize::from(key_set.threshold),
        BridgeError::InsufficientSignatures
    );

    let bump = config.reserve_authority_bump;
    let seeds: &[&[u8]] = &[SEED_RESERVE_AUTHORITY, &[bump]];
    let signer_seeds: &[&[&[u8]]] = &[seeds];
    let cpi_accounts = TransferChecked {
        from: ctx.accounts.reserve_token_account.to_account_info(),
        mint: ctx.accounts.reserve_mint.to_account_info(),
        to: ctx.accounts.destination_token_account.to_account_info(),
        authority: ctx.accounts.reserve_authority.to_account_info(),
    };
    let cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        cpi_accounts,
        signer_seeds,
    );
    token_interface::transfer_checked(cpi_ctx, amount, ctx.accounts.reserve_mint.decimals)?;

    let record = &mut ctx.accounts.rebalance_withdrawal;
    record.nonce = nonce;
    record.amount = amount;
    record.destination = ctx.accounts.destination_token_account.key();
    record.admin = ctx.accounts.admin.key();
    record.attestation_epoch = key_set.epoch;
    record.protocol_version = config.protocol_version;
    record.slot_created = Clock::get()?.slot;
    record.bump = ctx.bumps.rebalance_withdrawal;
    record.reserved = [0u8; 16];

    // Safe: `enforce_protected_minimum` above already proved
    // `reserve_balance_before >= amount + protected_minimum >= amount`.
    let reserve_balance_after = reserve_balance_before
        .checked_sub(amount)
        .ok_or(BridgeError::ArithmeticUnderflow)?;

    emit!(RebalanceWithdrawalExecuted {
        nonce,
        destination: record.destination,
        amount,
        attestation_epoch: record.attestation_epoch,
        admin: record.admin,
        reserve_balance_after,
    });
    Ok(())
}
