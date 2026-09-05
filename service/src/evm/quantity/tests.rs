use super::*;

use crate::evm::hex::EvmHexError;

// --- QUANTITY: the required values -----------------------------------

#[test]
fn zero_is_spelled_0x0() {
    // The specification's one carve-out from the minimal-form rule.
    assert_eq!(encode_quantity_u64(0), "0x0");
    assert_eq!(encode_quantity_u256(EvmU256::ZERO), "0x0");
    assert_eq!(parse_quantity_u64("0x0").unwrap(), 0);
    assert_eq!(parse_quantity_u256("0x0").unwrap(), EvmU256::ZERO);
}

#[test]
fn one_is_spelled_0x1() {
    assert_eq!(encode_quantity_u64(1), "0x1");
    assert_eq!(encode_quantity_u256(EvmU256::ONE), "0x1");
    assert_eq!(parse_quantity_u64("0x1").unwrap(), 1);
    assert_eq!(parse_quantity_u256("0x1").unwrap(), EvmU256::ONE);
}

#[test]
fn a_realistic_block_number_round_trips() {
    // 21,000,000 — an Ethereum mainnet block height, i.e. the shape of every
    // `blockNumber`, `logIndex` and `nonce` this bridge will read.
    let block_number = 21_000_000u64;
    let encoded = encode_quantity_u64(block_number);
    assert_eq!(encoded, "0x1406f40");
    assert_eq!(parse_quantity_u64(&encoded).unwrap(), block_number);
    assert_eq!(
        parse_quantity_u256(&encoded).unwrap().try_to_u64().unwrap(),
        block_number
    );
}

#[test]
fn a_large_uint256_round_trips_without_narrowing() {
    // 2^255: a value no u128 can hold, which must survive the QUANTITY
    // round trip intact rather than being narrowed or rejected.
    let mut bytes = [0u8; 32];
    bytes[0] = 0x80;
    let word = EvmU256::from_be_bytes(bytes);

    let encoded = encode_quantity_u256(word);
    assert_eq!(encoded, format!("0x8{}", "0".repeat(63)));
    assert_eq!(parse_quantity_u256(&encoded).unwrap(), word);
    // And it is genuinely too large for the narrow types.
    assert!(word.try_to_u128().is_err());
    assert!(parse_quantity_u64(&encoded).is_err());
}

#[test]
fn the_full_uint256_max_round_trips() {
    let encoded = encode_quantity_u256(EvmU256::MAX);
    assert_eq!(encoded, format!("0x{}", "f".repeat(64)));
    assert_eq!(encoded.len(), 66);
    assert_eq!(parse_quantity_u256(&encoded).unwrap(), EvmU256::MAX);
}

#[test]
fn a_realistic_erc20_amount_round_trips() {
    // 20,000 GLC at 18 decimals — the designed per-transfer limit, and the
    // value that proved a u64 is not enough (Phase A).
    let amount = 20_000_000_000_000_000_000_000u128;
    let word = EvmU256::from_u128(amount);
    let encoded = encode_quantity_u256(word);
    assert_eq!(parse_quantity_u256(&encoded).unwrap(), word);
    assert_eq!(
        parse_quantity_u256(&encoded)
            .unwrap()
            .try_to_u128()
            .unwrap(),
        amount
    );
    assert!(
        u64::try_from(amount).is_err(),
        "this is why u64 is not enough"
    );
}

// --- QUANTITY: malformed input ---------------------------------------

#[test]
fn rejects_a_missing_or_malformed_prefix() {
    for input in ["1", "1237", "x1", "0", ""] {
        assert!(
            matches!(
                parse_quantity_u64(input),
                Err(EvmQuantityError::Hex(EvmHexError::MissingPrefix { .. }))
            ),
            "{input:?} must be rejected for its prefix"
        );
    }
    // `0X` is a second spelling of the prefix and is refused with the rest.
    assert!(matches!(
        parse_quantity_u64("0X1"),
        Err(EvmQuantityError::Hex(EvmHexError::MissingPrefix { .. }))
    ));
}

#[test]
fn rejects_an_empty_quantity() {
    assert_eq!(
        parse_quantity_u64("0x").unwrap_err(),
        EvmQuantityError::EmptyQuantity
    );
    assert_eq!(
        parse_quantity_u256("0x").unwrap_err(),
        EvmQuantityError::EmptyQuantity
    );
    // ...whereas "0x" IS a valid, empty DATA value. The asymmetry is the
    // point of having two functions.
    assert_eq!(parse_data("0x").unwrap(), Vec::<u8>::new());
}

#[test]
fn rejects_negative_values() {
    for input in ["-1", "-0x1", "-0x0"] {
        assert_eq!(
            parse_quantity_u64(input).unwrap_err(),
            EvmQuantityError::Signed { found: '-' },
            "{input:?}"
        );
    }
    // A leading `+` is refused too: it is not a spelling any node emits.
    assert_eq!(
        parse_quantity_u256("+0x1").unwrap_err(),
        EvmQuantityError::Signed { found: '+' }
    );
    // A sign inside the digits is a plain non-hex digit.
    assert!(matches!(
        parse_quantity_u64("0x1-2"),
        Err(EvmQuantityError::Hex(EvmHexError::InvalidDigit { .. }))
    ));
}

#[test]
fn rejects_a_bad_leading_zero() {
    for input in ["0x00", "0x01", "0x0123", "0x000000"] {
        let err = parse_quantity_u64(input).unwrap_err();
        assert!(
            matches!(err, EvmQuantityError::LeadingZero { .. }),
            "{input:?} gave {err}"
        );
    }
    // The same rule at 256 bits, including a full-width DATA word being
    // mistakenly handed to the QUANTITY parser — the dangerous case.
    let full_width_word = format!("0x{}1", "0".repeat(63));
    assert!(matches!(
        parse_quantity_u256(&full_width_word),
        Err(EvmQuantityError::LeadingZero { .. })
    ));
}

#[test]
fn rejects_malformed_hex_digits() {
    assert_eq!(
        parse_quantity_u64("0x12g4").unwrap_err(),
        EvmQuantityError::Hex(EvmHexError::InvalidDigit {
            position: 2,
            found: 'g'
        })
    );
    for input in ["0x 1", "0x1 ", "0x1\n", "0x1.5", "0xdead beef"] {
        assert!(
            matches!(
                parse_quantity_u256(input),
                Err(EvmQuantityError::Hex(EvmHexError::InvalidDigit { .. }))
            ),
            "{input:?} must be rejected"
        );
    }
}

#[test]
fn accepts_any_digit_case() {
    assert_eq!(parse_quantity_u64("0xABCDEF").unwrap(), 0xabcdef);
    assert_eq!(parse_quantity_u64("0xabcdef").unwrap(), 0xabcdef);
    assert_eq!(parse_quantity_u64("0xAbCdEf").unwrap(), 0xabcdef);
    // ...but only ever emits lowercase, so a value has one spelling.
    assert_eq!(encode_quantity_u64(0xabcdef), "0xabcdef");
}

// --- QUANTITY: overflow ----------------------------------------------

#[test]
fn rejects_a_u64_quantity_that_is_out_of_range() {
    // 2^64 exactly: 17 minimal digits.
    let over = format!("0x1{}", "0".repeat(16));
    let err = parse_quantity_u64(&over).unwrap_err();
    assert_eq!(
        err,
        EvmQuantityError::Overflow {
            value: over.clone(),
            digits: 17,
            max_digits: 16,
            bits: 64
        }
    );
    assert!(err.to_string().contains("refusing to truncate"), "{err}");
    // u64::MAX itself, at exactly 16 digits, is fine.
    assert_eq!(
        parse_quantity_u64(&format!("0x{}", "f".repeat(16))).unwrap(),
        u64::MAX
    );
    // ...and the out-of-range value is still a perfectly good uint256.
    assert_eq!(
        parse_quantity_u256(&over).unwrap().try_to_u128().unwrap(),
        1u128 << 64
    );
}

#[test]
fn rejects_a_quantity_beyond_the_uint256_range() {
    // 65 minimal digits: larger than 2^256 - 1, which the EVM itself cannot
    // represent. Refused, not reduced modulo anything.
    let over = format!("0x1{}", "0".repeat(64));
    let err = parse_quantity_u256(&over).unwrap_err();
    assert_eq!(
        err,
        EvmQuantityError::Overflow {
            value: over,
            digits: 65,
            max_digits: 64,
            bits: 256
        }
    );
    // uint256 max itself, at exactly 64 digits, is accepted.
    assert_eq!(
        parse_quantity_u256(&format!("0x{}", "f".repeat(64))).unwrap(),
        EvmU256::MAX
    );
    // And a wildly long input does not panic or allocate unboundedly.
    assert!(parse_quantity_u256(&format!("0x{}", "f".repeat(10_000))).is_err());
}

#[test]
fn a_pathological_input_is_truncated_in_the_error() {
    let long = format!("0x1{}", "0".repeat(4096));
    let EvmQuantityError::Overflow { value, .. } = parse_quantity_u256(&long).unwrap_err() else {
        panic!("expected Overflow");
    };
    assert_eq!(value.chars().count(), 72);
}

// --- QUANTITY: round trips -------------------------------------------

#[test]
fn every_u64_quantity_round_trips() {
    let cases = [
        0u64,
        1,
        9,
        10,
        15,
        16,
        255,
        256,
        4663,
        46630,
        21_000_000,
        u32::MAX as u64,
        u64::MAX - 1,
        u64::MAX,
    ];
    for value in cases {
        let encoded = encode_quantity_u64(value);
        assert_eq!(parse_quantity_u64(&encoded).unwrap(), value, "{value}");
        // The encoder always produces the minimal form, so its own output is
        // accepted by the strict parser — the property that makes the
        // no-leading-zero rule safe to enforce.
        assert!(
            !encoded[2..].starts_with('0') || encoded == "0x0",
            "{encoded}"
        );
        // And the 256-bit parser agrees with the 64-bit one.
        assert_eq!(
            parse_quantity_u256(&encoded).unwrap(),
            EvmU256::from_u64(value)
        );
    }
}

#[test]
fn every_u256_quantity_round_trips() {
    let mut two_pow_255 = [0u8; 32];
    two_pow_255[0] = 0x80;
    let cases = [
        EvmU256::ZERO,
        EvmU256::ONE,
        EvmU256::from_u64(u64::MAX),
        EvmU256::from_u128(u128::MAX),
        EvmU256::from_be_bytes(two_pow_255),
        EvmU256::MAX,
    ];
    for word in cases {
        let encoded = encode_quantity_u256(word);
        assert_eq!(parse_quantity_u256(&encoded).unwrap(), word, "{word}");
    }
}

#[test]
fn the_two_encoders_agree_on_every_value_both_can_hold() {
    for value in [0u64, 1, 255, 4663, 21_000_000, u32::MAX as u64, u64::MAX] {
        assert_eq!(
            encode_quantity_u64(value),
            encode_quantity_u256(EvmU256::from_u64(value)),
            "{value}"
        );
    }
}

// --- DATA ------------------------------------------------------------

#[test]
fn data_keeps_leading_zeros_and_a_fixed_width() {
    assert_eq!(encode_data(&[]), "0x");
    assert_eq!(encode_data(&[0x00]), "0x00");
    assert_eq!(encode_data(&[0x00, 0x01]), "0x0001");
    assert_eq!(encode_data(&[0xff; 4]), "0xffffffff");
}

#[test]
fn data_and_quantity_are_not_interchangeable() {
    // "0x0" is a valid QUANTITY and an invalid DATA (odd digit count).
    assert_eq!(parse_quantity_u64("0x0").unwrap(), 0);
    assert_eq!(
        parse_data("0x0").unwrap_err(),
        EvmQuantityError::Hex(EvmHexError::OddLength(1))
    );

    // "0x00" is a valid DATA and an invalid QUANTITY (non-minimal).
    assert_eq!(parse_data("0x00").unwrap(), vec![0u8]);
    assert!(matches!(
        parse_quantity_u64("0x00"),
        Err(EvmQuantityError::LeadingZero { .. })
    ));
}

#[test]
fn data_round_trips_at_arbitrary_lengths() {
    for len in 0..40usize {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 13 % 256) as u8).collect();
        let encoded = encode_data(&bytes);
        assert_eq!(parse_data(&encoded).unwrap(), bytes, "length {len}");
    }
}

#[test]
fn data_exact_never_pads_or_truncates() {
    let word = format!("0x{}", "ab".repeat(32));
    assert_eq!(parse_data_exact::<32>(&word).unwrap(), [0xabu8; 32]);

    // One byte short of a word: an error, not a left-pad.
    let short = format!("0x{}", "ab".repeat(31));
    assert_eq!(
        parse_data_exact::<32>(&short).unwrap_err(),
        EvmQuantityError::Hex(EvmHexError::WrongLength {
            expected: 64,
            actual: 62
        })
    );
    // One byte long: an error, not a truncation.
    let long = format!("0x{}", "ab".repeat(33));
    assert!(matches!(
        parse_data_exact::<32>(&long),
        Err(EvmQuantityError::Hex(EvmHexError::WrongLength {
            expected: 64,
            actual: 66
        }))
    ));
    // A 20-byte address is not a 32-byte topic.
    assert!(parse_data_exact::<32>(&format!("0x{}", "ab".repeat(20))).is_err());
}

#[test]
fn data_rejects_an_odd_digit_count_and_bad_digits() {
    assert_eq!(
        parse_data("0xabc").unwrap_err(),
        EvmQuantityError::Hex(EvmHexError::OddLength(3))
    );
    assert!(matches!(
        parse_data("0xabcg"),
        Err(EvmQuantityError::Hex(EvmHexError::InvalidDigit { .. }))
    ));
    assert!(matches!(
        parse_data("abcd"),
        Err(EvmQuantityError::Hex(EvmHexError::MissingPrefix { .. }))
    ));
}
