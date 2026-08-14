//! Solana-deposit -> reserve path: the user transfers existing Solana GLC
//! into the reserve and a payout obligation is recorded, atomically.
//! Replaces the old bridge's `burn_wrapped` (docs/01-reuse-inventory.md):
//! same atomicity guarantee (there is no state in which value moved without
//! a persistent, queryable obligation record), but a plain SPL transfer
//! instead of a burn — the reserve model never burns.
//!
//! No attestation is required for this instruction: the user is moving
//! their own tokens into a program-owned account they do not control, which
//! needs no authorization beyond the user's own signature. The asymmetric,
//! weaker-guarantee direction is the eventual Goldcoin-side payout this
//! obligation authorizes (see `complete_goldcoin_payout.rs` and
//! docs/02-trust-model.md's asymmetry note) — not this deposit step, which
//! is fully on-chain and fully verifiable by construction.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, TransferChecked};

use crate::constants::{
    MAX_GLC_ADDRESS_LEN, SEED_BRIDGE_CONFIG, SEED_RESERVE_AUTHORITY, SEED_ROLLING_VOLUME_WINDOW,
    SEED_WITHDRAWAL_OBLIGATION,
};
use crate::errors::BridgeError;
use crate::events::ReserveDeposited;
use crate::limits::{enforce_and_record_rolling_volume, enforce_transfer_amount};
use crate::state::{BridgeConfig, RollingVolumeWindow, WithdrawalObligation, WithdrawalStatus};

#[derive(Accounts)]
pub struct DepositToReserve<'info> {
    /// The wallet depositing; signs its own transfer and pays the
    /// obligation record's rent.
    #[account(mut)]
    pub user: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.reserve_token_mint != Pubkey::default()
            @ BridgeError::ReserveNotConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(
        mut,
        seeds = [SEED_ROLLING_VOLUME_WINDOW, &[1u8]],
        bump = deposit_volume_window.bump
    )]
    pub deposit_volume_window: Account<'info, RollingVolumeWindow>,

    #[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]
    pub reserve_mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = reserve_mint,
        associated_token::authority = user,
    )]
    pub user_token_account: Account<'info, TokenAccount>,

    /// CHECK: data-less PDA; unused as a signer here (the user signs their
    /// own transfer) but pinned so the ATA derivation below is
    /// unambiguously the same reserve vault `release_from_reserve` uses.
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    #[account(
        mut,
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
    )]
    pub reserve_token_account: Account<'info, TokenAccount>,

    /// The persistent payout obligation, seeded by the CURRENT counter
    /// value; the counter increments in the handler.
    #[account(
        init,
        payer = user,
        space = WithdrawalObligation::SPACE,
        seeds = [SEED_WITHDRAWAL_OBLIGATION, &bridge_config.obligation_count.to_le_bytes()],
        bump
    )]
    pub withdrawal_obligation: Account<'info, WithdrawalObligation>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn deposit_to_reserve(
    ctx: Context<DepositToReserve>,
    amount: u64,
    glc_address: Vec<u8>,
) -> Result<()> {
    let config = &ctx.accounts.bridge_config;

    require!(!config.paused, BridgeError::BridgeGloballyPaused);
    require!(!config.deposit_paused, BridgeError::DepositDirectionPaused);
    enforce_transfer_amount(config, amount)?;
    require!(
        !glc_address.is_empty() && glc_address.len() <= MAX_GLC_ADDRESS_LEN,
        BridgeError::InvalidGlcAddressLength
    );

    // Fail on counter exhaustion before any value moves.
    let index = config.obligation_count;
    let next_count = index
        .checked_add(1)
        .ok_or(BridgeError::ArithmeticOverflow)?;

    let now = Clock::get()?.unix_timestamp;
    enforce_and_record_rolling_volume(
        config,
        &mut ctx.accounts.deposit_volume_window,
        amount,
        now,
    )?;

    // The user signs their own transfer — no PDA authority involved, same
    // as the old bridge's user-signed burn.
    let cpi_accounts = TransferChecked {
        from: ctx.accounts.user_token_account.to_account_info(),
        mint: ctx.accounts.reserve_mint.to_account_info(),
        to: ctx.accounts.reserve_token_account.to_account_info(),
        authority: ctx.accounts.user.to_account_info(),
    };
    let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
    // See the matching comment in `release_from_reserve` — read decimals
    // from the mint itself rather than a hardcoded constant.
    token::transfer_checked(cpi_ctx, amount, ctx.accounts.reserve_mint.decimals)?;

    let record = &mut ctx.accounts.withdrawal_obligation;
    record.index = index;
    record.amount = amount;
    record.requester = ctx.accounts.user.key();
    record.glc_address = [0u8; 64];
    record.glc_address[..glc_address.len()].copy_from_slice(&glc_address);
    record.glc_address_len = glc_address.len() as u8;
    record.status = WithdrawalStatus::Pending;
    record.requested_at_slot = Clock::get()?.slot;
    record.protocol_version = config.protocol_version;
    record.bump = ctx.bumps.withdrawal_obligation;
    record.reserved = [0u8; 48];

    ctx.accounts.bridge_config.obligation_count = next_count;

    emit!(ReserveDeposited {
        index,
        requester: record.requester,
        amount,
        glc_address,
    });
    Ok(())
}
