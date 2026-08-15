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
use crate::state::{BridgeConfig, RollingVolumeWindow};

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

/// Resets the bucket if it has expired, then checks and records `amount`
/// against the rolling volume limit. Mutates `window` only on success — a
/// rejected transfer must never advance the window's state.
pub fn enforce_and_record_rolling_volume(
    config: &BridgeConfig,
    window: &mut RollingVolumeWindow,
    amount: u64,
    now: i64,
) -> Result<()> {
    let bucket_age = now.saturating_sub(window.window_start);
    let (start, total) = if bucket_age >= config.rolling_window_seconds {
        (now, 0u64)
    } else {
        (window.window_start, window.window_total)
    };
    let projected = total
        .checked_add(amount)
        .ok_or(BridgeError::ArithmeticOverflow)?;
    require!(
        projected <= config.rolling_volume_limit,
        BridgeError::ExceedsRollingVolumeLimit
    );
    window.window_start = start;
    window.window_total = projected;
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
