//! One-time reserve-vault creation: the token account the program holds
//! reserve GLC in, owned by a data-less PDA (no keypair exists — the
//! program signs releases via `invoke_signed`, mirroring the old bridge's
//! mint-authority PDA pattern for the analogous "who can move value"
//! question).
//!
//! Uses the EXISTING Solana GLC SPL mint passed in by the admin — this
//! program never creates a mint (docs/12-management-decisions.md item 10:
//! the exact mint address/program (SPL Token vs Token-2022) must be
//! confirmed against the live token before production use; this
//! instruction accepts whatever mint account is supplied and does not
//! assume one).

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{SEED_BRIDGE_CONFIG, SEED_RESERVE_AUTHORITY};
use crate::errors::BridgeError;
use crate::events::ReserveVaultInitialized;
use crate::state::BridgeConfig;

#[derive(Accounts)]
pub struct InitializeReserveVault<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin,
        constraint = bridge_config.reserve_token_mint == Pubkey::default()
            @ BridgeError::ReserveAlreadyConfigured
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    /// The existing Solana GLC SPL mint. Not created or modified here.
    pub reserve_mint: Account<'info, Mint>,

    /// CHECK: data-less PDA, sole authority over the reserve token account.
    /// Address is fully constrained by seeds; no keypair exists for it
    /// (constraint 8: signing keys never stored in the repository — there
    /// is nothing to store).
    #[account(seeds = [SEED_RESERVE_AUTHORITY], bump)]
    pub reserve_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = reserve_mint,
        associated_token::authority = reserve_authority,
    )]
    pub reserve_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn initialize_reserve_vault(ctx: Context<InitializeReserveVault>) -> Result<()> {
    let config = &mut ctx.accounts.bridge_config;
    config.reserve_token_mint = ctx.accounts.reserve_mint.key();
    config.reserve_authority_bump = ctx.bumps.reserve_authority;

    emit!(ReserveVaultInitialized {
        reserve_token_mint: config.reserve_token_mint,
        reserve_token_account: ctx.accounts.reserve_token_account.key(),
        reserve_authority: ctx.accounts.reserve_authority.key(),
    });
    Ok(())
}
