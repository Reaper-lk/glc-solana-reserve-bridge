//! Production Goldcoin-deposit -> Solana-reserve-release path. Replaces the
//! old bridge's `mint_wrapped` (docs/01-reuse-inventory.md: same mechanism,
//! reserve transfer instead of mint).
//!
//! Authorization is an internal 2-of-3 threshold attestation (docs/02-
//! trust-model.md, approved — NOT federation): the transaction carries one
//! ed25519-precompile instruction, immediately before this one, in which at
//! least `threshold` distinct current attestation keys signed the
//! canonical release-claim message ([`glc_reserve_bridge_shared::claim`]).
//! The runtime verified the signatures before execution;
//! [`crate::verification`] proves what was signed and by whom, and each
//! signer independently re-derived the claim from its own Goldcoin chain
//! read before signing (constraint 3, 4 — off-chain, enforced by policy
//! this program cannot see, but the signature itself is the on-chain proof
//! that policy was followed).
//!
//! Constraint 1 (1 Solana GLC released requires 1 corresponding GLC locked
//! on the source side) is enforced by: the attestation only exists if
//! signers independently verified the Goldcoin deposit; the `DepositClaim`
//! PDA `init` constraint below makes a second release for the same
//! `(txid, vout)` structurally impossible (constraint 5); and constraint 6
//! (reserve insufficiency fails closed) is enforced by
//! [`crate::limits::enforce_protected_minimum`] before any transfer happens.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::instructions::{
    get_instruction_relative, ID as INSTRUCTIONS_SYSVAR_ID,
};
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};
use glc_reserve_bridge_shared::claim::release_claim_message;

use crate::constants::{
    SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_DEPOSIT_CLAIM, SEED_RESERVE_AUTHORITY,
    SEED_ROLLING_VOLUME_WINDOW,
};
use crate::errors::BridgeError;
use crate::events::ReserveReleased;
use crate::limits::{
    enforce_and_record_rolling_volume, enforce_protected_minimum, enforce_transfer_amount,
};
use crate::state::{AttestationKeySet, BridgeConfig, DepositClaim, RollingVolumeWindow};
use crate::token_extensions::{validate_mint_extensions, validate_token_account_extensions};
use crate::verification::count_unique_attestation_signers;

#[derive(Accounts)]
#[instruction(txid: [u8; 32], vout: u32)]
pub struct ReleaseFromReserve<'info> {
    /// Any fee payer; funds the claim account's rent. Confers no authority
    /// — a valid threshold attestation is the only authority.
    #[account(mut)]
    pub submitter: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.reserve_token_mint != Pubkey::default()
            @ BridgeError::ReserveNotConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(seeds = [SEED_ATTESTATION_KEY_SET], bump = attestation_key_set.bump)]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    /// Existence of this account is the on-chain replay guard: a second
    /// release for the same `(txid, vout)` fails right here at `init`.
    #[account(
        init,
        payer = submitter,
        space = DepositClaim::SPACE,
        seeds = [SEED_DEPOSIT_CLAIM, txid.as_ref(), &vout.to_le_bytes()],
        bump
    )]
    pub deposit_claim: Account<'info, DepositClaim>,

    #[account(
        mut,
        seeds = [SEED_ROLLING_VOLUME_WINDOW, &[0u8]],
        bump = release_volume_window.bump
    )]
    pub release_volume_window: Account<'info, RollingVolumeWindow>,

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

    /// CHECK: the deposit's bound Solana recipient. Only its address is
    /// used: it anchors the ATA derivation below and is committed to
    /// inside the signed claim message.
    pub recipient: UncheckedAccount<'info>,

    /// Must already exist (no `init_if_needed`): matches the old bridge's
    /// owner decision that the recipient's ATA is a precondition, not
    /// something a stranger's release transaction creates and charges rent
    /// for on the recipient's behalf.
    #[account(
        mut,
        associated_token::mint = reserve_mint,
        associated_token::authority = recipient,
        associated_token::token_program = token_program,
    )]
    pub recipient_token_account: InterfaceAccount<'info, TokenAccount>,

    /// CHECK: the Instructions sysvar, address-pinned.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    /// Pinned to whichever program `initialize_reserve_vault` recorded as
    /// actually owning the reserve mint — substituting the other legitimate
    /// SPL token program is rejected structurally, not by assumption.
    #[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

pub fn release_from_reserve(
    ctx: Context<ReleaseFromReserve>,
    txid: [u8; 32],
    vout: u32,
    amount: u64,
    attestation_epoch: u64,
) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    let key_set = &ctx.accounts.attestation_key_set;

    require!(!config.paused, BridgeError::BridgeGloballyPaused);
    require!(!config.release_paused, BridgeError::ReleaseDirectionPaused);
    require!(
        attestation_epoch == key_set.epoch,
        BridgeError::StaleAttestationEpoch
    );

    enforce_transfer_amount(config, amount)?;

    // Re-reviewed on every call, not just at vault setup (Task 2: do not
    // assume future extension changes are harmless).
    validate_mint_extensions(&ctx.accounts.reserve_mint.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.reserve_token_account.to_account_info())?;
    validate_token_account_extensions(&ctx.accounts.recipient_token_account.to_account_info())?;

    // Reserve sufficiency (constraint 6: fail closed). Checked BEFORE
    // verifying the attestation so a release that can never be fulfilled
    // costs nothing to reject and never partially executes.
    enforce_protected_minimum(
        ctx.accounts.reserve_token_account.amount,
        config.protected_minimum,
        amount,
    )?;

    // The threshold attestation: the instruction directly before this one
    // must be the ed25519 precompile carrying >= threshold unique current
    // attestation-key signatures over exactly the canonical claim bytes.
    let verification_ix =
        get_instruction_relative(-1, &ctx.accounts.instructions_sysvar.to_account_info())
            .map_err(|_| BridgeError::MissingSignatureVerification)?;
    require!(
        verification_ix.program_id == anchor_lang::solana_program::ed25519_program::ID,
        BridgeError::MissingSignatureVerification
    );
    let expected_message = release_claim_message(
        config.protocol_version,
        &crate::ID.to_bytes(),
        key_set.epoch,
        &txid,
        vout,
        amount,
        &ctx.accounts.recipient.key().to_bytes(),
        &config.reserve_token_mint.to_bytes(),
    );
    let signer_count =
        count_unique_attestation_signers(&verification_ix.data, &expected_message, &key_set.keys)?;
    require!(
        signer_count >= usize::from(key_set.threshold),
        BridgeError::InsufficientSignatures
    );

    // Rolling volume: recorded only after every other check passes, so a
    // rejected release never consumes window capacity.
    let now = Clock::get()?.unix_timestamp;
    enforce_and_record_rolling_volume(
        config,
        &mut ctx.accounts.release_volume_window,
        amount,
        now,
    )?;

    let bump = config.reserve_authority_bump;
    let seeds: &[&[u8]] = &[SEED_RESERVE_AUTHORITY, &[bump]];
    let signer_seeds: &[&[&[u8]]] = &[seeds];
    let cpi_accounts = TransferChecked {
        from: ctx.accounts.reserve_token_account.to_account_info(),
        mint: ctx.accounts.reserve_mint.to_account_info(),
        to: ctx.accounts.recipient_token_account.to_account_info(),
        authority: ctx.accounts.reserve_authority.to_account_info(),
    };
    let cpi_ctx = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        cpi_accounts,
        signer_seeds,
    );
    // `transfer_checked` validates its `decimals` argument against the
    // mint account's own `decimals` field and errors on any mismatch —
    // reading it from `reserve_mint` (already constrained to the
    // configured reserve mint) rather than a hardcoded constant means
    // this is correct for whatever the real mint's decimals actually are,
    // not whatever a build-time guess assumed (a real bug found when this
    // program's assumed decimals turned out to disagree with the
    // production GLC mint's actual decimals).
    token_interface::transfer_checked(cpi_ctx, amount, ctx.accounts.reserve_mint.decimals)?;

    let claim = &mut ctx.accounts.deposit_claim;
    claim.txid = txid;
    claim.vout = vout;
    claim.amount = amount;
    claim.recipient = ctx.accounts.recipient.key();
    claim.attestation_epoch = key_set.epoch;
    claim.protocol_version = config.protocol_version;
    claim.slot_created = Clock::get()?.slot;
    claim.bump = ctx.bumps.deposit_claim;
    claim.reserved = [0u8; 16];

    emit!(ReserveReleased {
        txid,
        vout,
        recipient: claim.recipient,
        amount,
        attestation_epoch: claim.attestation_epoch,
    });
    Ok(())
}
