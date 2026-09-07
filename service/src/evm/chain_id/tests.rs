use super::*;

use crate::evm::hex::EvmHexError;
use crate::evm::networks::{ROBINHOOD_MAINNET_CHAIN_ID_RAW, ROBINHOOD_TESTNET_CHAIN_ID_RAW};

// --- The chain ids this bridge is being built for --------------------

#[test]
fn accepts_robinhood_mainnet_4663() {
    let id = EvmChainId::new(4663).unwrap();
    assert_eq!(id.get(), 4663);
    assert_eq!(id.get(), ROBINHOOD_MAINNET_CHAIN_ID_RAW);
    assert_eq!(id.to_string(), "4663");
    assert_eq!("4663".parse::<EvmChainId>().unwrap(), id);
}

#[test]
fn accepts_robinhood_testnet_46630() {
    let id = EvmChainId::new(46630).unwrap();
    assert_eq!(id.get(), 46630);
    assert_eq!(id.get(), ROBINHOOD_TESTNET_CHAIN_ID_RAW);
    assert_eq!(id.to_string(), "46630");
    assert_eq!("46630".parse::<EvmChainId>().unwrap(), id);
}

#[test]
fn mainnet_and_testnet_are_never_equal() {
    assert_ne!(
        EvmChainId::new(4663).unwrap(),
        EvmChainId::new(46630).unwrap()
    );
}

// --- Zero, explicitly ------------------------------------------------

#[test]
fn zero_is_rejected_everywhere() {
    // The decision recorded on `EvmChainId::new`: absence is
    // `Option<EvmChainId>`, never chain id 0.
    assert_eq!(EvmChainId::new(0).unwrap_err(), EvmChainIdError::Zero);
    assert_eq!(
        "0".parse::<EvmChainId>().unwrap_err(),
        EvmChainIdError::Zero
    );
    assert_eq!(
        EvmChainId::from_quantity_hex("0x0").unwrap_err(),
        EvmChainIdError::Zero
    );
    let message = EvmChainIdError::Zero.to_string();
    assert!(message.contains("replay protection"), "{message}");
}

#[test]
fn the_representation_makes_zero_unconstructible() {
    // `EvmChainId` holds a `NonZeroU64`, so there is no `EvmChainId(0)` to
    // write even inside this module — the invariant is structural, not a
    // check that a future constructor could forget.
    assert_eq!(std::mem::size_of::<EvmChainId>(), 8);
    assert_eq!(
        std::mem::size_of::<Option<EvmChainId>>(),
        8,
        "the niche makes Option<EvmChainId> free, which is the point of spelling absence that way"
    );
}

// --- Range -----------------------------------------------------------

#[test]
fn accepts_one_and_the_maximum_representable_value() {
    assert_eq!(EvmChainId::new(1).unwrap().get(), 1);
    let max = EvmChainId::new(u64::MAX).unwrap();
    assert_eq!(max.get(), u64::MAX);
    assert_eq!(max.to_string(), "18446744073709551615");
    assert_eq!(max.to_string().parse::<EvmChainId>().unwrap(), max);
}

#[test]
fn rejects_a_value_above_the_u64_range() {
    // 2^64 exactly, one past the representable maximum.
    let err = "18446744073709551616".parse::<EvmChainId>().unwrap_err();
    assert!(matches!(err, EvmChainIdError::NotDecimal { .. }));
    assert!(err.to_string().contains("64-bit"), "{err}");
}

#[test]
fn accepts_the_chain_ids_of_real_networks() {
    // A sanity spread: Ethereum mainnet, Optimism, Polygon, Base, Sepolia,
    // and a 10-digit id — none of which needs anything wider than a u64.
    for raw in [1u64, 10, 137, 8453, 11_155_111, 1_313_161_554] {
        assert_eq!(EvmChainId::new(raw).unwrap().get(), raw);
    }
}

// --- Decimal parsing is strict ---------------------------------------

#[test]
fn rejects_a_hex_spelling_with_a_message_naming_the_other_parser() {
    let err = "0x1237".parse::<EvmChainId>().unwrap_err();
    let message = err.to_string();
    assert!(message.contains("from_quantity_hex"), "{message}");
    assert!(matches!(err, EvmChainIdError::NotDecimal { .. }));
    // Uppercase prefix too.
    assert!(matches!(
        "0X1237".parse::<EvmChainId>(),
        Err(EvmChainIdError::NotDecimal { .. })
    ));
}

#[test]
fn a_hex_chain_id_read_as_decimal_would_be_a_different_network() {
    // The whole reason the two spellings have separate entry points.
    assert_eq!(
        EvmChainId::from_quantity_hex("0x4663").unwrap().get(),
        18019
    );
    assert_eq!("4663".parse::<EvmChainId>().unwrap().get(), 4663);
    assert_ne!(
        EvmChainId::from_quantity_hex("0x4663").unwrap(),
        "4663".parse::<EvmChainId>().unwrap()
    );
}

#[test]
fn rejects_a_sign_whitespace_and_separators() {
    for input in ["-1", "+1", " 1", "1 ", "1\n", "4_663", "4,663", "4663 "] {
        assert!(
            matches!(
                input.parse::<EvmChainId>(),
                Err(EvmChainIdError::NotDecimal { .. })
            ),
            "{input:?} must be rejected"
        );
    }
}

#[test]
fn rejects_an_empty_string() {
    let err = "".parse::<EvmChainId>().unwrap_err();
    assert!(err.to_string().contains("empty"), "{err}");
}

#[test]
fn rejects_a_leading_zero_but_not_the_bare_zero_spelling() {
    let err = "04663".parse::<EvmChainId>().unwrap_err();
    assert!(err.to_string().contains("leading zero"), "{err}");
    assert!(matches!(
        "0000".parse::<EvmChainId>(),
        Err(EvmChainIdError::NotDecimal { .. })
    ));
    // A bare "0" is well-formed decimal and fails for being zero, not for
    // its spelling — a different diagnosis for a different problem.
    assert_eq!(
        "0".parse::<EvmChainId>().unwrap_err(),
        EvmChainIdError::Zero
    );
}

#[test]
fn a_pathological_input_is_truncated_in_the_error() {
    let long = "9".repeat(4096);
    let EvmChainIdError::NotDecimal { value, .. } = long.parse::<EvmChainId>().unwrap_err() else {
        panic!("expected NotDecimal");
    };
    assert_eq!(value.chars().count(), 32);
}

// --- Hex quantity parsing --------------------------------------------

#[test]
fn parses_and_emits_the_json_rpc_quantity_spelling() {
    let mainnet = EvmChainId::new(4663).unwrap();
    assert_eq!(mainnet.to_quantity_hex(), "0x1237");
    assert_eq!(EvmChainId::from_quantity_hex("0x1237").unwrap(), mainnet);

    let one = EvmChainId::new(1).unwrap();
    assert_eq!(one.to_quantity_hex(), "0x1");
    assert_eq!(EvmChainId::from_quantity_hex("0x1").unwrap(), one);
}

#[test]
fn the_quantity_parser_inherits_the_quantity_rules() {
    // Minimal form only, prefix mandatory, no sign — the rules live in
    // `crate::evm::quantity` and are not re-implemented here.
    assert!(matches!(
        EvmChainId::from_quantity_hex("0x01237"),
        Err(EvmChainIdError::Quantity(
            EvmQuantityError::LeadingZero { .. }
        ))
    ));
    assert!(matches!(
        EvmChainId::from_quantity_hex("1237"),
        Err(EvmChainIdError::Quantity(EvmQuantityError::Hex(
            EvmHexError::MissingPrefix { .. }
        )))
    ));
    assert!(matches!(
        EvmChainId::from_quantity_hex("0x"),
        Err(EvmChainIdError::Quantity(EvmQuantityError::EmptyQuantity))
    ));
    assert!(matches!(
        EvmChainId::from_quantity_hex("-0x1"),
        Err(EvmChainIdError::Quantity(EvmQuantityError::Signed { .. }))
    ));
    assert!(matches!(
        EvmChainId::from_quantity_hex("0xzz"),
        Err(EvmChainIdError::Quantity(EvmQuantityError::Hex(
            EvmHexError::InvalidDigit { .. }
        )))
    ));
}

#[test]
fn both_spellings_round_trip() {
    for raw in [1u64, 4663, 46630, 137, u64::MAX] {
        let id = EvmChainId::new(raw).unwrap();
        assert_eq!(id.to_string().parse::<EvmChainId>().unwrap(), id);
        assert_eq!(
            EvmChainId::from_quantity_hex(&id.to_quantity_hex()).unwrap(),
            id
        );
    }
}

// --- EIP-712 encoding form -------------------------------------------

#[test]
fn to_u256_right_aligns_the_chain_id_in_a_word() {
    let id = EvmChainId::new(4663).unwrap();
    let word = id.to_u256();
    assert_eq!(word.try_to_u64().unwrap(), 4663);
    // 4663 == 0x1237, so the last two bytes are 0x12 0x37 and the rest are
    // zero: the ABI's left-padded `uint256`.
    let bytes = word.to_be_bytes();
    assert_eq!(bytes[30], 0x12);
    assert_eq!(bytes[31], 0x37);
    assert_eq!(bytes[..30], [0u8; 30]);
}

// --- Ordering --------------------------------------------------------

#[test]
fn ordering_is_numeric() {
    let mut ids: Vec<EvmChainId> = [46630u64, 1, 4663, u64::MAX]
        .into_iter()
        .map(|raw| EvmChainId::new(raw).unwrap())
        .collect();
    ids.sort();
    assert_eq!(
        ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
        vec![1, 4663, 46630, u64::MAX]
    );
}
