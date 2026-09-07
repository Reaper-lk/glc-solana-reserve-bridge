use super::*;

use crate::evm::hex::EvmHexError;

/// `u128::MAX` as a 256-bit word: the high 16 bytes zero, the low 16 all
/// `0xff`. The largest word that can be narrowed.
fn u128_max_word() -> EvmU256 {
    let mut bytes = [0u8; 32];
    bytes[16..].fill(0xff);
    EvmU256::from_be_bytes(bytes)
}

/// `u128::MAX + 1` as a 256-bit word: bit 128 set, everything else zero.
/// The smallest word that cannot be narrowed to a `u128`.
fn u128_max_plus_one_word() -> EvmU256 {
    let mut bytes = [0u8; 32];
    bytes[15] = 0x01;
    EvmU256::from_be_bytes(bytes)
}

// --- The required boundary values ------------------------------------

#[test]
fn zero_widens_and_narrows() {
    assert_eq!(EvmU256::from_u128(0), EvmU256::ZERO);
    assert_eq!(EvmU256::ZERO.try_to_u128().unwrap(), 0);
    assert_eq!(EvmU256::ZERO.try_to_u64().unwrap(), 0);
    assert!(EvmU256::ZERO.is_zero());
    assert_eq!(EvmU256::ZERO.to_be_bytes(), [0u8; 32]);
}

#[test]
fn one_widens_and_narrows() {
    let one = EvmU256::from_u128(1);
    assert_eq!(one, EvmU256::ONE);
    assert_eq!(one.try_to_u128().unwrap(), 1);
    assert_eq!(one.try_to_u64().unwrap(), 1);
    assert!(!one.is_zero());
    // Big-endian: the 1 lands in the LAST byte, not the first. Getting this
    // backwards would make every amount astronomically wrong.
    let mut expected = [0u8; 32];
    expected[31] = 1;
    assert_eq!(one.to_be_bytes(), expected);
}

#[test]
fn u128_max_round_trips_exactly() {
    let word = EvmU256::from_u128(u128::MAX);
    assert_eq!(word, u128_max_word());
    assert_eq!(word.try_to_u128().unwrap(), u128::MAX);
    // The high half must be untouched zeros: widening is right-aligned.
    assert_eq!(word.to_be_bytes()[..16], [0u8; 16]);
    assert_eq!(word.to_be_bytes()[16..], [0xffu8; 16]);
}

#[test]
fn u128_max_plus_one_is_refused_not_truncated() {
    let word = u128_max_plus_one_word();
    // The value is 2^128. Truncating to the low 128 bits would give 0 — a
    // silently free payout. It must be an error instead.
    let err = word.try_to_u128().unwrap_err();
    assert_eq!(
        err,
        EvmU256Error::Overflow {
            value: word,
            bits: 128
        }
    );
    let message = err.to_string();
    assert!(message.contains(&word.to_word_hex()), "{message}");
    assert!(message.contains("128"), "{message}");
}

#[test]
fn the_full_u256_max_is_refused_not_truncated() {
    // Low 128 bits of 2^256-1 are all ones, so a truncating implementation
    // would have returned u128::MAX and looked plausible.
    assert_eq!(
        EvmU256::MAX.try_to_u128().unwrap_err(),
        EvmU256Error::Overflow {
            value: EvmU256::MAX,
            bits: 128
        }
    );
    assert_eq!(
        EvmU256::MAX.try_to_u64().unwrap_err(),
        EvmU256Error::Overflow {
            value: EvmU256::MAX,
            bits: 64
        }
    );
    assert_eq!(EvmU256::MAX.to_be_bytes(), [0xffu8; 32]);
}

#[test]
fn u128_round_trips_across_the_whole_range() {
    let cases = [
        0u128,
        1,
        2,
        255,
        256,
        u64::MAX as u128,
        u64::MAX as u128 + 1,
        1_000_000_000_000_000_000, // one GLC at Robinhood's 18 decimals
        20_000_000_000_000_000_000_000, // the designed 20,000 GLC transfer limit
        u128::MAX - 1,
        u128::MAX,
    ];
    for value in cases {
        let word = EvmU256::from_u128(value);
        assert_eq!(word.try_to_u128().unwrap(), value, "value {value}");
        // And back out through the byte representation every EVM library
        // agrees on, which must not perturb anything.
        assert_eq!(
            EvmU256::from_be_bytes(word.to_be_bytes())
                .try_to_u128()
                .unwrap(),
            value
        );
    }
}

// --- u64 narrowing ---------------------------------------------------

#[test]
fn u64_narrowing_has_its_own_boundary() {
    let max = EvmU256::from_u64(u64::MAX);
    assert_eq!(max.try_to_u64().unwrap(), u64::MAX);
    assert_eq!(max.try_to_u128().unwrap(), u64::MAX as u128);

    let over = EvmU256::from_u128(u64::MAX as u128 + 1);
    assert_eq!(
        over.try_to_u64().unwrap_err(),
        EvmU256Error::Overflow {
            value: over,
            bits: 64
        }
    );
    // ...but it is still a perfectly good u128.
    assert_eq!(over.try_to_u128().unwrap(), u64::MAX as u128 + 1);
}

#[test]
fn from_u64_and_from_u128_agree() {
    for value in [0u64, 1, 255, 4663, u32::MAX as u64, u64::MAX] {
        assert_eq!(EvmU256::from_u64(value), EvmU256::from_u128(value as u128));
    }
}

// --- Conversion traits -----------------------------------------------

#[test]
fn the_conversion_traits_delegate_to_the_checked_methods() {
    let word: EvmU256 = 42u128.into();
    assert_eq!(word, EvmU256::from_u128(42));
    assert_eq!(u128::try_from(word).unwrap(), 42);
    assert_eq!(u64::try_from(word).unwrap(), 42);

    assert!(u128::try_from(EvmU256::MAX).is_err());
    assert!(u64::try_from(EvmU256::MAX).is_err());

    let from_u64: EvmU256 = 7u64.into();
    assert_eq!(from_u64, EvmU256::from_u64(7));
}

// --- Ordering --------------------------------------------------------

#[test]
fn ordering_over_big_endian_bytes_is_numeric_ordering() {
    assert!(EvmU256::ZERO < EvmU256::ONE);
    assert!(EvmU256::ONE < EvmU256::from_u128(2));
    assert!(u128_max_word() < u128_max_plus_one_word());
    assert!(u128_max_plus_one_word() < EvmU256::MAX);
    assert!(EvmU256::ZERO < EvmU256::MAX);

    // The case a little-endian mistake would get wrong: 0x0100 vs 0x00ff.
    // Numerically 256 > 255; byte-reversed it would compare the other way.
    assert!(EvmU256::from_u128(255) < EvmU256::from_u128(256));

    // And a broad monotonicity check across a decade of magnitudes.
    let mut previous = EvmU256::ZERO;
    for exponent in 0..38u32 {
        let value = 10u128.pow(exponent);
        let word = EvmU256::from_u128(value);
        assert!(
            previous < word,
            "10^{exponent} must exceed the previous power"
        );
        previous = word;
    }
}

#[test]
fn ordering_agrees_with_u128_ordering_over_sampled_pairs() {
    let samples = [
        0u128,
        1,
        7,
        255,
        256,
        65_535,
        u64::MAX as u128,
        u64::MAX as u128 + 1,
        u128::MAX / 3,
        u128::MAX,
    ];
    for left in samples {
        for right in samples {
            assert_eq!(
                EvmU256::from_u128(left).cmp(&EvmU256::from_u128(right)),
                left.cmp(&right),
                "{left} vs {right}"
            );
        }
    }
}

// --- Bytes and text --------------------------------------------------

#[test]
fn try_from_be_slice_requires_exactly_thirty_two_bytes() {
    assert_eq!(
        EvmU256::try_from_be_slice(&[0xffu8; 32]).unwrap(),
        EvmU256::MAX
    );
    assert_eq!(
        EvmU256::try_from_be_slice(&[0xffu8; 31]).unwrap_err(),
        EvmU256Error::WrongByteLength { actual: 31 }
    );
    assert_eq!(
        EvmU256::try_from_be_slice(&[0xffu8; 33]).unwrap_err(),
        EvmU256Error::WrongByteLength { actual: 33 }
    );
    // A 16-byte little/big-endian u128 is NOT a word: left-padding it here
    // would work by luck for big-endian input and be catastrophic for
    // little-endian input, so neither is guessed at.
    assert_eq!(
        EvmU256::try_from_be_slice(&[0xffu8; 16]).unwrap_err(),
        EvmU256Error::WrongByteLength { actual: 16 }
    );
    assert!(EvmU256::try_from_be_slice(&[]).is_err());
}

#[test]
fn the_word_hex_form_is_always_full_width() {
    assert_eq!(
        EvmU256::ZERO.to_word_hex(),
        "0x0000000000000000000000000000000000000000000000000000000000000000"
    );
    assert_eq!(
        EvmU256::ONE.to_word_hex(),
        "0x0000000000000000000000000000000000000000000000000000000000000001"
    );
    assert_eq!(
        EvmU256::MAX.to_word_hex(),
        "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    );
    for value in [0u128, 1, u64::MAX as u128, u128::MAX] {
        assert_eq!(EvmU256::from_u128(value).to_word_hex().len(), 66);
    }
}

#[test]
fn the_word_hex_form_round_trips() {
    for word in [
        EvmU256::ZERO,
        EvmU256::ONE,
        u128_max_word(),
        u128_max_plus_one_word(),
        EvmU256::MAX,
    ] {
        let text = word.to_word_hex();
        assert_eq!(EvmU256::from_word_hex(&text).unwrap(), word);
        assert_eq!(text, word.to_string());
    }
}

#[test]
fn from_word_hex_is_strict_about_width_and_prefix() {
    // Minimal QUANTITY spelling is NOT accepted here; that is
    // `crate::evm::quantity`'s job, and conflating the two is exactly the
    // mistake the two functions exist to keep apart.
    assert_eq!(
        EvmU256::from_word_hex("0x1").unwrap_err(),
        EvmU256Error::Hex(EvmHexError::WrongLength {
            expected: 64,
            actual: 1
        })
    );
    assert!(matches!(
        EvmU256::from_word_hex("0000000000000000000000000000000000000000000000000000000000000001"),
        Err(EvmU256Error::Hex(EvmHexError::MissingPrefix { .. }))
    ));
    assert!(matches!(
        EvmU256::from_word_hex(&format!("0x{}g", "0".repeat(63))),
        Err(EvmU256Error::Hex(EvmHexError::InvalidDigit { .. }))
    ));
    // Any digit case is fine: a word carries no checksum.
    assert_eq!(
        EvmU256::from_word_hex(
            "0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
        )
        .unwrap(),
        EvmU256::MAX
    );
}

#[test]
fn debug_is_readable() {
    assert_eq!(
        format!("{:?}", EvmU256::ONE),
        "EvmU256(0x0000000000000000000000000000000000000000000000000000000000000001)"
    );
}

#[test]
fn as_be_bytes_and_to_be_bytes_agree() {
    let word = EvmU256::from_u128(123_456_789);
    assert_eq!(*word.as_be_bytes(), word.to_be_bytes());
}

#[test]
fn hashing_treats_equal_words_as_one_key() {
    use std::collections::HashSet;

    let mut set = HashSet::new();
    set.insert(EvmU256::from_u128(1));
    set.insert(EvmU256::ONE);
    set.insert(EvmU256::from_word_hex(&EvmU256::ONE.to_word_hex()).unwrap());
    assert_eq!(set.len(), 1);
}

// ------------------------------------------------- saturating subtraction --

#[test]
fn saturating_sub_matches_u128_arithmetic_across_the_narrow_range() {
    for (a, b) in [
        (0u128, 0u128),
        (1, 0),
        (0, 1),
        (5, 5),
        (1_000_000, 999_999),
        (u128::from(u64::MAX), 1),
        (u128::MAX, u128::MAX),
        (u128::MAX, 1),
    ] {
        let expected = a.saturating_sub(b);
        assert_eq!(
            EvmU256::from_u128(a).saturating_sub(EvmU256::from_u128(b)),
            EvmU256::from_u128(expected),
            "{a} - {b}"
        );
    }
}

/// Borrow propagation across every byte lane, which a `u128`-range test
/// cannot reach: `2^128 - 1` requires a borrow to cross the 16-byte
/// boundary the low half ends at.
#[test]
fn saturating_sub_borrows_across_the_full_width() {
    let high = EvmU256::from_be_bytes({
        let mut b = [0u8; 32];
        b[15] = 1; // 2^128
        b
    });
    let expected = EvmU256::from_u128(u128::MAX); // 2^128 - 1
    assert_eq!(high.saturating_sub(EvmU256::ONE), expected);
}

/// The saturating case is the one that matters operationally: a bucket
/// total above a since-lowered limit is zero remaining, never a wrap to
/// an enormous number that would read as unlimited capacity.
#[test]
fn saturating_sub_floors_at_zero_rather_than_wrapping() {
    let small = EvmU256::from_u128(10);
    let large = EvmU256::from_u128(11);
    assert_eq!(small.saturating_sub(large), EvmU256::ZERO);

    let max = EvmU256::from_be_bytes([0xff; 32]);
    assert_eq!(EvmU256::ZERO.saturating_sub(max), EvmU256::ZERO);
    assert_eq!(max.saturating_sub(max), EvmU256::ZERO);
}

#[test]
fn saturating_sub_of_the_widest_values_is_exact() {
    let max = EvmU256::from_be_bytes([0xff; 32]);
    assert_eq!(max.saturating_sub(EvmU256::ZERO), max);
    let one_less = EvmU256::from_be_bytes({
        let mut b = [0xffu8; 32];
        b[31] = 0xfe;
        b
    });
    assert_eq!(max.saturating_sub(EvmU256::ONE), one_less);
}
