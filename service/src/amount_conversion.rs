//! Canonical Goldcoin<->Solana atomic-amount conversion policy (Task 3,
//! docs/18-token-2022-support.md).
//!
//! Goldcoin's native chain uses 8 decimals (Bitcoin-fork convention,
//! docs/goldcoin-rpc-notes.md — verified against a real Goldcoin node).
//! The canonical Solana GLC Token-2022 mint uses 6 decimals, verified
//! read-only against mainnet — but this module never hardcodes that
//! value: callers pass the live `decimals` they read from the mint
//! account, exactly as the on-chain program itself does
//! (`release_from_reserve`/`deposit_to_reserve` both read
//! `reserve_mint.decimals` live rather than trusting a compile-time
//! constant). Both chains represent the SAME underlying asset in
//! DIFFERENT atomic units, and this module is the only place a raw
//! atomic amount is allowed to cross from one chain's units to the
//! other's.
//!
//! # Policy
//!
//! Converting to a chain with MORE decimals (finer-grained units) is
//! always exact: multiply by the scale factor. Converting to a chain with
//! FEWER decimals (coarser-grained units) is exact ONLY when the source
//! amount's low-order digits beyond the destination's precision are all
//! zero; otherwise the amount cannot be represented exactly on the
//! destination side and conversion is REJECTED — never silently rounded
//! or truncated. Rounding down would permanently strand the depositor's
//! entitlement to the remainder inside the reserve; rounding up would
//! create GLC that was never deposited anywhere. Both are exactly the
//! "silently round value in a way that creates or destroys user
//! entitlement" failure Task 3 explicitly forbids — so this module fails
//! closed instead.

use std::cmp::Ordering;

/// Goldcoin's native-chain atomic-unit precision. Fixed by the Goldcoin
/// protocol itself (a Bitcoin fork), unlike the Solana side's decimals,
/// which are read live from the mint and passed into every function here.
pub const GOLDCOIN_DECIMALS: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConversionError {
    #[error(
        "amount {amount} atomic unit(s) at {from_decimals} decimals cannot be represented \
         exactly at {to_decimals} decimals — converting would silently strand/create {remainder} \
         atomic unit(s) of precision the destination chain cannot express"
    )]
    NotExactlyRepresentable {
        amount: u64,
        from_decimals: u32,
        to_decimals: u32,
        remainder: u64,
    },
    #[error("amount {0} overflows u64 when converted to the destination's finer precision")]
    Overflow(u64),
}

/// Converts `amount` from `from_decimals` atomic units to `to_decimals`
/// atomic units, representing the identical real-world GLC quantity.
/// Deterministic; never rounds. See module docs for the exactness policy.
pub fn convert_atomic_amount(
    amount: u64,
    from_decimals: u32,
    to_decimals: u32,
) -> Result<u64, ConversionError> {
    match from_decimals.cmp(&to_decimals) {
        Ordering::Equal => Ok(amount),
        Ordering::Less => {
            // Widening: destination is finer-grained, always exact.
            let scale = 10u64
                .checked_pow(to_decimals - from_decimals)
                .ok_or(ConversionError::Overflow(amount))?;
            amount
                .checked_mul(scale)
                .ok_or(ConversionError::Overflow(amount))
        }
        Ordering::Greater => {
            // Narrowing: destination is coarser-grained; exact only if
            // `amount` is already a multiple of the scale factor.
            let scale = 10u64
                .checked_pow(from_decimals - to_decimals)
                .ok_or(ConversionError::Overflow(amount))?;
            let remainder = amount % scale;
            if remainder != 0 {
                return Err(ConversionError::NotExactlyRepresentable {
                    amount,
                    from_decimals,
                    to_decimals,
                    remainder,
                });
            }
            Ok(amount / scale)
        }
    }
}

/// Converts a Goldcoin-atomic amount (8 decimals) into the equivalent
/// Solana-atomic amount at the mint's live `solana_decimals`. This is the
/// GLC->Solana release direction; today's canonical mint (6 decimals) is
/// a narrowing conversion, so it fails closed on any amount not divisible
/// by 100 rather than losing or fabricating GLC.
pub fn goldcoin_to_solana_atomic(
    goldcoin_atomic: u64,
    solana_decimals: u8,
) -> Result<u64, ConversionError> {
    convert_atomic_amount(
        goldcoin_atomic,
        GOLDCOIN_DECIMALS,
        u32::from(solana_decimals),
    )
}

/// Converts a Solana-atomic amount at the mint's live `solana_decimals`
/// into the equivalent Goldcoin-atomic amount (8 decimals). This is the
/// Solana->Goldcoin deposit direction; today's canonical mint (6
/// decimals) is a widening conversion and always exact.
pub fn solana_to_goldcoin_atomic(
    solana_atomic: u64,
    solana_decimals: u8,
) -> Result<u64, ConversionError> {
    convert_atomic_amount(solana_atomic, u32::from(solana_decimals), GOLDCOIN_DECIMALS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_decimals_is_the_identity() {
        assert_eq!(convert_atomic_amount(12345, 6, 6), Ok(12345));
    }

    #[test]
    fn widening_multiplies_exactly() {
        // 6 -> 8 decimals: scale by 100.
        assert_eq!(convert_atomic_amount(1, 6, 8), Ok(100));
        assert_eq!(convert_atomic_amount(123_456, 6, 8), Ok(12_345_600));
    }

    #[test]
    fn narrowing_divides_exactly_when_the_remainder_is_zero() {
        // 8 -> 6 decimals: scale by 100. 2.5 GLC at 8 decimals ->
        // 2.5 GLC at 6 decimals.
        assert_eq!(convert_atomic_amount(250_000_000, 8, 6), Ok(2_500_000));
    }

    #[test]
    fn narrowing_rejects_a_nonzero_remainder_rather_than_rounding() {
        // 8 -> 6 decimals: 0.000000_01 GLC (1 atomic unit at 8 decimals)
        // has no exact representation at 6 decimals.
        let err = convert_atomic_amount(1, 8, 6).unwrap_err();
        assert_eq!(
            err,
            ConversionError::NotExactlyRepresentable {
                amount: 1,
                from_decimals: 8,
                to_decimals: 6,
                remainder: 1,
            }
        );
    }

    #[test]
    fn narrowing_rejects_a_partial_remainder_not_just_the_full_scale() {
        // 250_000_001 at 8 decimals is 2.50000001 GLC — the trailing
        // "01" cannot survive a narrowing to 6 decimals.
        let err = convert_atomic_amount(250_000_001, 8, 6).unwrap_err();
        assert_eq!(
            err,
            ConversionError::NotExactlyRepresentable {
                amount: 250_000_001,
                from_decimals: 8,
                to_decimals: 6,
                remainder: 1,
            }
        );
    }

    #[test]
    fn goldcoin_to_solana_matches_the_canonical_six_decimal_mint() {
        // 2.5 GLC: glc_to_atomic gives 250_000_000 at 8 decimals.
        assert_eq!(goldcoin_to_solana_atomic(250_000_000, 6), Ok(2_500_000));
    }

    #[test]
    fn solana_to_goldcoin_matches_the_canonical_six_decimal_mint() {
        assert_eq!(solana_to_goldcoin_atomic(2_500_000, 6), Ok(250_000_000));
    }

    #[test]
    fn goldcoin_to_solana_rejects_dust_finer_than_the_solana_mint_can_hold() {
        // 1 atomic Goldcoin unit (1e-8 GLC) cannot be represented at 6
        // decimals (1e-6 GLC granularity) — must reject, not truncate to 0
        // (which would silently destroy the depositor's entitlement).
        assert!(goldcoin_to_solana_atomic(1, 6).is_err());
    }

    #[test]
    fn round_trip_is_lossless_for_amounts_expressible_in_both_precisions() {
        // Any amount that is a whole multiple of 0.01 GLC (100 atomic
        // units at 8 decimals) round-trips exactly through 6 decimals and
        // back, for every amount up to a reasonable bound — a property
        // check, not just a couple of fixed examples.
        for whole_hundredths in 0u64..=10_000 {
            let goldcoin_atomic = whole_hundredths * 100;
            let solana_atomic = goldcoin_to_solana_atomic(goldcoin_atomic, 6).unwrap();
            let back = solana_to_goldcoin_atomic(solana_atomic, 6).unwrap();
            assert_eq!(back, goldcoin_atomic);
        }
    }

    #[test]
    fn exact_conversion_never_changes_total_value_at_the_boundary_of_u64() {
        // A large, realistic reserve-sized amount that is exactly
        // representable must convert without overflow or loss.
        let goldcoin_atomic = 1_000_000_000_000u64; // 10,000 GLC
        let solana_atomic = goldcoin_to_solana_atomic(goldcoin_atomic, 6).unwrap();
        assert_eq!(solana_atomic, 10_000_000_000);
        assert_eq!(
            solana_to_goldcoin_atomic(solana_atomic, 6).unwrap(),
            goldcoin_atomic
        );
    }
}
