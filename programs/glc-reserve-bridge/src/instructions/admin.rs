//! Admin-gated instructions: pause, limit changes, the rolling-volume-window
//! reset override, and the two-step admin handover.
//!
//! **Attestation-key rotation is NOT here.** It lives in
//! [`crate::instructions::governance`], behind a threshold-gated timelock,
//! because a single admin key able to rotate the attestation keys would be
//! able to install attacker-controlled keys and defeat the approved trust
//! model's core property: no single key can release reserves
//! (docs/02-trust-model.md, docs/12-management-decisions.md item 1).
//!
//! Pause and limit changes ARE currently admin-gated-immediate — a known
//! interim posture inherited deliberately from the old bridge's own Phase 1
//! precedent (see IMPLEMENTATION_LOG.md), not the target end state
//! described in docs/03-architecture.md's asymmetric-governance design.
//! Admin instructions remain callable while paused, otherwise un-pausing
//! would be impossible. [`reset_rolling_volume_window`] is the one
//! exception that goes the other way: it REQUIRES the bridge to already be
//! paused (docs/09-runbook.md's rolling-volume-window maintenance
//! sequence) — see its own doc comment.

use anchor_lang::prelude::*;

use crate::constants::{SEED_BRIDGE_CONFIG, SEED_ROLLING_VOLUME_WINDOW};
use crate::errors::BridgeError;
use crate::events::{
    AdminTransferInitiated, AdminTransferred, LimitsChanged, PauseStateChanged,
    RollingVolumeWindowReset,
};
use crate::state::{BridgeConfig, Direction, RollingVolumeWindow};

/// Which circuit breaker(s) a `set_paused` call targets.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum PauseScope {
    /// Global emergency pause: blocks both settlement directions.
    Global,
    /// Directional: Goldcoin -> Solana releases only.
    Release,
    /// Directional: Solana -> Goldcoin deposits only.
    Deposit,
}

#[derive(Accounts)]
pub struct AdminConfig<'info> {
    pub admin: Signer<'info>,
    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin
    )]
    pub bridge_config: Account<'info, BridgeConfig>,
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    /// The pending admin accepting the handover.
    pub new_admin: Signer<'info>,
    #[account(
        mut,
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump
    )]
    pub bridge_config: Account<'info, BridgeConfig>,
}

/// Flips the requested circuit breaker. A no-op flip is rejected so a stale
/// client can't silently "succeed" while observing the opposite state.
pub fn set_paused(ctx: Context<AdminConfig>, scope: PauseScope, paused: bool) -> Result<()> {
    let config = &mut ctx.accounts.bridge_config;
    match scope {
        PauseScope::Global => {
            require!(config.paused != paused, BridgeError::PauseStateUnchanged);
            config.paused = paused;
        }
        PauseScope::Release => {
            require!(
                config.release_paused != paused,
                BridgeError::PauseStateUnchanged
            );
            config.release_paused = paused;
        }
        PauseScope::Deposit => {
            require!(
                config.deposit_paused != paused,
                BridgeError::PauseStateUnchanged
            );
            config.deposit_paused = paused;
        }
    }
    emit!(PauseStateChanged {
        paused: config.paused,
        release_paused: config.release_paused,
        deposit_paused: config.deposit_paused,
    });
    Ok(())
}

/// Which `BridgeConfig` limit a `set_limit` call targets.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum LimitField {
    MinTransferAmount,
    PerTransferLimit,
    ProtectedMinimum,
    RollingVolumeLimit,
}

/// Admin-gated, immediate limit change. See module docs: this is an interim
/// posture, not the asymmetric timelocked-governance design the
/// architecture calls for — tracked in IMPLEMENTATION_LOG.md as follow-up
/// work, not silently accepted as final.
pub fn set_limit(ctx: Context<AdminConfig>, field: LimitField, new_value: u64) -> Result<()> {
    let config = &mut ctx.accounts.bridge_config;
    let (previous, field_name) = match field {
        LimitField::MinTransferAmount => {
            let previous = config.min_transfer_amount;
            config.min_transfer_amount = new_value;
            (previous, "min_transfer_amount")
        }
        LimitField::PerTransferLimit => {
            require!(new_value > 0, BridgeError::ZeroAmount);
            let previous = config.per_transfer_limit;
            config.per_transfer_limit = new_value;
            (previous, "per_transfer_limit")
        }
        LimitField::ProtectedMinimum => {
            let previous = config.protected_minimum;
            config.protected_minimum = new_value;
            (previous, "protected_minimum")
        }
        LimitField::RollingVolumeLimit => {
            require!(new_value > 0, BridgeError::ZeroAmount);
            let previous = config.rolling_volume_limit;
            config.rolling_volume_limit = new_value;
            (previous, "rolling_volume_limit")
        }
    };
    emit!(LimitsChanged {
        field: field_name.to_string(),
        previous,
        current: new_value,
    });
    Ok(())
}

/// Step 1 of the admin handover: propose a new admin.
pub fn transfer_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
    let config = &mut ctx.accounts.bridge_config;
    require!(new_admin != config.admin, BridgeError::AdminUnchanged);
    config.pending_admin = Some(new_admin);
    emit!(AdminTransferInitiated {
        admin: config.admin,
        pending_admin: new_admin,
    });
    Ok(())
}

/// Step 2 of the admin handover: only the proposed key may accept.
pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
    let config = &mut ctx.accounts.bridge_config;
    let pending = config.pending_admin.ok_or(BridgeError::NoPendingAdmin)?;
    require!(
        pending == ctx.accounts.new_admin.key(),
        BridgeError::PendingAdminMismatch
    );
    let previous_admin = config.admin;
    config.admin = pending;
    config.pending_admin = None;
    emit!(AdminTransferred {
        previous_admin,
        new_admin: config.admin,
    });
    Ok(())
}

/// `direction` selects which of the two fixed-seed `RollingVolumeWindow`
/// PDAs this call targets — `seeds` derives the exact expected address
/// from it directly, so passing the account for the OTHER direction is
/// structurally rejected by Anchor's own seeds check
/// (`ConstraintSeeds`) before the handler ever runs; there is no
/// additional runtime branch that could pick the wrong one. `bridge_config`
/// is deliberately NOT `mut` here — this instruction never writes to it.
#[derive(Accounts)]
#[instruction(direction: Direction)]
pub struct ResetRollingVolumeWindow<'info> {
    pub admin: Signer<'info>,
    #[account(
        seeds = [SEED_BRIDGE_CONFIG],
        bump = bridge_config.bump,
        constraint = bridge_config.admin == admin.key() @ BridgeError::UnauthorizedAdmin
    )]
    pub bridge_config: Account<'info, BridgeConfig>,
    #[account(
        mut,
        seeds = [SEED_ROLLING_VOLUME_WINDOW, &[direction as u8]],
        bump = rolling_volume_window.bump,
    )]
    pub rolling_volume_window: Account<'info, RollingVolumeWindow>,
}

/// Administrative override of the rolling-volume anti-drain protection
/// (docs/09-runbook.md "SolToGlc"/"GlcToSol rolling-volume window"): lets
/// the admin manually reopen a direction after maintenance/refill without
/// waiting out the remainder of its current window, and without touching
/// SQLite or fabricating a timestamp by hand.
///
/// Requires the bridge to ALREADY be globally paused — this is an
/// intentional precondition the operator must consciously satisfy first
/// (same discipline as `rebalance_withdraw`'s identical check), not
/// merely a side effect of no concurrent settlement being possible.
/// Deliberately does NOT require the individual direction's own pause
/// flag: an operator resetting the window during a global-pause
/// maintenance window should not also have to flip the directional flag
/// first and back again.
///
/// Touches ONLY the selected `RollingVolumeWindow` account: reserve
/// balances, obligations, `protected_minimum`, fees, `per_transfer_limit`,
/// and the OTHER direction's window are all untouched by construction
/// (they are not even present in this instruction's account list).
///
/// Reset semantics mirror `limits::enforce_and_record_rolling_volume`'s
/// own bucket-expiry branch exactly: `window_start` becomes the current
/// on-chain clock's `unix_timestamp` (a fresh window starts now, from the
/// real trusted clock — never a caller-supplied value), and `window_total`
/// becomes zero. The off-chain `remaining`/`quota_exhausted` figures
/// `GET /status` reports are DERIVED from these two fields alone
/// (`service/src/solana/accounts.rs`'s `rolling_volume_remaining`), so
/// this reset is immediately and correctly visible there with no
/// service-side change — `remaining` becomes the full configured
/// `rolling_volume_limit` and `quota_exhausted` becomes `false`.
pub fn reset_rolling_volume_window(
    ctx: Context<ResetRollingVolumeWindow>,
    direction: Direction,
) -> Result<()> {
    require!(
        ctx.accounts.bridge_config.paused,
        BridgeError::BridgeNotPaused
    );

    let window = &mut ctx.accounts.rolling_volume_window;
    let previous_window_start = window.window_start;
    let previous_window_total = window.window_total;

    let clock = Clock::get()?;
    window.window_start = clock.unix_timestamp;
    window.window_total = 0;

    emit!(RollingVolumeWindowReset {
        admin: ctx.accounts.admin.key(),
        direction,
        previous_window_start,
        previous_window_total,
        new_window_start: window.window_start,
        new_window_total: window.window_total,
        timestamp: clock.unix_timestamp,
        slot: clock.slot,
    });
    Ok(())
}
