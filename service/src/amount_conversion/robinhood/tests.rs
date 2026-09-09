//! Tests for the Robinhood 18-decimal amount model.
//!
//! Structure mirrors the exactness argument in the module docs: the
//! widening direction is proved total over the whole `u64` domain, the
//! narrowing direction is probed at every boundary where it must fail,
//! the ABI word is probed at `u128::MAX` and beyond, and a round-trip
//! property closes the loop over sampled values. Property coverage is
//! written as deterministic loops over a fixed generator rather than
//! pulling in `proptest`, matching the parent module's existing
//! `round_trip_is_lossless_for_amounts_expressible_in_both_precisions`
//! style and keeping the audited dependency tree unchanged.

use super::*;

/// One whole GLC in canonical 8-decimal atomic units.
const ONE_GLC_CANONICAL: u64 = 100_000_000;
/// One whole GLC in Robinhood 18-decimal atomic units.
const ONE_GLC_ROBINHOOD: u128 = 1_000_000_000_000_000_000;

// ------------------------------------------------------------ constants --

#[test]
fn the_scale_constant_is_ten_to_the_decimal_difference() {
    // The longhand literal must equal the value its definition claims —
    // a typo here would mis-scale every amount the bridge ever moves.
    assert_eq!(
        CANONICAL_TO_ROBINHOOD_SCALE,
        10u128.pow(ROBINHOOD_DECIMALS - GOLDCOIN_DECIMALS)
    );
    assert_eq!(CANONICAL_TO_ROBINHOOD_SCALE, 10_000_000_000);
    assert_eq!(ROBINHOOD_DECIMALS, 18);
    assert_eq!(GOLDCOIN_DECIMALS, 8);
}

#[test]
fn one_glc_is_consistent_across_both_units() {
    assert_eq!(
        u128::from(ONE_GLC_CANONICAL) * CANONICAL_TO_ROBINHOOD_SCALE,
        ONE_GLC_ROBINHOOD
    );
}

// --------------------------------------------------- widening (C -> R) --

/// Table-driven cover of the amounts the handoff design actually cares
/// about: nothing, the smallest indivisible unit, a unit amount, the
/// `minTransfer` figure, the per-transfer limit, the rolling limit, and
/// the arithmetic extreme of the canonical type.
#[test]
fn canonical_widens_exactly_at_every_designed_amount() {
    let cases: &[(u64, u128)] = &[
        // zero
        (0, 0),
        // one canonical atomic unit (1e-8 GLC) -> 1e10 wei-equivalent
        (1, CANONICAL_TO_ROBINHOOD_SCALE),
        // 1 GLC
        (ONE_GLC_CANONICAL, ONE_GLC_ROBINHOOD),
        // 99 GLC (designed minTransfer)
        (99 * ONE_GLC_CANONICAL, 99 * ONE_GLC_ROBINHOOD),
        // 20,000 GLC (designed perTransferLimit) — three orders of
        // magnitude past what a u64 could hold at 18 decimals.
        (20_000 * ONE_GLC_CANONICAL, 20_000 * ONE_GLC_ROBINHOOD),
        // 100,000 GLC (designed rollingLimit)
        (100_000 * ONE_GLC_CANONICAL, 100_000 * ONE_GLC_ROBINHOOD),
        // the canonical type's arithmetic ceiling
        (u64::MAX, 184_467_440_737_095_516_150_000_000_000),
    ];
    for &(canonical, expected) in cases {
        let widened = CanonicalAtomic(canonical)
            .to_robinhood()
            .unwrap_or_else(|e| panic!("widening {canonical} must be exact, got {e}"));
        assert_eq!(widened.get(), expected, "canonical {canonical}");
    }
}

#[test]
fn twenty_thousand_glc_overflows_a_u64_at_eighteen_decimals() {
    // The concrete reason RobinhoodAtomic is a u128 and not a u64. This
    // is the load-bearing fact behind risk R2 in the handoff register.
    let twenty_k = CanonicalAtomic(20_000 * ONE_GLC_CANONICAL)
        .to_robinhood()
        .unwrap();
    assert!(twenty_k.get() > u128::from(u64::MAX));
    assert!(u64::try_from(twenty_k.get()).is_err());
}

/// Requirement: prove that widening EVERY valid `u64` canonical amount by
/// 10^10 fits in `u128`.
///
/// Multiplication by a positive constant is strictly monotone over the
/// non-negative integers, so `c * SCALE <= u64::MAX * SCALE` for every
/// `c <= u64::MAX`. It therefore suffices to show the single worst case
/// does not overflow — which is what the first assertion does, in
/// `u128` arithmetic that would itself have to overflow to lie. The
/// remaining assertions pin the headroom so a future decimals change
/// cannot quietly erode it.
#[test]
fn widening_is_total_over_the_entire_u64_domain() {
    let worst_case = u128::from(u64::MAX)
        .checked_mul(CANONICAL_TO_ROBINHOOD_SCALE)
        .expect("u64::MAX * 10^10 must fit in u128");
    assert_eq!(worst_case, 184_467_440_737_095_516_150_000_000_000);
    assert!(worst_case < u128::MAX);

    // Headroom: u128::MAX is about 1.845 billion times the worst case.
    assert_eq!(u128::MAX / worst_case, 1_844_674_407);

    // And the constructor itself agrees at the boundary.
    assert_eq!(
        CanonicalAtomic(u64::MAX).to_robinhood().unwrap().get(),
        worst_case
    );
}

// -------------------------------------------------- narrowing (R -> C) --

#[test]
fn robinhood_narrows_exactly_at_every_designed_amount() {
    let cases: &[(u128, u64)] = &[
        (0, 0),
        // SCALE is accepted as exactly one canonical atomic unit
        (CANONICAL_TO_ROBINHOOD_SCALE, 1),
        (ONE_GLC_ROBINHOOD, ONE_GLC_CANONICAL),
        (20_000 * ONE_GLC_ROBINHOOD, 20_000 * ONE_GLC_CANONICAL),
        (100_000 * ONE_GLC_ROBINHOOD, 100_000 * ONE_GLC_CANONICAL),
    ];
    for &(robinhood, expected) in cases {
        let narrowed = RobinhoodAtomic::new(robinhood)
            .to_canonical()
            .unwrap_or_else(|e| panic!("narrowing {robinhood} must be exact, got {e}"));
        assert_eq!(narrowed, CanonicalAtomic(expected), "robinhood {robinhood}");
    }
}

#[test]
fn narrowing_rejects_a_single_wei_of_remainder_rather_than_rounding() {
    // 1 GLC + 1 wei-equivalent. Rounding down would strand 1 wei of the
    // depositor's entitlement in the reserve forever; rounding up would
    // mint GLC nobody deposited. Reject instead.
    let dusty = RobinhoodAtomic::new(ONE_GLC_ROBINHOOD + 1);
    assert_eq!(
        dusty.to_canonical().unwrap_err(),
        RobinhoodConversionError::NotExactlyRepresentable {
            robinhood: ONE_GLC_ROBINHOOD + 1,
            remainder: 1,
        }
    );
}

#[test]
fn narrowing_rejects_everything_strictly_below_one_canonical_unit() {
    // The whole open interval (0, SCALE) is unrepresentable. Check both
    // ends and the interior rather than only SCALE - 1.
    for robinhood in [
        1u128,
        2,
        CANONICAL_TO_ROBINHOOD_SCALE / 2,
        CANONICAL_TO_ROBINHOOD_SCALE - 1,
    ] {
        assert_eq!(
            RobinhoodAtomic::new(robinhood).to_canonical().unwrap_err(),
            RobinhoodConversionError::NotExactlyRepresentable {
                robinhood,
                remainder: robinhood,
            },
            "robinhood {robinhood} must be rejected, not floored to 0"
        );
    }
}

#[test]
fn scale_minus_one_is_rejected_and_scale_is_one_canonical_unit() {
    // The exact granularity boundary, spelled out on its own because the
    // contract enforces the same `% 10^10 == 0` rule on deposits.
    assert!(RobinhoodAtomic::new(CANONICAL_TO_ROBINHOOD_SCALE - 1)
        .to_canonical()
        .is_err());
    assert_eq!(
        RobinhoodAtomic::new(CANONICAL_TO_ROBINHOOD_SCALE)
            .to_canonical()
            .unwrap(),
        CanonicalAtomic(1)
    );
    assert!(RobinhoodAtomic::new(CANONICAL_TO_ROBINHOOD_SCALE + 1)
        .to_canonical()
        .is_err());
}

#[test]
fn narrowing_rejects_an_exact_multiple_whose_quotient_overflows_u64() {
    // Scale-aligned, so the remainder check passes — but the canonical
    // quantity is one unit past what the ledger's u64 can hold. Must be
    // rejected, not truncated.
    let robinhood = (u128::from(u64::MAX) + 1) * CANONICAL_TO_ROBINHOOD_SCALE;
    assert_eq!(
        RobinhoodAtomic::new(robinhood).to_canonical().unwrap_err(),
        RobinhoodConversionError::CanonicalOverflow {
            robinhood,
            canonical: u128::from(u64::MAX) + 1,
        }
    );

    // ...while u64::MAX itself still round-trips.
    assert_eq!(
        RobinhoodAtomic::new(u128::from(u64::MAX) * CANONICAL_TO_ROBINHOOD_SCALE)
            .to_canonical()
            .unwrap(),
        CanonicalAtomic(u64::MAX)
    );
}

#[test]
fn narrowing_the_largest_scale_aligned_u128_is_rejected_not_wrapped() {
    let largest_aligned = (u128::MAX / CANONICAL_TO_ROBINHOOD_SCALE) * CANONICAL_TO_ROBINHOOD_SCALE;
    assert!(matches!(
        RobinhoodAtomic::new(largest_aligned).to_canonical(),
        Err(RobinhoodConversionError::CanonicalOverflow { .. })
    ));
    // u128::MAX itself is not scale-aligned, so it fails the earlier check.
    assert!(matches!(
        RobinhoodAtomic::new(u128::MAX).to_canonical(),
        Err(RobinhoodConversionError::NotExactlyRepresentable { .. })
    ));
}

// ------------------------------------------------------- U256 boundary --

#[test]
fn u256_zero_decodes_to_zero() {
    assert_eq!(
        RobinhoodAtomic::try_from_u256(EvmU256::ZERO).unwrap(),
        RobinhoodAtomic::ZERO
    );
    assert_eq!(EvmU256::from_u128(0), EvmU256::ZERO);
}

#[test]
fn u256_decodes_u128_max_exactly() {
    let word = EvmU256::from_u128(u128::MAX);
    assert_eq!(word.to_be_bytes()[..16], [0u8; 16]);
    assert_eq!(word.to_be_bytes()[16..], [0xffu8; 16]);
    assert_eq!(
        RobinhoodAtomic::try_from_u256(word).unwrap().get(),
        u128::MAX
    );
}

#[test]
fn u256_rejects_u128_max_plus_one() {
    // 2^128 == the 33rd-from-last byte set to 1, everything below zero.
    let mut bytes = [0u8; 32];
    bytes[15] = 1;
    let word = EvmU256::from_be_bytes(bytes);
    assert_eq!(
        RobinhoodAtomic::try_from_u256(word).unwrap_err(),
        RobinhoodConversionError::U256ExceedsU128 { value: word }
    );
}

#[test]
fn u256_rejects_a_very_large_word() {
    // 2^256 - 1: every bit set. Truncating to the low 128 bits would
    // decode this as u128::MAX, a payable amount. It must not.
    let word = EvmU256::from_be_bytes([0xff; 32]);
    assert!(matches!(
        RobinhoodAtomic::try_from_u256(word),
        Err(RobinhoodConversionError::U256ExceedsU128 { .. })
    ));

    // A single high bit anywhere in the top 16 bytes is enough.
    for index in 0..16 {
        let mut bytes = [0u8; 32];
        bytes[index] = 1;
        assert!(
            RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes(bytes)).is_err(),
            "high byte {index} must force rejection"
        );
    }
}

#[test]
fn u256_widening_is_lossless_and_round_trips() {
    for value in [
        0u128,
        1,
        CANONICAL_TO_ROBINHOOD_SCALE,
        ONE_GLC_ROBINHOOD,
        20_000 * ONE_GLC_ROBINHOOD,
        u128::from(u64::MAX),
        u128::from(u64::MAX) + 1,
        u128::MAX,
    ] {
        let word = RobinhoodAtomic::new(value).to_u256();
        assert_eq!(word.try_to_u128().unwrap(), value);
        assert_eq!(RobinhoodAtomic::try_from_u256(word).unwrap().get(), value);
    }
}

#[test]
fn u256_byte_order_is_big_endian() {
    // 0x0102...  must land in the LOW bytes, high-order first — the ABI
    // byte order. A little-endian slip here would silently reinterpret
    // every inbound amount.
    let word = EvmU256::from_u128(0x0102_0304);
    let bytes = word.to_be_bytes();
    assert_eq!(bytes[28..], [0x01, 0x02, 0x03, 0x04]);
    assert!(bytes[..28].iter().all(|b| *b == 0));
    assert_eq!(EvmU256::from_be_bytes(bytes), word);
}

#[test]
fn u256_ordering_is_numeric() {
    // The derived Ord over big-endian bytes must agree with integer
    // ordering; a future limit check may rely on it.
    let mut sorted = [
        EvmU256::from_u128(u128::MAX),
        EvmU256::from_u128(1),
        EvmU256::from_be_bytes([0xff; 32]),
        EvmU256::ZERO,
        EvmU256::from_u128(ONE_GLC_ROBINHOOD),
    ];
    sorted.sort();
    assert_eq!(
        sorted,
        [
            EvmU256::ZERO,
            EvmU256::from_u128(1),
            EvmU256::from_u128(ONE_GLC_ROBINHOOD),
            EvmU256::from_u128(u128::MAX),
            EvmU256::from_be_bytes([0xff; 32]),
        ]
    );
}

#[test]
fn u256_displays_as_full_width_hex() {
    assert_eq!(
        EvmU256::ZERO.to_string(),
        "0x0000000000000000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(
        EvmU256::from_u128(u128::MAX).to_string(),
        "0x00000000000000000000000000000000ffffffffffffffffffffffffffffffff"
    );
}

// ------------------------------------------------- round-trip property --

/// canonical -> robinhood -> canonical is the identity, for every valid
/// canonical value sampled. Sampling is deterministic (a fixed
/// multiplicative LCG plus the exhaustive small-value prefix and the
/// designed amounts), so a failure is always reproducible.
#[test]
fn canonical_round_trip_is_the_identity() {
    let mut checked = 0u32;
    let mut check = |canonical: u64| {
        let robinhood = CanonicalAtomic(canonical).to_robinhood().unwrap();
        assert_eq!(
            robinhood.get(),
            u128::from(canonical) * CANONICAL_TO_ROBINHOOD_SCALE
        );
        assert_eq!(
            robinhood.to_canonical().unwrap(),
            CanonicalAtomic(canonical),
            "round trip must be the identity for {canonical}"
        );
        checked += 1;
    };

    // Exhaustive over the smallest values, where off-by-one scaling bugs
    // live.
    for canonical in 0u64..=10_000 {
        check(canonical);
    }

    // Every designed amount and every arithmetic edge.
    for canonical in [
        ONE_GLC_CANONICAL,
        99 * ONE_GLC_CANONICAL,
        20_000 * ONE_GLC_CANONICAL,
        100_000 * ONE_GLC_CANONICAL,
        u64::from(u32::MAX),
        u64::from(u32::MAX) + 1,
        u64::MAX / 2,
        u64::MAX - 1,
        u64::MAX,
    ] {
        check(canonical);
    }

    // Pseudo-random sweep across the whole u64 range. A 64-bit
    // multiplicative LCG (Knuth's constant), seeded fixed.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..20_000 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        check(state);
    }

    assert_eq!(checked, 10_001 + 9 + 20_000);
}

/// The reverse property: every scale-aligned Robinhood amount whose
/// canonical quotient fits `u64` narrows and re-widens to itself, and
/// every non-aligned neighbour is rejected.
#[test]
fn robinhood_round_trip_is_the_identity_for_aligned_amounts() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..20_000 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let aligned = u128::from(state) * CANONICAL_TO_ROBINHOOD_SCALE;

        let canonical = RobinhoodAtomic::new(aligned).to_canonical().unwrap();
        assert_eq!(canonical, CanonicalAtomic(state));
        assert_eq!(canonical.to_robinhood().unwrap().get(), aligned);

        // Its immediate neighbours are unrepresentable in both
        // directions — the alignment rule is exact, not approximate.
        assert!(RobinhoodAtomic::new(aligned + 1).to_canonical().is_err());
        if aligned > 0 {
            assert!(RobinhoodAtomic::new(aligned - 1).to_canonical().is_err());
        }
    }
}

/// The full chain a real GlcToRhn payout will take: canonical ledger
/// amount -> Robinhood unit -> ABI word -> back off the wire -> canonical.
#[test]
fn full_ledger_to_abi_and_back_round_trip() {
    for canonical in [
        0u64,
        1,
        ONE_GLC_CANONICAL,
        20_000 * ONE_GLC_CANONICAL,
        u64::MAX,
    ] {
        let outbound = CanonicalAtomic(canonical).to_robinhood().unwrap().to_u256();
        let inbound = RobinhoodAtomic::try_from_u256(outbound).unwrap();
        assert_eq!(inbound.to_canonical().unwrap(), CanonicalAtomic(canonical));
    }
}

// ------------------------------------------------- arithmetic + guards --

#[test]
fn checked_arithmetic_is_exact_and_fails_closed() {
    let one = RobinhoodAtomic::new(ONE_GLC_ROBINHOOD);
    let two = RobinhoodAtomic::new(2 * ONE_GLC_ROBINHOOD);
    assert_eq!(one.checked_add(one).unwrap(), two);
    assert_eq!(two.checked_sub(one).unwrap(), one);
    assert_eq!(one.checked_sub(one).unwrap(), RobinhoodAtomic::ZERO);

    assert_eq!(
        RobinhoodAtomic::new(u128::MAX)
            .checked_add(RobinhoodAtomic::new(1))
            .unwrap_err(),
        RobinhoodConversionError::ArithmeticOverflow {
            lhs: u128::MAX,
            op: "+",
            rhs: 1,
        }
    );
    assert_eq!(
        RobinhoodAtomic::ZERO
            .checked_sub(RobinhoodAtomic::new(1))
            .unwrap_err(),
        RobinhoodConversionError::ArithmeticOverflow {
            lhs: 0,
            op: "-",
            rhs: 1,
        }
    );
}

#[test]
fn zero_is_zero_in_both_representations() {
    assert_eq!(RobinhoodAtomic::ZERO.get(), 0);
    assert_eq!(
        CanonicalAtomic::ZERO.to_robinhood().unwrap(),
        RobinhoodAtomic::ZERO
    );
    assert_eq!(
        RobinhoodAtomic::ZERO.to_canonical().unwrap(),
        CanonicalAtomic::ZERO
    );
}

#[test]
fn decimals_are_asserted_not_assumed() {
    assert_eq!(ensure_robinhood_decimals(18), Ok(()));
    for observed in [0u8, 6, 8, 9, 17, 19, 255] {
        assert_eq!(
            ensure_robinhood_decimals(observed).unwrap_err(),
            RobinhoodConversionError::DecimalsMismatch {
                observed,
                expected: 18,
            }
        );
    }
}

#[test]
fn error_messages_name_the_offending_amount() {
    // The operator reading a parked request needs the actual number, not
    // a category.
    let err = RobinhoodAtomic::new(ONE_GLC_ROBINHOOD + 7)
        .to_canonical()
        .unwrap_err();
    let rendered = err.to_string();
    assert!(rendered.contains("1000000000000000007"), "{rendered}");
    assert!(rendered.contains('7'), "{rendered}");

    let err = RobinhoodAtomic::try_from_u256(EvmU256::from_be_bytes([0xff; 32])).unwrap_err();
    assert!(err.to_string().contains(&"ff".repeat(32)), "{err}");
}

#[test]
fn robinhood_atomic_displays_its_raw_unit() {
    assert_eq!(
        RobinhoodAtomic::new(ONE_GLC_ROBINHOOD).to_string(),
        "1000000000000000000"
    );
}

// ------------------------------------------- shared EvmU256 integration --
//
// These tests exist because this module briefly carried its OWN `EvmU256`,
// structurally identical to `crate::evm::EvmU256`. Deleting a duplicate is
// easy to do and easy to silently undo, so the property that matters is
// pinned here rather than left to a reviewer's memory: the word type this
// module consumes IS the crate-wide EVM primitive, not a look-alike.
//
// Every test below is written so it could not compile against a private
// duplicate. They name `crate::evm::EvmU256` through its full path, build
// words with constructors only the shared type has, and hand them straight
// to `RobinhoodAtomic`. If someone reintroduces a local `EvmU256`, these
// stop compiling with a type mismatch instead of quietly passing.

/// The type the amount model consumes is literally `crate::evm::EvmU256`.
/// A second, structurally identical type would fail this at compile time.
#[test]
fn amount_model_consumes_the_crate_wide_evm_word() {
    let shared: crate::evm::EvmU256 = crate::evm::EvmU256::from_u128(ONE_GLC_ROBINHOOD);
    assert_eq!(
        RobinhoodAtomic::try_from_u256(shared).unwrap(),
        RobinhoodAtomic::new(ONE_GLC_ROBINHOOD)
    );

    // ...and the outbound direction returns that same shared type.
    let out: crate::evm::EvmU256 = RobinhoodAtomic::new(ONE_GLC_ROBINHOOD).to_u256();
    assert_eq!(out, shared);
}

/// A word decoded by the shared primitive's own hex parser feeds the amount
/// model directly, with no bridging conversion anywhere. This is the path a
/// real `eth_getLogs` payload will take once the RPC layer lands.
#[test]
fn a_word_parsed_by_the_shared_primitive_feeds_the_amount_model() {
    let word = crate::evm::EvmU256::from_word_hex(
        "0x0000000000000000000000000000000000000000000000000de0b6b3a7640000",
    )
    .expect("valid full-width word");
    assert_eq!(
        RobinhoodAtomic::try_from_u256(word).unwrap(),
        RobinhoodAtomic::new(ONE_GLC_ROBINHOOD)
    );
}

/// Constructors that exist ONLY on the shared type (`try_from_be_slice`,
/// `From<u64>`, `MAX`, `ONE`) reach the amount model. A local duplicate
/// never had these, so this test is a structural proof of which type is in
/// play, not just a behavioural one.
#[test]
fn shared_only_constructors_reach_the_amount_model() {
    let from_slice = crate::evm::EvmU256::try_from_be_slice(&[0u8; 32]).unwrap();
    assert_eq!(
        RobinhoodAtomic::try_from_u256(from_slice).unwrap(),
        RobinhoodAtomic::new(0)
    );

    let from_u64: crate::evm::EvmU256 = u64::from(7u32).into();
    assert_eq!(
        RobinhoodAtomic::try_from_u256(from_u64).unwrap(),
        RobinhoodAtomic::new(7)
    );

    assert_eq!(
        RobinhoodAtomic::try_from_u256(crate::evm::EvmU256::ONE).unwrap(),
        RobinhoodAtomic::new(1)
    );

    // `MAX` is 2^256-1 and must be refused, not truncated.
    assert!(matches!(
        RobinhoodAtomic::try_from_u256(crate::evm::EvmU256::MAX),
        Err(RobinhoodConversionError::U256ExceedsU128 { .. })
    ));
}

/// The exact narrowing boundary, stated against the shared type: every word
/// up to and including `u128::MAX` converts exactly; the very next word is
/// refused. Off-by-one either way would be a truncation bug that silently
/// mints or destroys value.
#[test]
fn narrowing_boundary_is_exact_at_u128_max_on_the_shared_type() {
    let max = crate::evm::EvmU256::from_u128(u128::MAX);
    assert_eq!(
        RobinhoodAtomic::try_from_u256(max).unwrap(),
        RobinhoodAtomic::new(u128::MAX)
    );

    // u128::MAX + 1 == 2^128: bit 128 set, everything below zero.
    let mut bytes = [0u8; 32];
    bytes[15] = 0x01;
    let just_over = crate::evm::EvmU256::from_be_bytes(bytes);
    assert!(matches!(
        RobinhoodAtomic::try_from_u256(just_over),
        Err(RobinhoodConversionError::U256ExceedsU128 { .. })
    ));

    // And the boundary is genuinely one unit wide.
    assert_eq!(max.try_to_u128().unwrap(), u128::MAX);
    assert!(just_over.try_to_u128().is_err());
}

/// The shared primitive's error is translated into this module's vocabulary
/// rather than leaking out. A caller of the amount model must never have to
/// match on `EvmU256Error` to learn that an amount was unusable.
#[test]
fn shared_primitive_error_is_translated_not_leaked() {
    let err = RobinhoodAtomic::try_from_u256(crate::evm::EvmU256::MAX).unwrap_err();
    match err {
        RobinhoodConversionError::U256ExceedsU128 { value } => {
            // The offending word is carried through intact, full width.
            assert_eq!(value, crate::evm::EvmU256::MAX);
            assert_eq!(value.to_string(), format!("0x{}", "ff".repeat(32)));
        }
        other => panic!("expected U256ExceedsU128, got {other:?}"),
    }
}

/// Widening is total and lossless over the whole `u128` domain, checked at
/// the boundaries and across a deterministic sample, through the shared
/// type in both directions.
#[test]
fn widening_round_trips_through_the_shared_type() {
    let mut cases = vec![0u128, 1, 2, ONE_GLC_ROBINHOOD, u128::MAX, u128::MAX - 1];
    // Deterministic spread across the domain; no RNG, matching the file's
    // existing property-test style.
    let mut value = 3u128;
    for _ in 0..96 {
        cases.push(value);
        value = value.wrapping_mul(2_654_435_761).wrapping_add(1);
    }

    for raw in cases {
        let atomic = RobinhoodAtomic::new(raw);
        let word: crate::evm::EvmU256 = atomic.to_u256();
        assert_eq!(
            word.try_to_u128().unwrap(),
            raw,
            "word narrowing lost {raw}"
        );
        assert_eq!(
            RobinhoodAtomic::try_from_u256(word).unwrap(),
            atomic,
            "round trip lost {raw}"
        );
    }
}

// ------------------------------------------- observation floor (dust) --
//
// `to_canonical_floor` is the OBSERVATION rule (a pool balance nobody
// has a claim on yet), as distinct from `to_canonical`, which stays the
// CLAIM rule. These tests pin both halves of that distinction: the floor
// never differs from the exact conversion where the exact conversion
// succeeds, and where the exact conversion refuses, the floor reports
// the remainder rather than swallowing it.

#[test]
fn exact_eighteen_dp_balance_floors_to_the_same_canonical_value() {
    // The production figure this was written for: a bridge contract
    // holding 1,000,100 GLC, 18 dp in and 8 dp out, exactly.
    let observed = RobinhoodAtomic::new(1_000_100 * ONE_GLC_ROBINHOOD);
    let floor = observed
        .to_canonical_floor()
        .expect("exactly representable");

    assert_eq!(floor.canonical.0, 1_000_100 * ONE_GLC_CANONICAL);
    assert_eq!(floor.canonical.0, 100_010_000_000_000);
    assert_eq!(floor.remainder, 0);
    assert!(floor.is_exact());
    // And it agrees with the strict claim-side conversion, which is the
    // property that makes flooring safe to use for the common case.
    assert_eq!(floor.canonical, observed.to_canonical().unwrap());
}

#[test]
fn every_exactly_representable_amount_floors_to_its_exact_conversion() {
    // Sampled across ten orders of magnitude plus the boundary values —
    // wherever `to_canonical` succeeds, `to_canonical_floor` must agree
    // with it exactly and report no remainder. If these two ever
    // disagreed, the reserve would read differently depending on which
    // conversion a call site happened to pick.
    for canonical in [
        0u64,
        1,
        7,
        99_999_999,
        ONE_GLC_CANONICAL,
        20_000 * ONE_GLC_CANONICAL,
        1_000_100 * ONE_GLC_CANONICAL,
        u64::MAX,
    ] {
        let widened = RobinhoodAtomic::from_canonical(CanonicalAtomic(canonical)).unwrap();
        let floor = widened.to_canonical_floor().expect("exactly representable");
        assert_eq!(floor.canonical.0, canonical, "canonical {canonical}");
        assert_eq!(floor.remainder, 0, "canonical {canonical}");
        assert_eq!(floor.canonical, widened.to_canonical().unwrap());
    }
}

#[test]
fn one_wei_of_dust_floors_down_and_reports_the_remainder() {
    // The permissionless-transfer case. `to_canonical` refuses this
    // outright; the observation path must not, or a third party could
    // freeze reserve reconciliation for the price of one wei.
    let dusty = RobinhoodAtomic::new(1_000_100 * ONE_GLC_ROBINHOOD + 1);
    assert!(matches!(
        dusty.to_canonical(),
        Err(RobinhoodConversionError::NotExactlyRepresentable { remainder: 1, .. })
    ));

    let floor = dusty
        .to_canonical_floor()
        .expect("floors rather than refusing");
    assert_eq!(floor.canonical.0, 1_000_100 * ONE_GLC_CANONICAL);
    assert_eq!(floor.remainder, 1);
    assert!(!floor.is_exact());
}

#[test]
fn the_floor_is_always_a_lower_bound_and_the_remainder_is_sub_canonical() {
    // The safety argument in one property: flooring can only ever
    // UNDERSTATE the reserve, and never by as much as one canonical
    // unit. Understating is the pause-safe direction for the solvency
    // invariant `observed >= protected_minimum + pending_obligations`.
    for extra in [
        0u128,
        1,
        2,
        9_999,
        5_000_000_000,
        CANONICAL_TO_ROBINHOOD_SCALE - 1,
    ] {
        let observed = RobinhoodAtomic::new(42 * ONE_GLC_ROBINHOOD + extra);
        let floor = observed.to_canonical_floor().unwrap();

        assert!(
            floor.remainder < CANONICAL_TO_ROBINHOOD_SCALE,
            "extra {extra}"
        );
        assert_eq!(floor.remainder, extra, "extra {extra}");
        // Re-widening the floor must not exceed what was observed.
        let rewidened = RobinhoodAtomic::from_canonical(floor.canonical).unwrap();
        assert!(rewidened.get() <= observed.get(), "extra {extra}");
        assert_eq!(observed.get() - rewidened.get(), floor.remainder);
    }
}

#[test]
fn a_full_scale_of_dust_is_a_whole_canonical_unit_and_carries() {
    // The boundary: SCALE units of "dust" is not dust at all, it is one
    // canonical unit, and it must appear in the floor rather than in the
    // remainder.
    let observed = RobinhoodAtomic::new(CANONICAL_TO_ROBINHOOD_SCALE);
    let floor = observed.to_canonical_floor().unwrap();
    assert_eq!(floor.canonical.0, 1);
    assert_eq!(floor.remainder, 0);
}

#[test]
fn a_balance_too_large_for_the_canonical_unit_is_still_refused_not_truncated() {
    // Flooring relaxes the DUST rule only. An amount whose canonical
    // quantity overflows `u64` is not dust, and truncating it would
    // manufacture a reserve balance out of a malfunctioning or hostile
    // chain read — so it stays a hard failure, exactly as in
    // `to_canonical`.
    let overflowing =
        RobinhoodAtomic::new((u128::from(u64::MAX) + 1) * CANONICAL_TO_ROBINHOOD_SCALE);
    assert!(matches!(
        overflowing.to_canonical_floor(),
        Err(RobinhoodConversionError::CanonicalOverflow { .. })
    ));

    // ...and the same value with dust on top fails the same way, rather
    // than the dust path masking the overflow.
    let overflowing_dusty =
        RobinhoodAtomic::new((u128::from(u64::MAX) + 1) * CANONICAL_TO_ROBINHOOD_SCALE + 7);
    assert!(matches!(
        overflowing_dusty.to_canonical_floor(),
        Err(RobinhoodConversionError::CanonicalOverflow { .. })
    ));
}

#[test]
fn u256_words_beyond_u128_are_still_rejected_before_any_flooring() {
    // The ABI boundary is unchanged: a word too large to be a Robinhood
    // atomic amount never reaches the floor at all.
    let mut bytes = [0u8; 32];
    bytes[15] = 1; // 2^128
    let word = EvmU256::from_be_bytes(bytes);
    assert!(matches!(
        RobinhoodAtomic::try_from_u256(word),
        Err(RobinhoodConversionError::U256ExceedsU128 { .. })
    ));
}
