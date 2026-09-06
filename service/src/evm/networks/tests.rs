use super::*;

#[test]
fn the_robinhood_chain_ids_are_the_documented_decimal_values() {
    assert_eq!(ROBINHOOD_MAINNET_CHAIN_ID_RAW, 4663);
    assert_eq!(ROBINHOOD_TESTNET_CHAIN_ID_RAW, 46630);
    assert_eq!(ROBINHOOD_MAINNET_CHAIN_ID.get(), 4663);
    assert_eq!(ROBINHOOD_TESTNET_CHAIN_ID.get(), 46630);
}

#[test]
fn the_two_networks_are_distinct() {
    assert_ne!(ROBINHOOD_MAINNET_CHAIN_ID, ROBINHOOD_TESTNET_CHAIN_ID);
    assert_ne!(RobinhoodNetwork::Mainnet, RobinhoodNetwork::Testnet);
    assert_ne!(
        RobinhoodNetwork::Mainnet.chain_id(),
        RobinhoodNetwork::Testnet.chain_id()
    );
}

#[test]
fn the_decimal_spelling_is_what_is_meant_not_the_hex_one() {
    // 0x4663 is 18019 and 0x46630 is 288304: both are real integers and
    // neither is a Robinhood chain id. This pins down that the constants are
    // decimal, which is the one mistake that would silently point the bridge
    // at the wrong network.
    assert_eq!(u64::from_str_radix("4663", 16).unwrap(), 18019);
    assert_ne!(ROBINHOOD_MAINNET_CHAIN_ID_RAW, 18019);
    assert_eq!(u64::from_str_radix("46630", 16).unwrap(), 288_304);
    assert_ne!(ROBINHOOD_TESTNET_CHAIN_ID_RAW, 288_304);
}

#[test]
fn testnet_is_not_derived_from_mainnet_by_arithmetic() {
    // They happen to differ by a factor of ten today. Nothing may rely on
    // that; this test exists so that a "clever" refactor to
    // `MAINNET * 10` is a visible change rather than an invisible one.
    assert_eq!(
        ROBINHOOD_TESTNET_CHAIN_ID_RAW, 46630,
        "the testnet id is a written-down constant, not a computed one"
    );
}

#[test]
fn round_trips_through_from_chain_id() {
    for network in [RobinhoodNetwork::Mainnet, RobinhoodNetwork::Testnet] {
        assert_eq!(
            RobinhoodNetwork::from_chain_id(network.chain_id()),
            Some(network)
        );
    }
}

#[test]
fn an_unknown_chain_id_maps_to_no_network() {
    for raw in [1u64, 11_155_111, 18_019, 288_304, 4664, 46_631, u64::MAX] {
        let chain_id = EvmChainId::new(raw).unwrap();
        assert_eq!(
            RobinhoodNetwork::from_chain_id(chain_id),
            None,
            "chain id {raw} must not be mistaken for a Robinhood network"
        );
    }
}

#[test]
fn names_are_stable_and_lowercase() {
    assert_eq!(RobinhoodNetwork::Mainnet.as_str(), "mainnet");
    assert_eq!(RobinhoodNetwork::Testnet.as_str(), "testnet");
    assert_eq!(RobinhoodNetwork::Mainnet.to_string(), "mainnet");
    assert_eq!(RobinhoodNetwork::Testnet.to_string(), "testnet");
}

#[test]
fn the_chain_ids_agree_with_both_textual_parsers() {
    assert_eq!(
        "4663".parse::<EvmChainId>().unwrap(),
        ROBINHOOD_MAINNET_CHAIN_ID
    );
    assert_eq!(
        "46630".parse::<EvmChainId>().unwrap(),
        ROBINHOOD_TESTNET_CHAIN_ID
    );
    assert_eq!(
        EvmChainId::from_quantity_hex("0x1237").unwrap(),
        ROBINHOOD_MAINNET_CHAIN_ID,
        "0x1237 is 4663"
    );
    assert_eq!(
        EvmChainId::from_quantity_hex("0xb626").unwrap(),
        ROBINHOOD_TESTNET_CHAIN_ID,
        "0xb626 is 46630"
    );
    assert_eq!(ROBINHOOD_MAINNET_CHAIN_ID.to_quantity_hex(), "0x1237");
    assert_eq!(ROBINHOOD_TESTNET_CHAIN_ID.to_quantity_hex(), "0xb626");
}
