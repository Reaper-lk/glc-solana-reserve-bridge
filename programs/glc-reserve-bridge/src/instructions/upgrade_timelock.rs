//! Timelocked program-upgrade mechanism
//! (docs/12-management-decisions.md item 3, option (c): "timelocked
//! upgrade authority... recommended as an interim state").
//!
//! # What this does, and does not, decide
//!
//! This module builds the MECHANISM only. Whether it is ever armed in a
//! real deployment — i.e. whether [`accept_upgrade_authority`] is ever
//! called with the real deployer key — remains docs/12 item 3's open
//! management decision between (a) full threshold custody of the upgrade
//! authority, (b) revoking upgradeability entirely, and (c) this timelock.
//! Shipping this code changes nothing about any real deployment's actual
//! upgrade authority: until `accept_upgrade_authority` is called by
//! whoever currently holds it, [`crate::constants::SEED_UPGRADE_AUTHORITY`]
//! is just an address with no power over anything, and [`execute_upgrade`]
//! fails closed with [`BridgeError::UpgradeAuthorityNotYetAccepted`]
//! rather than silently doing nothing or falsely reporting success.
//!
//! Admin-gated to propose/cancel, not threshold-gated like attestation-key
//! rotation (`instructions::governance`): upgrade authority and
//! attestation-signer authority are different domains
//! (docs/02-trust-model.md) with no reason to share one authorization
//! model. This mirrors `instructions::admin::set_paused`'s existing
//! admin-gated-immediate posture, with a mandatory timelock added on top —
//! exactly docs/12 item 3's own recommended interim shape.
//!
//! # Flow
//!
//! ```text
//!   accept_upgrade_authority   (one-time; current REAL authority hands off to the PDA)
//!            |
//!   propose_upgrade            (admin-gated; names a buffer, starts the timelock)
//!            |  creates the singleton PendingProgramUpgrade { eta }
//!            v
//!     ... timelock window: publicly observable, cancellable ...
//!            |
//!   execute_upgrade            (permissionless once eta has passed; CPIs the real upgrade)
//!            |
//!   cancel_upgrade             (admin-gated, any time before execution)
//! ```
//!
//! # Why no on-chain code actually calls `accept_upgrade_authority`
//!
//! It requires a live signature from whoever holds the program's real,
//! external upgrade-authority keypair at deploy time — this repository
//! never generates or holds that key (constraint 8), and no production
//! deployment exists yet. Calling it is a real, one-time, deliberate
//! management action, not something this codebase can or should do on its
//! own behalf.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::bpf_loader_upgradeable;
use anchor_lang::solana_program::program::invoke_signed;

use crate::constants::{SEED_BRIDGE_CONFIG, SEED_PENDING_UPGRADE, SEED_UPGRADE_AUTHORITY};
use crate::errors::BridgeError;
use crate::events::{
    ProgramUpgradeCancelled, ProgramUpgradeExecuted, ProgramUpgradeProposed,
    UpgradeAuthorityAccepted,
};
use crate::state::{BridgeConfig, PendingProgramUpgrade};

// ------------------------------------------------- accept_upgrade_authority --

#[derive(Accounts)]
pub struct AcceptUpgradeAuthority<'info> {
    /// The program's CURRENT real upgrade authority — whoever deployed it,
    /// or last called `solana program set-upgrade-authority`. Checked
    /// against `program_data.upgrade_authority_address` below; the loader
    /// CPI itself re-checks this independently regardless, so this
    /// instruction cannot succeed with the wrong signer even if the
    /// constraint below were somehow bypassed.
    pub current_upgrade_authority: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    /// CHECK: data-less signing PDA (mirrors
    /// `crate::constants::SEED_RESERVE_AUTHORITY`'s existing pattern) —
    /// never holds data, never has a keypair anywhere. The loader only
    /// cares that `invoke_signed` proves this program controls the
    /// address, which the seeds below establish.
    #[account(seeds = [SEED_UPGRADE_AUTHORITY], bump)]
    pub upgrade_authority_pda: UncheckedAccount<'info>,

    /// This program's own executable account, tying `program_data` to the
    /// genuine loader-v3 ProgramData address (same check `initialize`
    /// already performs).
    #[account(
        constraint = program.programdata_address()? == Some(program_data.key())
            @ BridgeError::UnauthorizedInitializer
    )]
    pub program: Program<'info, crate::program::GlcReserveBridge>,

    #[account(
        mut,
        constraint = program_data.upgrade_authority_address == Some(current_upgrade_authority.key())
            @ BridgeError::NotCurrentUpgradeAuthority
    )]
    pub program_data: Account<'info, ProgramData>,

    /// CHECK: address-pinned to the real BPF Upgradeable Loader — the CPI
    /// target, not a data account this program reads.
    #[account(address = bpf_loader_upgradeable::ID)]
    pub bpf_loader_upgradeable_program: UncheckedAccount<'info>,
}

/// One-time handoff of REAL upgrade authority from whoever currently holds
/// it to this program's own signing PDA. Until this succeeds, every other
/// instruction in this module is inert — see module docs.
pub fn accept_upgrade_authority(ctx: Context<AcceptUpgradeAuthority>) -> Result<()> {
    let ix = bpf_loader_upgradeable::set_upgrade_authority_checked(
        &crate::ID,
        &ctx.accounts.current_upgrade_authority.key(),
        &ctx.accounts.upgrade_authority_pda.key(),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.program_data.to_account_info(),
            ctx.accounts.current_upgrade_authority.to_account_info(),
            ctx.accounts.upgrade_authority_pda.to_account_info(),
        ],
        &[&[SEED_UPGRADE_AUTHORITY, &[ctx.bumps.upgrade_authority_pda]]],
    )?;
    emit!(UpgradeAuthorityAccepted {
        previous_authority: ctx.accounts.current_upgrade_authority.key(),
        upgrade_authority_pda: ctx.accounts.upgrade_authority_pda.key(),
    });
    Ok(())
}

// -------------------------------------------------------------- propose --

#[derive(Accounts)]
pub struct ProposeUpgrade<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    /// Singleton: `init` fails if an upgrade is already pending — the
    /// replay/idempotency guard for `propose_upgrade` is structural, not a
    /// runtime check that could be forgotten.
    #[account(
        init,
        payer = admin,
        space = PendingProgramUpgrade::SPACE,
        seeds = [SEED_PENDING_UPGRADE],
        bump
    )]
    pub pending_upgrade: Account<'info, PendingProgramUpgrade>,

    pub system_program: Program<'info, System>,
}

/// Queues an upgrade to `buffer_address` behind the configurable timelock.
/// Does not itself validate the buffer's contents — that happens for real
/// when the loader processes [`execute_upgrade`]'s CPI; this only starts
/// the clock and records what was proposed, publicly and immutably, for
/// the timelock window's duration.
pub fn propose_upgrade(ctx: Context<ProposeUpgrade>, buffer_address: Pubkey) -> Result<()> {
    let config = &ctx.accounts.bridge_config;
    require!(
        config.upgrade_timelock_seconds > 0,
        BridgeError::ZeroUpgradeTimelock
    );
    let now = Clock::get()?.unix_timestamp;
    let eta = now
        .checked_add(config.upgrade_timelock_seconds)
        .ok_or(BridgeError::ArithmeticOverflow)?;

    let pending = &mut ctx.accounts.pending_upgrade;
    pending.buffer_address = buffer_address;
    pending.proposed_at = now;
    pending.eta = eta;
    pending.proposed_by = ctx.accounts.admin.key();
    pending.bump = ctx.bumps.pending_upgrade;
    pending.reserved = [0u8; 16];

    emit!(ProgramUpgradeProposed {
        buffer_address,
        eta,
        proposed_by: ctx.accounts.admin.key(),
    });
    Ok(())
}

// -------------------------------------------------------------- cancel --

#[derive(Accounts)]
pub struct CancelUpgrade<'info> {
    /// Rent refund recipient. Confers no authority beyond the `constraint`
    /// below — cancellation is admin-gated, same as proposal.
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin
    )]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(
        mut,
        close = admin,
        seeds = [SEED_PENDING_UPGRADE],
        bump = pending_upgrade.bump
    )]
    pub pending_upgrade: Account<'info, PendingProgramUpgrade>,
}

/// Cancels the pending upgrade at any time before execution — always safe,
/// since nothing on-chain has taken effect yet (no code has changed, no
/// authority has moved).
pub fn cancel_upgrade(ctx: Context<CancelUpgrade>) -> Result<()> {
    emit!(ProgramUpgradeCancelled {
        buffer_address: ctx.accounts.pending_upgrade.buffer_address,
        eta: ctx.accounts.pending_upgrade.eta,
    });
    Ok(())
}

// ------------------------------------------------------------- execute --

#[derive(Accounts)]
pub struct ExecuteUpgrade<'info> {
    /// Permissionless: rent from the closed `pending_upgrade` account is
    /// refunded here, and the loader CPI's "spill" account (excess
    /// lamports freed by the old programdata size) also lands here.
    /// Confers no authority — the admin's proposal already was the
    /// authorization; the delay is what gave observers their window.
    #[account(mut)]
    pub executor: Signer<'info>,

    #[account(seeds = [SEED_BRIDGE_CONFIG], bump = bridge_config.bump)]
    pub bridge_config: Account<'info, BridgeConfig>,

    #[account(
        mut,
        close = executor,
        seeds = [SEED_PENDING_UPGRADE],
        bump = pending_upgrade.bump
    )]
    pub pending_upgrade: Account<'info, PendingProgramUpgrade>,

    /// CHECK: data-less signing PDA; see [`AcceptUpgradeAuthority`].
    #[account(seeds = [SEED_UPGRADE_AUTHORITY], bump)]
    pub upgrade_authority_pda: UncheckedAccount<'info>,

    /// CHECK: this program's own executable account, address-pinned. Not
    /// the typed `Program<'info, T>` wrapper used elsewhere in this crate
    /// (`initialize.rs`, `AcceptUpgradeAuthority` above): that wrapper
    /// requires `T: AccountDeserialize`, which is incompatible with
    /// `#[account(mut)]` — and `mut` is required here because the real
    /// `bpf_loader_upgradeable::upgrade` CPI writes to this account (its
    /// `Program` entry is updated in place during an upgrade). A CPI
    /// requesting write access to an account this instruction only
    /// declared read-only fails as a privilege-escalation attempt — a
    /// runtime rule, not specific to this program.
    #[account(mut, address = crate::ID)]
    pub program: UncheckedAccount<'info>,

    /// The loader-v3 ProgramData PDA for this exact program — address
    /// derived directly (`seeds::program` targets the loader, not this
    /// program), rather than cross-checked against a separate typed
    /// `program` account as `initialize.rs`/`AcceptUpgradeAuthority` do,
    /// since `program` above is now an `UncheckedAccount` with no
    /// `programdata_address()` accessor to cross-check against.
    #[account(
        mut,
        seeds = [crate::ID.as_ref()],
        seeds::program = bpf_loader_upgradeable::ID,
        bump
    )]
    pub program_data: Account<'info, ProgramData>,

    /// CHECK: must equal `pending_upgrade.buffer_address`, re-checked
    /// below rather than trusted from the account list alone; the loader's
    /// own `Upgrade` instruction independently validates everything else
    /// about it (authority, size headroom).
    #[account(mut)]
    pub buffer: UncheckedAccount<'info>,

    pub rent: Sysvar<'info, Rent>,
    pub clock: Sysvar<'info, Clock>,

    /// CHECK: address-pinned to the real BPF Upgradeable Loader.
    #[account(address = bpf_loader_upgradeable::ID)]
    pub bpf_loader_upgradeable_program: UncheckedAccount<'info>,
}

/// Applies a queued upgrade once its timelock has elapsed. Fails closed
/// with [`BridgeError::UpgradeAuthorityNotYetAccepted`] if
/// [`accept_upgrade_authority`] was never called for this deployment —
/// this instruction can never silently no-op as "success" while believing
/// nothing actually happened.
pub fn execute_upgrade(ctx: Context<ExecuteUpgrade>) -> Result<()> {
    let pending = &ctx.accounts.pending_upgrade;
    require!(
        Clock::get()?.unix_timestamp >= pending.eta,
        BridgeError::UpgradeTimelockNotElapsed
    );
    require!(
        ctx.accounts.buffer.key() == pending.buffer_address,
        BridgeError::WrongUpgradeBuffer
    );
    require!(
        ctx.accounts.program_data.upgrade_authority_address
            == Some(ctx.accounts.upgrade_authority_pda.key()),
        BridgeError::UpgradeAuthorityNotYetAccepted
    );

    let buffer_address = pending.buffer_address;
    let ix = bpf_loader_upgradeable::upgrade(
        &crate::ID,
        &buffer_address,
        &ctx.accounts.upgrade_authority_pda.key(),
        &ctx.accounts.executor.key(),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.program_data.to_account_info(),
            ctx.accounts.program.to_account_info(),
            ctx.accounts.buffer.to_account_info(),
            ctx.accounts.executor.to_account_info(),
            ctx.accounts.rent.to_account_info(),
            ctx.accounts.clock.to_account_info(),
            ctx.accounts.upgrade_authority_pda.to_account_info(),
        ],
        &[&[SEED_UPGRADE_AUTHORITY, &[ctx.bumps.upgrade_authority_pda]]],
    )?;

    emit!(ProgramUpgradeExecuted { buffer_address });
    Ok(())
}
