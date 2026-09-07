//! Configuration validation: what a `[robinhood.indexer]` section must
//! say, and which values are refused rather than defaulted.

use super::*;
use crate::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID;

fn address(byte: u8) -> EvmAddress {
    EvmAddress::from_bytes([byte; 20])
}

fn valid() -> Result<RobinhoodIndexerConfig, RobinhoodConfigError> {
    RobinhoodIndexerConfig::new(
        "https://rpc.example.invalid".to_string(),
        ROBINHOOD_TESTNET_CHAIN_ID,
        address(0x11),
        address(0x22),
        1_000,
        12,
        5_000,
        10_000,
        2_000,
    )
}

#[test]
fn accepts_a_fully_specified_section() {
    let config = valid().expect("valid config");
    assert_eq!(config.chain_id, ROBINHOOD_TESTNET_CHAIN_ID);
    assert_eq!(config.start_block, 1_000);
    assert_eq!(config.confirmation_depth, 12);
    assert_eq!(config.max_log_block_range, 2_000);
}

#[test]
fn refuses_an_empty_or_non_http_endpoint() {
    let build = |url: &str| {
        RobinhoodIndexerConfig::new(
            url.to_string(),
            ROBINHOOD_TESTNET_CHAIN_ID,
            address(0x11),
            address(0x22),
            1,
            1,
            1,
            1,
            1,
        )
    };
    assert_eq!(build("   "), Err(RobinhoodConfigError::EmptyRpcUrl));
    // A websocket endpoint is refused rather than silently ignored: this
    // client never subscribes, so accepting one would promise a live feed
    // it does not provide.
    assert_eq!(
        build("wss://rpc.example.invalid"),
        Err(RobinhoodConfigError::UnsupportedRpcScheme {
            url: "wss://rpc.example.invalid".to_string()
        })
    );
}

#[test]
fn refuses_zero_addresses_and_a_bridge_that_is_its_own_token() {
    let build = |bridge: EvmAddress, token: EvmAddress| {
        RobinhoodIndexerConfig::new(
            "http://rpc.example.invalid".to_string(),
            ROBINHOOD_TESTNET_CHAIN_ID,
            bridge,
            token,
            1,
            1,
            1,
            1,
            1,
        )
    };
    assert_eq!(
        build(EvmAddress::ZERO, address(0x22)),
        Err(RobinhoodConfigError::ZeroBridgeContract)
    );
    assert_eq!(
        build(address(0x11), EvmAddress::ZERO),
        Err(RobinhoodConfigError::ZeroExpectedToken)
    );
    assert!(matches!(
        build(address(0x11), address(0x11)),
        Err(RobinhoodConfigError::BridgeIsToken { .. })
    ));
}

/// A depth of zero would call a deposit irreversible before it was in any
/// block — the one config mistake that silently accepts reversible money.
#[test]
fn refuses_a_zero_confirmation_depth() {
    let result = RobinhoodIndexerConfig::new(
        "http://rpc.example.invalid".to_string(),
        ROBINHOOD_TESTNET_CHAIN_ID,
        address(0x11),
        address(0x22),
        1,
        0,
        1,
        1,
        1,
    );
    assert_eq!(result, Err(RobinhoodConfigError::ZeroConfirmationDepth));
}

#[test]
fn refuses_zero_intervals_timeouts_and_ranges() {
    let build = |poll: u64, timeout: u64, range: u64| {
        RobinhoodIndexerConfig::new(
            "http://rpc.example.invalid".to_string(),
            ROBINHOOD_TESTNET_CHAIN_ID,
            address(0x11),
            address(0x22),
            1,
            1,
            poll,
            timeout,
            range,
        )
    };
    assert_eq!(build(0, 1, 1), Err(RobinhoodConfigError::ZeroPollInterval));
    assert_eq!(
        build(1, 0, 1),
        Err(RobinhoodConfigError::ZeroRequestTimeout)
    );
    assert_eq!(
        build(1, 1, 0),
        Err(RobinhoodConfigError::ZeroMaxLogBlockRange)
    );
}

/// `start_block` of 0 is legal — it means "this deployment really does
/// want the whole chain" — and is deliberately not defaulted to.
#[test]
fn a_zero_start_block_is_a_legitimate_choice_not_a_default() {
    let config = RobinhoodIndexerConfig::new(
        "http://rpc.example.invalid".to_string(),
        ROBINHOOD_TESTNET_CHAIN_ID,
        address(0x11),
        address(0x22),
        0,
        1,
        1,
        1,
        1,
    )
    .expect("start_block 0 is valid");
    assert_eq!(config.start_block, 0);
}
