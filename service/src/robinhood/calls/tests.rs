//! Selector and calldata tests.
//!
//! Every function signature this service calls is a wire contract with
//! deployed bytecode, so each selector is pinned against an independently
//! computed keccak of the literal signature string, and each calldata
//! layout is asserted word by word.

use super::*;
use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::keccak256;
use crate::robinhood::auth::{ProtocolChainPair, ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE};

fn addr(byte: u8) -> EvmAddress {
    EvmAddress::from_bytes([byte; 20])
}

fn selector_of(sig: &str) -> [u8; 4] {
    let h = keccak256(sig.as_bytes());
    [h[0], h[1], h[2], h[3]]
}

#[test]
fn every_signature_constant_is_the_canonical_solidity_spelling() {
    // A signature string with a space, a parameter name, or a struct
    // spelled by NAME rather than as its tuple selects a different
    // function. These are the exact strings, asserted against the field
    // lists in `contracts/src/GlcRobinhoodBridge.sol`.
    assert_eq!(
        SIG_EXECUTE_PAYOUT,
        "executePayout((uint8,bytes32,address,uint256,uint64,uint64),bytes[])"
    );
    assert_eq!(
        SIG_EXECUTE_REFUND,
        "executeRefund((bytes32,uint256,address,uint256,uint64,uint64),bytes[])"
    );
    assert_eq!(
        SIG_EXECUTE_SETTLEMENT,
        "executeSettlement((bytes32,uint256,uint64,uint64),bytes[])"
    );
    for sig in [
        SIG_EXECUTE_PAYOUT,
        SIG_EXECUTE_REFUND,
        SIG_EXECUTE_SETTLEMENT,
        SIG_TOKEN,
        SIG_BRIDGE_PROTOCOL_ID,
        SIG_SIGNER_EPOCH,
        SIG_ROUTE_ENABLED,
        SIG_IS_ROUTE_LIVE,
        SIG_ROUTE_CHAINS,
        SIG_LIMITS,
        SIG_INBOUND_WINDOW,
        SIG_OUTBOUND_WINDOW,
        SIG_DEPOSITS_PAUSED,
        SIG_PAYOUTS_PAUSED,
        SIG_MIGRATED,
        SIG_OBLIGATION,
        SIG_OBLIGATION_STATUS,
        SIG_OBLIGATION_COUNT,
        SIG_REQUEST_EXECUTED,
        SIG_ENCUMBERED_RESERVE,
        SIG_SIGNERS,
        SIG_DOMAIN_SEPARATOR,
        SIG_ERC20_BALANCE_OF,
        SIG_ERC20_DECIMALS,
    ] {
        assert!(!sig.contains(' '), "{sig:?} must have no spaces");
        assert!(
            sig.ends_with(')'),
            "{sig:?} must end with the argument list"
        );
    }
}

/// The cross-language selector check.
///
/// `contracts/test/GoldenDigests.t.sol` asserts the DEPLOYED CONTRACT's
/// own `.selector` for each of these equals the fixture's value. This
/// asserts the string this crate hashes produces the same four bytes. A
/// signature retyped wrongly on either side therefore fails on that side,
/// against a file neither side generates.
#[test]
fn every_selector_matches_the_cross_language_golden_fixture() {
    for (key, sig) in [
        ("executePayout", SIG_EXECUTE_PAYOUT),
        ("executeRefund", SIG_EXECUTE_REFUND),
        ("executeSettlement", SIG_EXECUTE_SETTLEMENT),
        ("token", SIG_TOKEN),
        ("bridgeProtocolId", SIG_BRIDGE_PROTOCOL_ID),
        ("signerEpoch", SIG_SIGNER_EPOCH),
        ("routeEnabled", SIG_ROUTE_ENABLED),
        ("isRouteLive", SIG_IS_ROUTE_LIVE),
        ("routeChains", SIG_ROUTE_CHAINS),
        ("depositsPaused", SIG_DEPOSITS_PAUSED),
        ("payoutsPaused", SIG_PAYOUTS_PAUSED),
        ("migrated", SIG_MIGRATED),
        ("obligation", SIG_OBLIGATION),
        ("obligationCount", SIG_OBLIGATION_COUNT),
        ("requestExecuted", SIG_REQUEST_EXECUTED),
        ("encumberedReserve", SIG_ENCUMBERED_RESERVE),
        ("signers", SIG_SIGNERS),
        ("domainSeparator", SIG_DOMAIN_SEPARATOR),
        ("migrationCommitted", SIG_MIGRATION_COMMITTED),
        ("migrationSuccessor", SIG_MIGRATION_SUCCESSOR),
        ("migrationFinalizableAt", SIG_MIGRATION_FINALIZABLE_AT),
        (
            "outstandingRefundableCount",
            SIG_OUTSTANDING_REFUNDABLE_COUNT,
        ),
        (
            "outstandingRefundablePrincipal",
            SIG_OUTSTANDING_REFUNDABLE_PRINCIPAL,
        ),
        (
            "commitMigration",
            crate::robinhood::governance::SIG_COMMIT_MIGRATION,
        ),
        (
            "finalizeMigration",
            crate::robinhood::governance::SIG_FINALIZE_MIGRATION,
        ),
    ] {
        // The fixture stores each selector as Solidity's
        // `bytes32(bytes4)` — left-aligned, right-padded with zeros —
        // because that is the form a Solidity test can compare against
        // `assertEq(bytes32, bytes32)`.
        let expected = golden(&format!("selectors.{key}"));
        let actual = format!(
            "0x{}{}",
            selector_of(sig)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "0".repeat(56)
        );
        assert_eq!(actual, expected, "selector for {sig}");
    }
}

#[test]
fn the_protocol_id_matches_the_cross_language_golden_fixture() {
    let actual = format!(
        "0x{}",
        bridge_protocol_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    assert_eq!(actual, golden("bridgeProtocolId"));
}

/// One value from the shared cross-language fixture, by dotted path. See
/// `crate::robinhood::auth::tests` for why the file is `include_str!`d
/// rather than read at run time.
fn golden(path: &str) -> String {
    const FIXTURE: &str = include_str!("../../../../contracts/test/fixtures/eip712-golden.json");
    let root: serde_json::Value =
        serde_json::from_str(FIXTURE).expect("the golden fixture must be valid JSON");
    let mut node = &root;
    for segment in path.split('.') {
        node = node
            .get(segment)
            .unwrap_or_else(|| panic!("the golden fixture has no key {path:?}"));
    }
    match node {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[test]
fn the_erc20_selectors_match_their_universally_published_values() {
    // Independent third-party check on the selector machinery itself.
    let hex = |b: [u8; 4]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert_eq!(hex(selector_of(SIG_ERC20_BALANCE_OF)), "70a08231");
    assert_eq!(hex(selector_of(SIG_ERC20_DECIMALS)), "313ce567");
}

#[test]
fn every_selector_this_service_uses_is_distinct() {
    // A collision would mean two different reads returning each other's
    // answers — silently, since both would decode.
    let sigs = [
        SIG_EXECUTE_PAYOUT,
        SIG_EXECUTE_REFUND,
        SIG_EXECUTE_SETTLEMENT,
        SIG_TOKEN,
        SIG_BRIDGE_PROTOCOL_ID,
        SIG_SIGNER_EPOCH,
        SIG_ROUTE_ENABLED,
        SIG_IS_ROUTE_LIVE,
        SIG_ROUTE_CHAINS,
        SIG_DEPOSITS_PAUSED,
        SIG_PAYOUTS_PAUSED,
        SIG_MIGRATED,
        SIG_OBLIGATION,
        SIG_OBLIGATION_COUNT,
        SIG_REQUEST_EXECUTED,
        SIG_ENCUMBERED_RESERVE,
        SIG_SIGNERS,
        SIG_DOMAIN_SEPARATOR,
    ];
    let mut seen = std::collections::HashSet::new();
    for sig in sigs {
        assert!(seen.insert(selector_of(sig)), "selector collision on {sig}");
    }
}

#[test]
fn the_protocol_id_is_keccak_of_the_documented_string() {
    // `GlcRobinhoodBridge.BRIDGE_PROTOCOL_ID`.
    assert_eq!(
        bridge_protocol_id(),
        keccak256(b"glc.reserve-bridge.robinhood")
    );
}

fn payout_auth() -> PayoutAuth {
    PayoutAuth {
        route: Route::GlcToRhn,
        chains: ProtocolChainPair {
            source: 1001,
            dest: 2001,
        },
        token: addr(0x70),
        request_id: [0x11; 32],
        recipient: addr(0xec),
        amount: RobinhoodAtomic::new(1_234_000_000_000_000_000_000),
        signer_epoch: 7,
        expiry: 1_800_000_000,
    }
}

fn sigs() -> Vec<EvmSignature> {
    let mut a = [0x11u8; 65];
    a[64] = 27;
    let mut b = [0x22u8; 65];
    b[64] = 28;
    vec![
        EvmSignature::from_bytes(a).unwrap(),
        EvmSignature::from_bytes(b).unwrap(),
    ]
}

fn word(data: &[u8], index: usize) -> &[u8] {
    &data[4 + index * 32..4 + (index + 1) * 32]
}

#[test]
fn execute_payout_calldata_lays_out_the_struct_inline_then_the_signature_array() {
    let auth = payout_auth();
    let data = encode_execute_payout(&auth, &sigs());

    assert_eq!(&data[..4], &selector_of(SIG_EXECUTE_PAYOUT));
    // Six inline struct words, then one offset word for `bytes[]`.
    assert_eq!(word(&data, 0)[31], 0x01, "route byte GlcToRhn");
    assert_eq!(word(&data, 1), &[0x11u8; 32], "requestId");
    assert_eq!(&word(&data, 2)[12..], addr(0xec).as_bytes(), "recipient");
    assert_eq!(
        EvmU256::from_be_bytes(word(&data, 3).try_into().unwrap()),
        auth.amount.to_u256(),
        "amount, in Robinhood 18-decimal units"
    );
    assert_eq!(word(&data, 4)[31], 7, "signerEpoch");
    assert_eq!(
        EvmU256::from_be_bytes(word(&data, 5).try_into().unwrap())
            .try_to_u64()
            .unwrap(),
        1_800_000_000,
        "expiry"
    );
    assert_eq!(
        EvmU256::from_be_bytes(word(&data, 6).try_into().unwrap())
            .try_to_u64()
            .unwrap(),
        7 * 32,
        "the bytes[] offset must be seven head words, measured after the selector"
    );

    // The array itself: count, two element offsets, two 65-byte elements.
    let array = &data[4 + 7 * 32..];
    assert_eq!(array[31], 2, "exactly two signatures");
    let first = &array[32 + 64..];
    assert_eq!(first[31], 65, "each signature is 65 bytes");
    assert_eq!(&first[32..32 + 65], &sigs()[0].to_bytes()[..]);
}

#[test]
fn execute_settlement_calldata_has_four_struct_words_and_names_no_token() {
    let auth = SettlementAuth {
        route: Route::RhnToGlc,
        chains: ProtocolChainPair {
            source: 2001,
            dest: 1001,
        },
        request_id: [0x11; 32],
        obligation_index: 42,
        signer_epoch: 7,
        expiry: 1_800_000_000,
    };
    let data = encode_execute_settlement(&auth, &sigs());
    assert_eq!(&data[..4], &selector_of(SIG_EXECUTE_SETTLEMENT));
    assert_eq!(word(&data, 0), &[0x11u8; 32], "requestId");
    assert_eq!(word(&data, 1)[31], 42, "obligationIndex");
    assert_eq!(word(&data, 2)[31], 7, "signerEpoch");
    assert_eq!(
        EvmU256::from_be_bytes(word(&data, 4).try_into().unwrap())
            .try_to_u64()
            .unwrap(),
        5 * 32,
        "five head words: four struct fields plus the array offset"
    );
    // A settlement carries NO route byte on the wire — the contract reads
    // the obligation's own — so the route must not appear as a head word.
    assert_ne!(word(&data, 0)[31], 0x02, "no route byte in the struct");
}

#[test]
fn execute_refund_calldata_carries_recipient_and_amount_but_no_route() {
    let auth = RefundAuth {
        route: Route::RhnToGlc,
        chains: ProtocolChainPair {
            source: 2001,
            dest: 1001,
        },
        token: addr(0x70),
        request_id: [0x11; 32],
        obligation_index: 42,
        recipient: addr(0xd0),
        amount: RobinhoodAtomic::new(500_000_000_000_000_000_000),
        signer_epoch: 7,
        expiry: 1_800_000_000,
    };
    let data = encode_execute_refund(&auth, &sigs());
    assert_eq!(&data[..4], &selector_of(SIG_EXECUTE_REFUND));
    assert_eq!(word(&data, 0), &[0x11u8; 32]);
    assert_eq!(word(&data, 1)[31], 42);
    assert_eq!(&word(&data, 2)[12..], addr(0xd0).as_bytes());
    assert_eq!(
        EvmU256::from_be_bytes(word(&data, 3).try_into().unwrap()),
        auth.amount.to_u256()
    );
    assert_eq!(
        EvmU256::from_be_bytes(word(&data, 6).try_into().unwrap())
            .try_to_u64()
            .unwrap(),
        7 * 32,
        "seven head words"
    );
}

#[test]
fn calldata_changes_whenever_any_authorized_field_changes() {
    // The calldata and the digest are built from ONE value, so this also
    // pins that a tampered payload cannot be paired with a signature over
    // the original: the bytes on the wire differ too.
    let base = encode_execute_payout(&payout_auth(), &sigs());
    let mut other = payout_auth();
    other.recipient = addr(0x01);
    assert_ne!(base, encode_execute_payout(&other, &sigs()));

    let mut other = payout_auth();
    other.amount = RobinhoodAtomic::new(1);
    assert_ne!(base, encode_execute_payout(&other, &sigs()));

    let mut other = payout_auth();
    other.expiry += 1;
    assert_ne!(base, encode_execute_payout(&other, &sigs()));
}

#[test]
fn the_obligation_status_constants_match_the_contracts_enum_order() {
    // Appended, never reordered — each is a wire value.
    assert_eq!(OBLIGATION_STATUS_NONE, 0);
    assert_eq!(OBLIGATION_STATUS_PENDING, 1);
    assert_eq!(OBLIGATION_STATUS_SETTLED, 2);
    assert_eq!(OBLIGATION_STATUS_REFUNDED, 3);
    assert_eq!(OBLIGATION_STATUS_ABANDONED, 4);

    let ob = |status| Obligation {
        depositor: addr(1),
        status,
        route: 2,
        amount: EvmU256::ZERO,
    };
    assert!(ob(OBLIGATION_STATUS_PENDING).is_pending());
    for terminal in [
        OBLIGATION_STATUS_NONE,
        OBLIGATION_STATUS_SETTLED,
        OBLIGATION_STATUS_REFUNDED,
        OBLIGATION_STATUS_ABANDONED,
    ] {
        assert!(!ob(terminal).is_pending(), "status {terminal}");
    }
    assert_eq!(ob(OBLIGATION_STATUS_SETTLED).status_name(), "Settled");
    assert_eq!(ob(200).status_name(), "Unknown");
}

#[test]
fn the_three_actions_have_the_contracts_discriminators() {
    assert_eq!(ACTION_PAYOUT, 1);
    assert_eq!(ACTION_REFUND, 2);
    assert_eq!(ACTION_SETTLE, 3);
}

// ------------------------------------------ limits and rolling windows --
//
// The reads Phase G's reserve reporting needs. Each returns a struct of
// STATIC fields only, so it comes back as its fields inline — no offset
// word, no length word — and the decode is asserted field by field in
// declaration order, because a transposed pair here would report an
// inbound limit as an outbound one and both would look plausible.

/// Whole GLC as a Robinhood 18-decimal atomic word.
fn glc(whole: u64) -> EvmU256 {
    EvmU256::from_u128(u128::from(whole) * 1_000_000_000_000_000_000)
}

#[tokio::test]
async fn limits_decodes_all_seven_fields_in_declaration_order() {
    let node = crate::robinhood::testkit::MockNode::new(addr(0xb1));
    let reader = BridgeReader::new(addr(0xb1));
    let limits = reader
        .limits(&node, EvmBlockTag::Latest)
        .await
        .expect("the mock deployment answers limits()");

    // Distinct values per field, so a transposition cannot pass.
    assert_eq!(limits.inbound_min, glc(1));
    assert_eq!(limits.inbound_max, glc(10_000));
    assert_eq!(limits.inbound_rolling_limit, glc(100_000));
    assert_eq!(limits.outbound_min, glc(1));
    assert_eq!(limits.outbound_max, glc(10_000));
    assert_eq!(limits.outbound_rolling_limit, glc(100_000));
    assert_eq!(limits.protected_min_reserve, glc(1_000));
}

#[tokio::test]
async fn the_two_windows_are_read_independently() {
    let node = crate::robinhood::testkit::MockNode::new(addr(0xb1));
    let reader = BridgeReader::new(addr(0xb1));

    let inbound = reader
        .inbound_window(&node, EvmBlockTag::Latest)
        .await
        .expect("inboundWindow()");
    let outbound = reader
        .outbound_window(&node, EvmBlockTag::Latest)
        .await
        .expect("outboundWindow()");

    // The two directions are entirely independent accumulators; reading
    // one must never return the other's total.
    assert_eq!(inbound.total, glc(250));
    assert_eq!(outbound.total, glc(400));
    assert_eq!(inbound.window_start, 1_700_000_000);
    assert_eq!(outbound.window_start, 1_700_000_000);
}

#[test]
fn the_bucket_width_matches_the_contracts_constant() {
    // `GlcRobinhoodBridge.ROLLING_WINDOW_SECONDS = 24 hours`. A
    // transcription, so it is pinned.
    assert_eq!(ROLLING_WINDOW_SECONDS, 86_400);
}

#[test]
fn remaining_is_measured_against_the_current_bucket() {
    let window = RollingWindow {
        window_start: 1_000_000,
        total: glc(30),
    };
    let limit = glc(100);
    assert_eq!(window.resets_at(), 1_000_000 + 86_400);

    // Inside the bucket: what is left of the limit.
    assert!(window.is_current(1_000_000));
    assert!(window.is_current(1_000_000 + 86_399));
    assert_eq!(window.remaining(limit, 1_000_000 + 100), glc(70));
}

/// The fixed-bucket property, which is the thing an operator most easily
/// gets wrong: capacity does not trickle back, it returns all at once.
#[test]
fn an_expired_bucket_reports_the_whole_limit_again() {
    let window = RollingWindow {
        window_start: 1_000_000,
        total: glc(100),
    };
    let limit = glc(100);

    // One second before the boundary: exhausted.
    assert_eq!(window.remaining(limit, 1_000_000 + 86_399), EvmU256::ZERO);
    // At the boundary: the contract would reset `total`, so the full
    // limit is available. Reporting the stale total here would show a
    // consumption that is no longer charged against anything.
    assert!(!window.is_current(1_000_000 + 86_400));
    assert_eq!(window.remaining(limit, 1_000_000 + 86_400), limit);
    assert_eq!(window.remaining(limit, 2_000_000), limit);
}

/// A limit lowered under an existing bucket leaves the bucket above it.
/// That is zero remaining — never a wrap to an enormous number that would
/// read as unlimited capacity.
#[test]
fn a_total_above_a_lowered_limit_saturates_at_zero() {
    let window = RollingWindow {
        window_start: 1_000_000,
        total: glc(500),
    };
    assert_eq!(window.remaining(glc(100), 1_000_100), EvmU256::ZERO);
}
