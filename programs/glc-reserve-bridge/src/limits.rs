//! Pure limit-enforcement helpers shared by `release_from_reserve` and
//! `deposit_to_reserve`, so the two settlement directions can never drift on
//! how per-transfer and rolling-volume limits are checked. Kept free of
//! `Context`/account-fetch machinery so the rules are unit-testable without
//! a runtime.
//!
//! `RollingVolumeWindow` implements a **fixed-bucket** window, not a true
//! sliding one — documented in `state.rs` and the implementation log as a
//! deliberate simplification for this phase. It never under-counts volume
//! within a bucket; a burst spanning a bucket boundary can exceed the
//! configured limit within a short combined window, which is why the
//! configured limit should be set with that in mind until a sliding-window
//! implementation replaces it.

use anchor_lang::prelude::*;

use crate::errors::BridgeError;
use crate::state::{BridgeConfig, RebalancePolicy, RollingVolumeWindow};

/// Checks a transfer amount against the dust floor and the per-transfer
/// ceiling. Does not touch the rolling window — call
/// [`enforce_and_record_rolling_volume`] separately once the amount is
/// otherwise accepted.
pub fn enforce_transfer_amount(config: &BridgeConfig, amount: u64) -> Result<()> {
    require!(amount > 0, BridgeError::ZeroAmount);
    require!(
        amount >= config.min_transfer_amount,
        BridgeError::BelowMinimumTransfer
    );
    require!(
        amount <= config.per_transfer_limit,
        BridgeError::ExceedsPerTransferLimit
    );
    Ok(())
}

/// The one fixed-bucket implementation, shared by the two settlement
/// directions' [`RollingVolumeWindow`]s and by the reserve withdrawal
/// budget in [`RebalancePolicy`]. Extracted so the three can never drift:
/// a bucket that expires differently, or an overflow that is checked in
/// one place and not another, is exactly the kind of divergence that only
/// shows up under the load it was supposed to bound.
///
/// Resets the bucket if it has expired, then checks and records `amount`.
/// Mutates the caller's window fields ONLY on success — a rejected
/// transfer must never advance the window's state, or a caller could
/// exhaust someone else's budget with requests that were themselves
/// refused.
///
/// `over_limit` is a parameter rather than a fixed error because the
/// budgets are genuinely different policies with genuinely different
/// remediations: exceeding a settlement direction's volume limit and
/// exceeding the operator withdrawal budget should never surface as the
/// same error code to an operator reading a failed transaction.
fn enforce_and_record_bucket(
    window_start: &mut i64,
    window_total: &mut u64,
    window_seconds: i64,
    limit: u64,
    amount: u64,
    now: i64,
    over_limit: BridgeError,
) -> Result<()> {
    let bucket_age = now.saturating_sub(*window_start);
    let (start, total) = if bucket_age >= window_seconds {
        (now, 0u64)
    } else {
        (*window_start, *window_total)
    };
    let projected = total
        .checked_add(amount)
        .ok_or(BridgeError::ArithmeticOverflow)?;
    if projected > limit {
        return Err(over_limit.into());
    }
    *window_start = start;
    *window_total = projected;
    Ok(())
}

/// Resets the bucket if it has expired, then checks and records `amount`
/// against the rolling volume limit. Mutates `window` only on success — a
/// rejected transfer must never advance the window's state.
pub fn enforce_and_record_rolling_volume(
    config: &BridgeConfig,
    window: &mut RollingVolumeWindow,
    amount: u64,
    now: i64,
) -> Result<()> {
    enforce_and_record_bucket(
        &mut window.window_start,
        &mut window.window_total,
        config.rolling_window_seconds,
        config.rolling_volume_limit,
        amount,
        now,
        BridgeError::ExceedsRollingVolumeLimit,
    )
}

/// The reserve withdrawal budget: checks and records `amount` against
/// [`RebalancePolicy::rolling_limit`] over its own fixed bucket.
///
/// This is a DEDICATED budget, deliberately not shared with either
/// settlement direction's [`RollingVolumeWindow`]. Two reasons, both
/// load-bearing:
///
/// 1. Operator withdrawals and user settlements are sized on completely
///    different scales; one budget would have to be loose enough for the
///    larger, which would make it useless for the smaller.
/// 2. `instructions::admin::reset_rolling_volume_window` can reset a
///    `RollingVolumeWindow` on the admin's signature alone. Sharing that
///    account would hand a compromised admin a one-transaction reset of
///    the budget that exists to contain a compromised admin. Nothing in
///    this program resets the policy's window; it only ages out.
pub fn enforce_and_record_rebalance_volume(
    policy: &mut RebalancePolicy,
    amount: u64,
    now: i64,
) -> Result<()> {
    let (limit, window_seconds) = (policy.rolling_limit, policy.rolling_window_seconds);
    enforce_and_record_bucket(
        &mut policy.window_start,
        &mut policy.window_total,
        window_seconds,
        limit,
        amount,
        now,
        BridgeError::ExceedsRebalanceRollingLimit,
    )
}

/// Checks a treasury withdrawal against [`RebalancePolicy::per_withdrawal_limit`].
///
/// Deliberately NOT `BridgeConfig.per_transfer_limit`: that limit is sized
/// for user settlements and is editable immediately on the admin's
/// signature alone, so reusing it would have left the withdrawal ceiling
/// under the control of the exact key this limit exists to bound. The
/// policy's own limit is threshold-approved and timelocked.
pub fn enforce_rebalance_per_withdrawal_limit(policy: &RebalancePolicy, amount: u64) -> Result<()> {
    require!(
        amount <= policy.per_withdrawal_limit,
        BridgeError::ExceedsRebalancePerWithdrawalLimit
    );
    Ok(())
}

/// Checks that releasing `amount` from a reserve currently holding
/// `reserve_balance` would not breach `protected_minimum` (constraint 6:
/// reserve insufficiency fails closed). `checked_add` rather than a
/// subtract-then-compare: an amount large enough to underflow the subtract
/// must be rejected outright, not wrap into a small number that passes.
pub fn enforce_protected_minimum(
    reserve_balance: u64,
    protected_minimum: u64,
    amount: u64,
) -> Result<()> {
    let required_floor = amount
        .checked_add(protected_minimum)
        .ok_or(BridgeError::ArithmeticOverflow)?;
    require!(
        reserve_balance >= required_floor,
        BridgeError::InsufficientReserveBalance
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(min: u64, max: u64, rolling_limit: u64, window_seconds: i64) -> BridgeConfig {
        BridgeConfig {
            protocol_version: 1,
            admin: Pubkey::new_unique(),
            pending_admin: None,
            paused: false,
            release_paused: false,
            deposit_paused: false,
            bump: 0,
            reserve_token_mint: Pubkey::new_unique(),
            reserve_token_program: anchor_spl::token::ID,
            reserve_authority_bump: 0,
            obligation_count: 0,
            governance_timelock_seconds: 3600,
            min_transfer_amount: min,
            per_transfer_limit: max,
            protected_minimum: 0,
            rolling_volume_limit: rolling_limit,
            rolling_window_seconds: window_seconds,
            upgrade_timelock_seconds: 3600,
        }
    }

    fn window(start: i64, total: u64) -> RollingVolumeWindow {
        RollingVolumeWindow {
            direction: crate::state::Direction::GoldcoinToSolana,
            window_start: start,
            window_total: total,
            bump: 0,
            reserved: [0u8; 16],
        }
    }

    #[test]
    fn rejects_zero_amount() {
        let c = config(0, 1000, 1000, 3600);
        assert_eq!(
            enforce_transfer_amount(&c, 0).unwrap_err(),
            Error::from(BridgeError::ZeroAmount)
        );
    }

    #[test]
    fn rejects_below_minimum() {
        let c = config(10, 1000, 1000, 3600);
        assert_eq!(
            enforce_transfer_amount(&c, 5).unwrap_err(),
            Error::from(BridgeError::BelowMinimumTransfer)
        );
    }

    #[test]
    fn rejects_above_per_transfer_limit() {
        let c = config(0, 1000, 1000, 3600);
        assert_eq!(
            enforce_transfer_amount(&c, 1001).unwrap_err(),
            Error::from(BridgeError::ExceedsPerTransferLimit)
        );
    }

    #[test]
    fn accepts_within_bounds() {
        let c = config(10, 1000, 1000, 3600);
        assert!(enforce_transfer_amount(&c, 500).is_ok());
    }

    /// The 2026-08-29 production values, in the Solana mint's 6-decimal
    /// atomic units: `per_transfer_limit` = 20,000 GLC = 20_000_000_000
    /// (raised from 2,000 GLC), `min_transfer_amount` = 99 GLC =
    /// 99_000_000 (unchanged — the NET-side floor; the fee-adjusted
    /// GROSS entry minimum the UI derives from it is 102.061856 GLC at
    /// the 3% fee — docs/22-production-readiness-review.md's 2026-08-29
    /// update note). The generic-value tests above prove the comparison
    /// logic; this one pins the real configured boundary so a
    /// fat-fingered production value or a unit mix-up (8-decimal
    /// canonical vs 6-decimal mint) shows up as a test failure, not a
    /// live incident.
    #[test]
    fn production_limits_accept_20_000_glc_and_reject_anything_above() {
        const MIN_TRANSFER: u64 = 99_000_000; // 99 GLC net floor, 6 decimals
        const PER_TRANSFER_LIMIT: u64 = 20_000_000_000; // 20,000 GLC, 6 decimals
        let c = config(MIN_TRANSFER, PER_TRANSFER_LIMIT, u64::MAX, 3600);
        // Exactly 20,000 GLC is accepted.
        assert!(enforce_transfer_amount(&c, 20_000_000_000).is_ok());
        // One atomic unit above is rejected.
        assert_eq!(
            enforce_transfer_amount(&c, 20_000_000_001).unwrap_err(),
            Error::from(BridgeError::ExceedsPerTransferLimit)
        );
        // The 3%-fee-adjusted gross minimum the UI derives (102.061856
        // GLC) clears the floor with its net: 102_061_856 - 3% floored
        // = 99_000_001 >= 99_000_000.
        assert!(enforce_transfer_amount(&c, 99_000_001).is_ok());
        // The exact configured floor is accepted; one below is not.
        assert!(enforce_transfer_amount(&c, 99_000_000).is_ok());
        assert_eq!(
            enforce_transfer_amount(&c, 98_999_999).unwrap_err(),
            Error::from(BridgeError::BelowMinimumTransfer)
        );
    }

    #[test]
    fn rolling_window_accumulates_within_bucket() {
        let c = config(0, 1000, 1000, 3600);
        let mut w = window(0, 400);
        assert!(enforce_and_record_rolling_volume(&c, &mut w, 500, 100).is_ok());
        assert_eq!(w.window_total, 900);
        assert_eq!(w.window_start, 0);
    }

    #[test]
    fn rolling_window_rejects_when_over_limit() {
        let c = config(0, 1000, 1000, 3600);
        let mut w = window(0, 900);
        let err = enforce_and_record_rolling_volume(&c, &mut w, 200, 100).unwrap_err();
        assert_eq!(err, Error::from(BridgeError::ExceedsRollingVolumeLimit));
        // Rejected transfer must not mutate the window.
        assert_eq!(w.window_total, 900);
    }

    #[test]
    fn rolling_window_resets_after_bucket_expires() {
        let c = config(0, 1000, 1000, 3600);
        let mut w = window(0, 900);
        // now - window_start = 3600 >= rolling_window_seconds -> reset.
        assert!(enforce_and_record_rolling_volume(&c, &mut w, 200, 3600).is_ok());
        assert_eq!(w.window_total, 200);
        assert_eq!(w.window_start, 3600);
    }

    #[test]
    fn protected_minimum_allows_exact_floor() {
        assert!(enforce_protected_minimum(1000, 500, 500).is_ok());
    }

    #[test]
    fn protected_minimum_rejects_breach() {
        let err = enforce_protected_minimum(1000, 500, 501).unwrap_err();
        assert_eq!(err, Error::from(BridgeError::InsufficientReserveBalance));
    }

    #[test]
    fn protected_minimum_rejects_amount_exceeding_balance_without_underflow() {
        // amount + protected_minimum overflows u64; must fail closed via the
        // checked_add, never wrap into a small number that passes.
        let err = enforce_protected_minimum(100, u64::MAX, 1).unwrap_err();
        assert_eq!(err, Error::from(BridgeError::ArithmeticOverflow));
    }

    #[test]
    fn protected_minimum_rejects_amount_alone_exceeding_balance() {
        // amount alone exceeds reserve_balance (no overflow involved); must
        // fail closed as ordinary insufficiency.
        let err = enforce_protected_minimum(100, 0, u64::MAX).unwrap_err();
        assert_eq!(err, Error::from(BridgeError::InsufficientReserveBalance));
    }
}
