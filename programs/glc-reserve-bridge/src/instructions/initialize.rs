//! One-time bridge initialization.
//!
//! Authorization: only the program's upgrade authority may initialize,
//! proven by matching the signer against the loader-v3 `ProgramData`
//! account (reused from the old bridge's ADR-0008 pattern). This closes the
//! initialization-front-running window on a fresh deployment. The
//! initializer becomes the initial admin; governance can then be handed
//! over via the two-step admin transfer.
//!
//! Reinitialization is structurally impossible: every account below is
//! created with Anchor `init` on fixed seeds.
//!
//! The reserve token account is NOT created here — that is
//! `initialize_reserve_vault`, gated on knowing the existing GLC SPL mint's
//! address (docs/12-management-decisions.md item 10).

use anchor_lang::prelude::*;

use crate::constants::{
    PROTOCOL_VERSION, SEED_ATTESTATION_KEY_SET, SEED_BRIDGE_CONFIG, SEED_ROLLING_VOLUME_WINDOW,
};
use crate::errors::BridgeError;
use crate::events::BridgeInitialized;
use crate::state::{AttestationKeySet, BridgeConfig, Direction, RollingVolumeWindow};
use crate::validation::validate_attestation_key_set;

#[derive(Accounts)]
pub struct Initialize<'info> {
    /// The program upgrade authority; pays rent and becomes the initial
    /// admin.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        init,
        payer = authority,
        space = BridgeConfig::SPACE,
        seeds = [SEED_BRIDGE_CONFIG],
        bump
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(
        init,
        payer = authority,
        space = AttestationKeySet::SPACE,
        seeds = [SEED_ATTESTATION_KEY_SET],
        bump
    )]
    pub attestation_key_set: Account<'info, AttestationKeySet>,

    #[account(
        init,
        payer = authority,
        space = RollingVolumeWindow::SPACE,
        seeds = [SEED_ROLLING_VOLUME_WINDOW, &[0u8]],
        bump
    )]
    pub release_volume_window: Account<'info, RollingVolumeWindow>,

    #[account(
        init,
        payer = authority,
        space = RollingVolumeWindow::SPACE,
        seeds = [SEED_ROLLING_VOLUME_WINDOW, &[1u8]],
        bump
    )]
    pub deposit_volume_window: Account<'info, RollingVolumeWindow>,

    /// This program's own executable account; ties `program_data` to the
    /// genuine loader-v3 ProgramData address.
    #[account(
        constraint = program.programdata_address()? == Some(program_data.key())
            @ BridgeError::UnauthorizedInitializer
    )]
    pub program: Program<'info, crate::program::GlcReserveBridge>,

    #[account(
        constraint = program_data.upgrade_authority_address == Some(authority.key())
            @ BridgeError::UnauthorizedInitializer
    )]
    pub program_data: Account<'info, ProgramData>,

    pub system_program: Program<'info, System>,
}

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
) -> Result<()> {
    validate_attestation_key_set(&attestation_keys, threshold)?;

    // No built-in defaults for any of these (same discipline the old bridge
    // used for its governance timelock and supply cap): a safe value is a
    // live security/ops decision, and the program refuses to start without
    // one rather than choosing on the operator's behalf.
    require!(
        governance_timelock_seconds > 0,
        BridgeError::ZeroGovernanceTimelock
    );
    require!(per_transfer_limit > 0, BridgeError::ZeroAmount);
    require!(rolling_volume_limit > 0, BridgeError::ZeroAmount);
    require!(rolling_window_seconds > 0, BridgeError::ZeroAmount);

    let config = &mut ctx.accounts.bridge_config;
    config.protocol_version = PROTOCOL_VERSION;
    config.admin = ctx.accounts.authority.key();
    config.pending_admin = None;
    config.paused = false;
    config.release_paused = false;
    config.deposit_paused = false;
    config.bump = ctx.bumps.bridge_config;
    // Reserve vault not created here: default = unset sentinel until
    // `initialize_reserve_vault`.
    config.reserve_token_mint = Pubkey::default();
    config.reserve_token_program = Pubkey::default();
    config.reserve_authority_bump = 0;
    config.obligation_count = 0;
    config.governance_timelock_seconds = governance_timelock_seconds;
    config.min_transfer_amount = min_transfer_amount;
    config.per_transfer_limit = per_transfer_limit;
    config.protected_minimum = protected_minimum;
    config.rolling_volume_limit = rolling_volume_limit;
    config.rolling_window_seconds = rolling_window_seconds;
    let key_count = attestation_keys.len() as u8;
    let set = &mut ctx.accounts.attestation_key_set;
    set.epoch = 0;
    set.threshold = threshold;
    set.bump = ctx.bumps.attestation_key_set;
    set.keys = attestation_keys;
    set.reserved = [0u8; 32];

    let now = Clock::get()?.unix_timestamp;
    let release_window = &mut ctx.accounts.release_volume_window;
    release_window.direction = Direction::GoldcoinToSolana;
    release_window.window_start = now;
    release_window.window_total = 0;
    release_window.bump = ctx.bumps.release_volume_window;
    release_window.reserved = [0u8; 16];

    let deposit_window = &mut ctx.accounts.deposit_volume_window;
    deposit_window.direction = Direction::SolanaToGoldcoin;
    deposit_window.window_start = now;
    deposit_window.window_total = 0;
    deposit_window.bump = ctx.bumps.deposit_volume_window;
    deposit_window.reserved = [0u8; 16];

    emit!(BridgeInitialized {
        admin: config.admin,
        protocol_version: PROTOCOL_VERSION,
        threshold,
        attestation_key_count: key_count,
    });
    Ok(())
}
