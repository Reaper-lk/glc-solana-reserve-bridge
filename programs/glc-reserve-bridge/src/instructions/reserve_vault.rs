//! One-time reserve-vault creation: the token account the program holds
//! reserve GLC in, owned by a data-less PDA (no keypair exists — the
//! program signs releases via `invoke_signed`, mirroring the old bridge's
//! mint-authority PDA pattern for the analogous "who can move value"
//! question).
//!
//! Uses the EXISTING Solana GLC SPL mint passed in by the admin — this
//! program never creates a mint. The canonical Solana GLC mint is an
//! existing Token-2022 mint (docs/18-token-2022-support.md), but this
//! instruction accepts whatever mint the admin supplies and whatever SPL
//! token program actually owns it (legacy SPL Token or Token-2022, via
//! `anchor_spl::token_interface`); the program owning `reserve_mint` at
//! this one-time call is captured into `BridgeConfig.reserve_token_program`
//! and pinned on every later reserve-touching instruction via an `address`
//! constraint, so no other program can ever be substituted for it
//! afterwards. `reserve_mint`'s extensions are reviewed against the
//! explicit allowlist in `crate::token_extensions` before the vault is
//! accepted — this is the natural onboarding review point.

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::constants::{SEED_BRIDGE_CONFIG, SEED_RESERVE_AUTHORITY};
use crate::errors::BridgeError;
use crate::events::ReserveVaultInitialized;
use crate::state::BridgeConfig;
use crate::token_extensions::validate_mint_extensions;

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

    /// The existing Solana GLC mint. Not created or modified here. Accepted
    /// under whichever SPL token program (`token_program` below) actually
    /// owns it — legacy SPL Token or Token-2022.
    pub reserve_mint: InterfaceAccount<'info, Mint>,

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
        associated_token::token_program = token_program,
    )]
    pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,

    /// Whichever SPL token program actually owns `reserve_mint` — legacy
    /// SPL Token or Token-2022. Not constrained to a fixed program ID here
    /// (that pin does not exist until this call establishes it); every
    /// later reserve-touching instruction constrains this account to
    /// `bridge_config.reserve_token_program`, set below.
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

pub fn initialize_reserve_vault(ctx: Context<InitializeReserveVault>) -> Result<()> {
    validate_mint_extensions(&ctx.accounts.reserve_mint.to_account_info())?;

    let config = &mut ctx.accounts.bridge_config;
    config.reserve_token_mint = ctx.accounts.reserve_mint.key();
    config.reserve_token_program = ctx.accounts.token_program.key();
    config.reserve_authority_bump = ctx.bumps.reserve_authority;

    emit!(ReserveVaultInitialized {
        reserve_token_mint: config.reserve_token_mint,
        reserve_token_account: ctx.accounts.reserve_token_account.key(),
        reserve_authority: ctx.accounts.reserve_authority.key(),
    });
    Ok(())
}
