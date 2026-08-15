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
    #[error(
        "recomputed fee breakdown for gross {gross} disagrees with the stored ledger record \
         (stored fee {stored_fee}, recomputed {recomputed_fee}; stored net {stored_net}, \
         recomputed {recomputed_net}) — refusing to settle on an inconsistent accounting record"
    )]
    AccountingMismatch {
        gross: u64,
        stored_fee: u64,
        recomputed_fee: u64,
        stored_net: u64,
        recomputed_net: u64,
    },
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

// ------------------------------------------------------------------------
// Canonical accounting unit + bridge fee (fee/reserve-capacity accounting
// pass, docs/20-bridge-fee.md).
//
// Every ledger-level bridge obligation, reserve-capacity figure, gross
// amount, fee amount, and net amount is denominated in ONE canonical unit:
// [`CanonicalAtomic`], 8 decimals — numerically identical to Goldcoin's own
// native atomic unit (`GOLDCOIN_DECIMALS`), the finer of the two real,
// verified chain precisions today. This is what
// `docs/18-token-2022-support.md`'s flagged "reserve-capacity accounting-
// unit gap" resolves to: `bridge_requests`' gross/fee/net columns are
// always canonical, for both directions, never source-chain-native.
//
// [`SolanaAtomic`] is the OTHER unit that ever appears in this accounting
// pipeline: the reserve mint's own live decimals, needed only at the two
// points a canonical amount must become (or came from) a real Solana
// transfer amount. The two types deliberately do not implement any
// arithmetic or comparison against each other — Rust's type system itself
// is what makes `SolanaAtomic(5) < CanonicalAtomic(5)` a compile error
// rather than a silent unit-confusion bug, satisfying the "structurally
// difficult or impossible" requirement without runtime checks.
// ------------------------------------------------------------------------

/// Basis-point denominator: `fee_bps / BPS_DENOMINATOR` is the fee rate.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// The bridge's fee rate: exactly 1.00%. A compile-time constant, never a
/// runtime parameter to any signing/attestation function — see
/// docs/20-bridge-fee.md's "fee-bypass protections" section for why that
/// specific design choice is what makes "altered fee_bps" structurally
/// impossible rather than merely checked.
pub const BRIDGE_FEE_BPS: u64 = 100;

/// An amount in the ledger's canonical accounting unit (8 decimals,
/// numerically identical to Goldcoin's own native atomic unit). See the
/// module-level section above.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalAtomic(pub u64);

/// An amount in the Solana reserve mint's own live decimals. Never
/// compared or combined with [`CanonicalAtomic`] directly — convert first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SolanaAtomic(pub u64);

impl CanonicalAtomic {
    pub const ZERO: CanonicalAtomic = CanonicalAtomic(0);

    pub fn checked_add(self, rhs: CanonicalAtomic) -> Result<CanonicalAtomic, ConversionError> {
        self.0
            .checked_add(rhs.0)
            .map(CanonicalAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    pub fn checked_sub(self, rhs: CanonicalAtomic) -> Result<CanonicalAtomic, ConversionError> {
        self.0
            .checked_sub(rhs.0)
            .map(CanonicalAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    /// Converts to the reserve mint's own live decimals. Delegates to
    /// [`goldcoin_to_solana_atomic`] — the one, canonical conversion
    /// implementation; this is a typed wrapper around it, not a second one.
    pub fn to_solana(self, solana_decimals: u8) -> Result<SolanaAtomic, ConversionError> {
        goldcoin_to_solana_atomic(self.0, solana_decimals).map(SolanaAtomic)
    }
}

impl SolanaAtomic {
    pub const ZERO: SolanaAtomic = SolanaAtomic(0);

    pub fn checked_add(self, rhs: SolanaAtomic) -> Result<SolanaAtomic, ConversionError> {
        self.0
            .checked_add(rhs.0)
            .map(SolanaAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    pub fn checked_sub(self, rhs: SolanaAtomic) -> Result<SolanaAtomic, ConversionError> {
        self.0
            .checked_sub(rhs.0)
            .map(SolanaAtomic)
            .ok_or(ConversionError::Overflow(self.0))
    }

    /// Converts to the ledger's canonical unit. Delegates to
    /// [`solana_to_goldcoin_atomic`] — see [`CanonicalAtomic::to_solana`].
    pub fn to_canonical(self, solana_decimals: u8) -> Result<CanonicalAtomic, ConversionError> {
        solana_to_goldcoin_atomic(self.0, solana_decimals).map(CanonicalAtomic)
    }
}

/// A fully reconciled gross/fee/net breakdown, all in canonical units:
/// `gross == fee + net` holds BY CONSTRUCTION (`net` is derived as
/// `gross - fee`, never computed independently), so this type cannot
/// represent an inconsistent state — see [`compute_fee`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeBreakdown {
    pub gross: CanonicalAtomic,
    pub fee_bps: u64,
    pub fee: CanonicalAtomic,
    pub net: CanonicalAtomic,
}

/// Computes the bridge fee for `gross` (canonical units) at the fixed
/// [`BRIDGE_FEE_BPS`] rate. `fee` is floored (rounds toward the user, in
/// the user's favor, never up) via checked integer arithmetic — never
/// floating point. `net = gross - fee` is derived, not independently
/// computed, so `gross == fee + net` is a structural guarantee of
/// [`FeeBreakdown`], not something callers must separately verify.
///
/// A `gross` too small for `BRIDGE_FEE_BPS` of it to matter (e.g. `gross <
/// BPS_DENOMINATOR / BRIDGE_FEE_BPS`) still succeeds here — `fee` is
/// simply `0` and `net == gross`. Whether a given gross amount can
/// actually SETTLE end to end (i.e. whether `net` survives the
/// destination chain's own decimal precision exactly) is a SEPARATE
/// question, checked at the point `net` is converted to the destination
/// chain's native unit (see docs/20-bridge-fee.md's "smallest
/// mathematically valid gross amount" analysis) — `compute_fee` itself
/// never rejects an amount on exactness grounds, only on overflow.
pub fn compute_fee(gross: CanonicalAtomic) -> Result<FeeBreakdown, ConversionError> {
    let scaled = gross
        .0
        .checked_mul(BRIDGE_FEE_BPS)
        .ok_or(ConversionError::Overflow(gross.0))?;
    let fee = scaled / BPS_DENOMINATOR; // floor; BPS_DENOMINATOR is a nonzero constant
    let net = gross
        .0
        .checked_sub(fee)
        .ok_or(ConversionError::Overflow(gross.0))?;
    Ok(FeeBreakdown {
        gross,
        fee_bps: BRIDGE_FEE_BPS,
        fee: CanonicalAtomic(fee),
        net: CanonicalAtomic(net),
    })
}

/// Recomputes the fee breakdown for `gross_atomic` from scratch and
/// asserts it matches `stored_fee_atomic`/`stored_net_atomic` — the
/// ledger's own persisted record for a request (docs/20-bridge-fee.md's
/// "FAIL CLOSED on accounting inconsistencies" requirement). Every
/// settlement-construction call site (attestation signing, release-
/// instruction building, the Goldcoin payout plan) uses this rather than
/// trusting the stored fee/net columns directly: the RETURNED breakdown
/// is always the freshly recomputed one, never the stored one, so even if
/// `stored_fee_atomic`/`stored_net_atomic` were somehow tampered with
/// in the database, they are never actually used to build a real
/// settlement — only compared against, and rejected on mismatch.
pub fn verify_fee_breakdown(
    gross_atomic: u64,
    stored_fee_atomic: u64,
    stored_net_atomic: u64,
) -> Result<FeeBreakdown, ConversionError> {
    let fb = compute_fee(CanonicalAtomic(gross_atomic))?;
    if fb.fee.0 != stored_fee_atomic || fb.net.0 != stored_net_atomic {
        return Err(ConversionError::AccountingMismatch {
            gross: gross_atomic,
            stored_fee: stored_fee_atomic,
            recomputed_fee: fb.fee.0,
            stored_net: stored_net_atomic,
            recomputed_net: fb.net.0,
        });
    }
    Ok(fb)
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

    // --------------------------------------------------------------- fee --

    #[test]
    fn hundred_glc_gross_charges_one_glc_fee() {
        // 100 GLC gross -> 1 GLC fee -> 99 GLC net.
        let gross = CanonicalAtomic(100 * 100_000_000);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic(100_000_000));
        assert_eq!(fb.net, CanonicalAtomic(99 * 100_000_000));
        assert_eq!(fb.fee_bps, 100);
    }

    #[test]
    fn thousand_glc_gross_charges_ten_glc_fee() {
        // 1,000 GLC gross -> 10 GLC fee -> 990 GLC net.
        let gross = CanonicalAtomic(1_000 * 100_000_000);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic(10 * 100_000_000));
        assert_eq!(fb.net, CanonicalAtomic(990 * 100_000_000));
    }

    #[test]
    fn gross_always_equals_fee_plus_net_by_construction() {
        for gross in [1u64, 2, 99, 100, 101, 12_345, 1_000_000, 999_999_999] {
            let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
            assert_eq!(fb.fee.0 + fb.net.0, fb.gross.0);
        }
    }

    #[test]
    fn fee_never_exceeds_gross_and_net_is_never_negative() {
        // At 100 bps (< BPS_DENOMINATOR), fee can never reach gross, so net
        // can never underflow — but assert it explicitly as a property,
        // not just an implementation detail of the formula.
        for gross in [0u64, 1, 50, 99, 100, u64::MAX / 200] {
            let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
            assert!(fb.fee.0 <= fb.gross.0);
            assert!(fb.net.0 <= fb.gross.0);
        }
    }

    #[test]
    fn fee_rounds_down_never_up() {
        // gross=1: 1*100/10000 = 0.01 -> floors to 0, never rounds up to 1
        // (rounding up would charge a fee larger than 1% of the amount).
        let fb = compute_fee(CanonicalAtomic(1)).unwrap();
        assert_eq!(fb.fee, CanonicalAtomic::ZERO);
        assert_eq!(fb.net, CanonicalAtomic(1));
    }

    #[test]
    fn fee_computation_overflows_closed_rather_than_wrapping() {
        // gross large enough that gross * BRIDGE_FEE_BPS overflows u64.
        let gross = CanonicalAtomic(u64::MAX / 10); // *100 overflows u64
        assert_eq!(compute_fee(gross), Err(ConversionError::Overflow(gross.0)));
    }

    #[test]
    fn very_large_valid_gross_computes_without_overflow() {
        // The largest gross for which gross * BRIDGE_FEE_BPS still fits in
        // u64.
        let gross = CanonicalAtomic(u64::MAX / BRIDGE_FEE_BPS);
        let fb = compute_fee(gross).unwrap();
        assert_eq!(fb.fee.0 + fb.net.0, fb.gross.0);
    }

    #[test]
    fn typed_units_reject_mixing_at_compile_time() {
        // This test's real assertion is that the file compiles at all:
        // CanonicalAtomic and SolanaAtomic have no shared arithmetic/
        // comparison impl, so `CanonicalAtomic(1) == SolanaAtomic(1)` (or
        // any direct comparison/addition between the two) is a compile
        // error, not a runtime bug. Nothing to execute; documents the
        // property under test.
        let c = CanonicalAtomic(500_000_000);
        let s = c.to_solana(6).unwrap();
        assert_eq!(s, SolanaAtomic(5_000_000));
        assert_eq!(s.to_canonical(6).unwrap(), c);
    }

    /// GLC -> Solana direction: the smallest canonical gross amount whose
    /// NET (after the 1% fee) survives conversion to the canonical mint's
    /// 6-decimal precision exactly, per docs/20-bridge-fee.md's
    /// "mathematically smallest valid gross amount" analysis. Computed
    /// here by brute force rather than hardcoded, so this test would catch
    /// a regression in either the fee formula or the conversion policy,
    /// not just pin a number.
    #[test]
    fn smallest_valid_glc_to_solana_gross_is_101_canonical_atomic_units() {
        let solana_decimals = 6u8;
        let smallest = (1u64..10_000)
            .find(|&gross| {
                let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
                fb.net.0 > 0 && fb.net.to_solana(solana_decimals).is_ok()
            })
            .expect("a valid gross must exist well within this search bound");
        assert_eq!(smallest, 101);

        // One canonical atomic unit below is invalid for either reason
        // (net==0 or non-exact conversion) for every smaller gross.
        for gross in 1..smallest {
            let fb = compute_fee(CanonicalAtomic(gross)).unwrap();
            let valid = fb.net.0 > 0 && fb.net.to_solana(solana_decimals).is_ok();
            assert!(
                !valid,
                "gross {gross} should not be a valid GLC->Solana amount"
            );
        }
    }

    /// Solana -> GLC direction: canonical is Goldcoin-native already, so
    /// converting gross (Solana-native, widening) to canonical is always
    /// exact — the only constraint is `net > 0`. Smallest valid Solana
    /// gross is therefore 1 atomic unit (1e-6 GLC).
    #[test]
    fn smallest_valid_solana_to_glc_gross_is_one_solana_atomic_unit() {
        let solana_decimals = 6u8;
        let smallest = (1u64..10_000)
            .find(|&gross_solana| {
                let gross_canonical = SolanaAtomic(gross_solana)
                    .to_canonical(solana_decimals)
                    .unwrap();
                let fb = compute_fee(gross_canonical).unwrap();
                fb.net.0 > 0
            })
            .expect("a valid gross must exist well within this search bound");
        assert_eq!(smallest, 1);
    }

    // --------------------------------------------------- verify_fee_breakdown --

    #[test]
    fn verify_fee_breakdown_accepts_a_correctly_reconciled_record() {
        let fb = verify_fee_breakdown(100_000, 1_000, 99_000).unwrap();
        assert_eq!(fb.fee.0, 1_000);
        assert_eq!(fb.net.0, 99_000);
    }

    #[test]
    fn verify_fee_breakdown_rejects_a_tampered_fee_amount() {
        // Real breakdown for gross=100_000 is fee=1_000/net=99_000; an
        // attacker (or corrupted row) claims a smaller fee while leaving net
        // untouched — `gross == fee + net` would then also be violated, but
        // this must fail closed on the fee mismatch itself, not rely on
        // that secondary check.
        let result = verify_fee_breakdown(100_000, 500, 99_000);
        assert!(matches!(
            result,
            Err(ConversionError::AccountingMismatch {
                gross: 100_000,
                stored_fee: 500,
                recomputed_fee: 1_000,
                stored_net: 99_000,
                recomputed_net: 99_000,
            })
        ));
    }

    #[test]
    fn verify_fee_breakdown_rejects_a_tampered_net_amount() {
        // Fee left correct but net inflated — the classic "keep the fee
        // small so it looks plausible, inflate what you actually receive"
        // tamper attempt.
        let result = verify_fee_breakdown(100_000, 1_000, 100_000);
        assert!(matches!(
            result,
            Err(ConversionError::AccountingMismatch {
                gross: 100_000,
                stored_fee: 1_000,
                recomputed_fee: 1_000,
                stored_net: 100_000,
                recomputed_net: 99_000,
            })
        ));
    }

    #[test]
    fn verify_fee_breakdown_rejects_gross_ne_fee_plus_net() {
        // Both fee and net are individually wrong in a way that doesn't
        // even sum back to the claimed gross.
        let result = verify_fee_breakdown(100_000, 2_000, 99_000);
        assert!(result.is_err());
    }

    #[test]
    fn verify_fee_breakdown_rejects_a_tampered_fee_bps_effect() {
        // There is no `fee_bps` parameter to tamper with here by design
        // (docs/20-bridge-fee.md: `BRIDGE_FEE_BPS` is a compile-time
        // constant, never threaded through as data) — the closest a tamper
        // attempt can get is claiming a fee/net pair consistent with a
        // DIFFERENT bps rate, e.g. 50 bps instead of the real 100.
        let wrong_bps_fee = 100_000 * 50 / 10_000; // what 0.5% would have charged
        let wrong_bps_net = 100_000 - wrong_bps_fee;
        let result = verify_fee_breakdown(100_000, wrong_bps_fee, wrong_bps_net);
        assert!(matches!(
            result,
            Err(ConversionError::AccountingMismatch { .. })
        ));
    }

    // ------------------------------------------ typed checked_add/checked_sub --

    #[test]
    fn canonical_checked_add_overflows_closed_rather_than_wrapping() {
        let result = CanonicalAtomic(u64::MAX).checked_add(CanonicalAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    #[test]
    fn canonical_checked_sub_underflows_closed_rather_than_wrapping() {
        let result = CanonicalAtomic(0).checked_sub(CanonicalAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    #[test]
    fn solana_checked_add_overflows_closed_rather_than_wrapping() {
        let result = SolanaAtomic(u64::MAX).checked_add(SolanaAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }

    #[test]
    fn solana_checked_sub_underflows_closed_rather_than_wrapping() {
        let result = SolanaAtomic(0).checked_sub(SolanaAtomic(1));
        assert!(matches!(result, Err(ConversionError::Overflow(_))));
    }
}
