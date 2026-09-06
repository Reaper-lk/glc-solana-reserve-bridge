use super::*;

use crate::evm::hex::EvmHexError;

const ZERO_TEXT: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const FF_TEXT: &str = "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
/// A real Ethereum mainnet transaction hash shape — mixed case as a block
/// explorer or an `eth_getTransactionByHash` reply might spell it.
const MIXED_CASE: &str = "0xAbC0000000000000000000000000000000000000000000000000000000000DeF";
const MIXED_CASE_LOWER: &str = "0xabc0000000000000000000000000000000000000000000000000000000000def";

// --- Boundary values -------------------------------------------------

#[test]
fn parses_the_zero_hash() {
    let tx: EvmTxHash = ZERO_TEXT.parse().unwrap();
    assert_eq!(tx, EvmTxHash::ZERO);
    assert!(tx.is_zero());
    assert_eq!(tx.to_bytes(), [0u8; 32]);
    assert_eq!(tx.to_string(), ZERO_TEXT);

    let block: EvmBlockHash = ZERO_TEXT.parse().unwrap();
    assert_eq!(block, EvmBlockHash::ZERO);
    assert!(block.is_zero());
    assert_eq!(block.to_string(), ZERO_TEXT);
}

#[test]
fn parses_the_all_ff_hash() {
    let tx: EvmTxHash = FF_TEXT.parse().unwrap();
    assert_eq!(tx.to_bytes(), [0xffu8; 32]);
    assert!(!tx.is_zero());
    assert_eq!(tx.to_string(), FF_TEXT);

    let block: EvmBlockHash = FF_TEXT.parse().unwrap();
    assert_eq!(block.to_bytes(), [0xffu8; 32]);
    assert_eq!(block.to_string(), FF_TEXT);
}

// --- Digit case ------------------------------------------------------

#[test]
fn accepts_any_digit_case_and_normalises_to_lowercase() {
    // Hashes have no checksum scheme, so case is not information — unlike
    // an address, where mixed case is verified.
    let from_mixed: EvmTxHash = MIXED_CASE.parse().unwrap();
    let from_lower: EvmTxHash = MIXED_CASE_LOWER.parse().unwrap();
    let from_upper: EvmTxHash = MIXED_CASE
        .to_uppercase()
        .replace("0X", "0x")
        .parse()
        .unwrap();

    assert_eq!(from_mixed, from_lower);
    assert_eq!(from_mixed, from_upper);
    assert_eq!(from_mixed.to_string(), MIXED_CASE_LOWER);
}

// --- Malformed shapes ------------------------------------------------

#[test]
fn rejects_a_missing_or_uppercase_prefix() {
    assert!(matches!(
        ZERO_TEXT[2..].parse::<EvmTxHash>(),
        Err(EvmHashError::Hex {
            source: EvmHexError::MissingPrefix { .. },
            ..
        })
    ));
    assert!(matches!(
        format!("0X{}", &ZERO_TEXT[2..]).parse::<EvmBlockHash>(),
        Err(EvmHashError::Hex {
            source: EvmHexError::MissingPrefix { .. },
            ..
        })
    ));
}

#[test]
fn rejects_a_too_short_hash_rather_than_padding_it() {
    // 62 digits: a 31-byte value. Left- or right-padding it would produce a
    // valid-looking hash that identifies nothing.
    let short = format!("0x{}", "0".repeat(62));
    assert_eq!(
        short.parse::<EvmTxHash>().unwrap_err(),
        EvmHashError::Hex {
            kind: "transaction",
            source: EvmHexError::WrongLength {
                expected: 64,
                actual: 62
            }
        }
    );
}

#[test]
fn rejects_a_too_long_hash_rather_than_truncating_it() {
    let long = format!("0x{}", "0".repeat(66));
    assert_eq!(
        long.parse::<EvmBlockHash>().unwrap_err(),
        EvmHashError::Hex {
            kind: "block",
            source: EvmHexError::WrongLength {
                expected: 64,
                actual: 66
            }
        }
    );
}

#[test]
fn rejects_an_odd_digit_count() {
    let odd = format!("0x{}", "a".repeat(63));
    assert_eq!(
        odd.parse::<EvmTxHash>().unwrap_err(),
        EvmHashError::Hex {
            kind: "transaction",
            source: EvmHexError::WrongLength {
                expected: 64,
                actual: 63
            }
        }
    );
}

#[test]
fn rejects_a_20_byte_address_shaped_value() {
    // The exact confusion the fixed width exists to prevent: an address
    // must never be accepted where a hash is expected.
    let address_shaped = "0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed";
    assert!(matches!(
        address_shaped.parse::<EvmTxHash>(),
        Err(EvmHashError::Hex {
            source: EvmHexError::WrongLength {
                expected: 64,
                actual: 40
            },
            ..
        })
    ));
}

#[test]
fn rejects_malformed_hex() {
    let bad = format!("0x{}g", "a".repeat(63));
    assert_eq!(
        bad.parse::<EvmTxHash>().unwrap_err(),
        EvmHashError::Hex {
            kind: "transaction",
            source: EvmHexError::InvalidDigit {
                position: 63,
                found: 'g'
            }
        }
    );
    // Whitespace and a decimal-looking value are both rejected.
    assert!(format!("0x{} ", "a".repeat(63))
        .parse::<EvmTxHash>()
        .is_err());
    assert!("0x".parse::<EvmTxHash>().is_err());
    assert!("".parse::<EvmTxHash>().is_err());
}

#[test]
fn error_messages_name_which_kind_of_hash_failed() {
    let short = format!("0x{}", "0".repeat(62));
    let tx_message = short.parse::<EvmTxHash>().unwrap_err().to_string();
    let block_message = short.parse::<EvmBlockHash>().unwrap_err().to_string();
    assert!(tx_message.contains("transaction"), "{tx_message}");
    assert!(block_message.contains("block"), "{block_message}");
    assert_ne!(tx_message, block_message);
}

// --- Semantic separation ---------------------------------------------

#[test]
fn tx_and_block_hashes_stay_semantically_distinct() {
    // Identical bytes, and the two values are still different things. This
    // test documents the property; the compiler enforces it — there is no
    // `From`, `Into`, `PartialEq` or accessor bridging the two types, so
    // `let _: EvmTxHash = block_hash;` does not compile.
    let bytes = [0x7fu8; 32];
    let tx = EvmTxHash::from_bytes(bytes);
    let block = EvmBlockHash::from_bytes(bytes);

    assert_eq!(tx.to_bytes(), block.to_bytes());
    assert_eq!(tx.to_string(), block.to_string());

    // The one legitimate way across is explicit and visible in review.
    assert_eq!(EvmTxHash::from_bytes(block.to_bytes()), tx);

    assert_eq!(EvmTxHash::KIND, "transaction");
    assert_eq!(EvmBlockHash::KIND, "block");
}

#[test]
fn debug_names_the_concrete_type() {
    let bytes = [0u8; 32];
    assert_eq!(
        format!("{:?}", EvmTxHash::from_bytes(bytes)),
        format!("EvmTxHash({ZERO_TEXT})")
    );
    assert_eq!(
        format!("{:?}", EvmBlockHash::from_bytes(bytes)),
        format!("EvmBlockHash({ZERO_TEXT})")
    );
}

// --- Bytes <-> type --------------------------------------------------

#[test]
fn try_from_slice_requires_exactly_thirty_two_bytes() {
    assert_eq!(
        EvmTxHash::try_from_slice(&[0xabu8; 32]).unwrap(),
        EvmTxHash::from_bytes([0xabu8; 32])
    );
    assert_eq!(
        EvmTxHash::try_from_slice(&[0xabu8; 31]).unwrap_err(),
        EvmHashError::WrongByteLength {
            kind: "transaction",
            actual: 31
        }
    );
    assert_eq!(
        EvmBlockHash::try_from_slice(&[0xabu8; 33]).unwrap_err(),
        EvmHashError::WrongByteLength {
            kind: "block",
            actual: 33
        }
    );
    // A 20-byte address must not become a hash.
    assert_eq!(
        EvmBlockHash::try_from_slice(&[0xabu8; 20]).unwrap_err(),
        EvmHashError::WrongByteLength {
            kind: "block",
            actual: 20
        }
    );
    assert!(EvmTxHash::try_from_slice(&[]).is_err());
}

#[test]
fn as_bytes_and_to_bytes_agree() {
    let tx: EvmTxHash = MIXED_CASE.parse().unwrap();
    assert_eq!(*tx.as_bytes(), tx.to_bytes());
}

// --- Round trips -----------------------------------------------------

#[test]
fn round_trips_through_display() {
    for text in [ZERO_TEXT, FF_TEXT, MIXED_CASE_LOWER] {
        let tx: EvmTxHash = text.parse().unwrap();
        assert_eq!(tx.to_string(), text);
        assert_eq!(tx.to_string().parse::<EvmTxHash>().unwrap(), tx);

        let block: EvmBlockHash = text.parse().unwrap();
        assert_eq!(block.to_string(), text);
        assert_eq!(block.to_string().parse::<EvmBlockHash>().unwrap(), block);
    }
}

#[test]
fn round_trips_over_a_range_of_byte_patterns() {
    for seed in 0u16..=255 {
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (seed as usize).wrapping_mul(index + 1) as u8;
        }
        let tx = EvmTxHash::from_bytes(bytes);
        assert_eq!(tx.to_string().parse::<EvmTxHash>().unwrap(), tx);
        assert_eq!(tx.to_string().len(), 66);
    }
}

#[test]
fn ordering_and_hashing_are_over_the_bytes() {
    use std::collections::HashSet;

    assert!(EvmTxHash::ZERO < EvmTxHash::from_bytes([0xffu8; 32]));
    assert!(EvmBlockHash::ZERO < EvmBlockHash::from_bytes([0x01u8; 32]));

    let mut set = HashSet::new();
    set.insert(MIXED_CASE.parse::<EvmTxHash>().unwrap());
    set.insert(MIXED_CASE_LOWER.parse::<EvmTxHash>().unwrap());
    assert_eq!(set.len(), 1, "case must not create two distinct hashes");
}
