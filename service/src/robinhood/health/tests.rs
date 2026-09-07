//! The health state's reporting contract.

use super::*;
use crate::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID;
use crate::ledger::{RobinhoodHaltReason, RobinhoodObservationSummary};

/// A credential-bearing endpoint, so every test in this module exercises
/// the redaction path rather than a trivially-clean one. The password and
/// the host below must never appear in any snapshot field.
const TEST_RPC_URL: &str = "https://user:sup3rs3cret@rpc.rhn.invalid:8545/v2/KEY0123456789";
const TEST_RPC_PASSWORD: &str = "sup3rs3cret";
const TEST_RPC_HOST: &str = "rpc.rhn.invalid";

fn summary() -> RobinhoodObservationSummary {
    RobinhoodObservationSummary {
        provisional: 2,
        finalized: 3,
        reorged: 1,
        highest_finalized_block: Some(90),
    }
}

/// The distinction a reader needs: "no Robinhood indexer in this
/// deployment" is not the same as "the Robinhood indexer is not
/// answering".
#[test]
fn unconfigured_reports_configured_false_and_nothing_else() {
    let snapshot = RobinhoodHealth::unconfigured().snapshot();
    assert!(!snapshot.configured);
    assert!(!snapshot.connected);
    assert_eq!(snapshot.expected_chain_id, None);
    assert_eq!(snapshot.observed_chain_id, None);
    assert_eq!(snapshot.head_block, None);
    assert_eq!(snapshot.cursor_block, None);
    assert_eq!(snapshot.last_success_unix, None);
    assert_eq!(snapshot.halt, None);
}

#[test]
fn a_configured_indexer_knows_its_expected_chain_id_before_any_tick() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 1_700);
    let snapshot = health.snapshot();
    assert!(snapshot.configured);
    assert_eq!(
        snapshot.expected_chain_id,
        Some(ROBINHOOD_TESTNET_CHAIN_ID.get())
    );
    // Seeded from process start so the first scrape does not read as
    // decades of silence.
    assert_eq!(snapshot.last_success_unix, Some(1_700));
    assert_eq!(snapshot.observed_chain_id, None);
}

#[test]
fn a_tick_publishes_head_finality_cursor_and_lag_together() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 1_700);
    health.record_tick(
        ROBINHOOD_TESTNET_CHAIN_ID,
        100,
        Some(89),
        Some(97),
        summary(),
        1_800,
    );
    let snapshot = health.snapshot();
    assert!(snapshot.connected);
    assert_eq!(
        snapshot.observed_chain_id,
        Some(ROBINHOOD_TESTNET_CHAIN_ID.get())
    );
    assert_eq!(snapshot.head_block, Some(100));
    assert_eq!(snapshot.finalized_block, Some(89));
    assert_eq!(snapshot.cursor_block, Some(97));
    assert_eq!(snapshot.lag_blocks, Some(3));
    assert_eq!(snapshot.last_success_unix, Some(1_800));
    assert_eq!(snapshot.observations, summary());
}

/// A cursor above the head is a chain that went backwards, which the
/// reorg path deals with; the lag gauge must not underflow into a huge
/// number on the way there.
#[test]
fn lag_never_goes_negative() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_tick(ROBINHOOD_TESTNET_CHAIN_ID, 50, None, Some(60), summary(), 1);
    assert_eq!(health.snapshot().lag_blocks, Some(0));
}

/// A definitive method error means the endpoint WAS reached; reporting it
/// as disconnected would point an operator at the network.
#[test]
fn only_a_transport_failure_clears_connected() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_tick(ROBINHOOD_TESTNET_CHAIN_ID, 10, None, None, summary(), 1);

    health.record_error(RobinhoodRpcErrorClass::RpcMethod, "filter too wide", 2);
    let snapshot = health.snapshot();
    assert!(snapshot.connected);
    assert_eq!(snapshot.last_rpc_error.as_deref(), Some("filter too wide"));
    assert_eq!(
        snapshot.last_rpc_error_class,
        Some(RobinhoodRpcErrorClass::RpcMethod)
    );
    assert_eq!(snapshot.last_rpc_error_unix, Some(2));

    health.record_error(RobinhoodRpcErrorClass::Transport, "connection refused", 3);
    assert!(!health.snapshot().connected);
}

/// The last error survives a later success, so a flapping endpoint is
/// still visible; the two timestamps together are what say which is
/// current.
#[test]
fn the_last_error_is_kept_after_a_later_success() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_error(RobinhoodRpcErrorClass::Transport, "connection refused", 5);
    health.record_tick(ROBINHOOD_TESTNET_CHAIN_ID, 10, None, None, summary(), 9);
    let snapshot = health.snapshot();
    assert_eq!(
        snapshot.last_rpc_error.as_deref(),
        Some("connection refused")
    );
    assert_eq!(snapshot.last_rpc_error_unix, Some(5));
    assert_eq!(snapshot.last_success_unix, Some(9));
    assert!(snapshot.connected);
}

/// The deepest reorg is what an operator needs to see trending toward the
/// finality depth; a later shallow one must not erase it.
#[test]
fn the_deepest_reorg_is_kept_not_the_most_recent() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_reorg(40);
    health.record_reorg(1);
    let snapshot = health.snapshot();
    assert_eq!(snapshot.deepest_reorg_blocks, 40);
    assert_eq!(snapshot.reorgs_reconciled, 2);
}

#[test]
fn a_halt_is_mirrored_and_clearable() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    let halt = crate::ledger::RobinhoodHalt {
        reason: RobinhoodHaltReason::ChainIdMismatch,
        detail: "wrong network".to_string(),
        halted_at: 12,
    };
    health.set_halt(Some(halt.clone()));
    assert_eq!(health.snapshot().halt, Some(halt));
    health.set_halt(None);
    assert_eq!(health.snapshot().halt, None);
}

// ---------------------------------------------------------------- redaction --
//
// The health surface is served by `ops::health`, which has no
// authentication by design. Anything reaching this snapshot is readable
// by anything that can reach that port, so these are the tests that say
// an RPC credential cannot be one of those things.

/// Asserts a snapshot carries no part of the configured endpoint.
fn assert_snapshot_is_clean(snapshot: &RobinhoodHealthSnapshot) {
    let rendered = format!("{snapshot:?}").to_ascii_lowercase();
    for secret in [
        TEST_RPC_URL,
        TEST_RPC_PASSWORD,
        TEST_RPC_HOST,
        "user:sup3rs3cret",
    ] {
        assert!(
            !rendered.contains(&secret.to_ascii_lowercase()),
            "the published snapshot contains {secret:?}:\n{snapshot:#?}"
        );
    }
    assert!(
        !rendered.contains("://"),
        "the published snapshot contains a scheme-qualified URL:\n{snapshot:#?}"
    );
}

/// The exact leak this closes: reqwest's `Display` embeds the request
/// URL, credentials and all, and that string used to be published
/// verbatim.
#[test]
fn a_reqwest_style_transport_error_cannot_put_the_rpc_url_in_the_snapshot() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_error(
        RobinhoodRpcErrorClass::Transport,
        &format!("error sending request for url ({TEST_RPC_URL})"),
        7,
    );
    let snapshot = health.snapshot();
    assert_snapshot_is_clean(&snapshot);

    // What survives is what an operator triages on.
    let message = snapshot.last_rpc_error.expect("an error was recorded");
    assert!(message.contains("error sending request"), "{message}");
    assert_eq!(
        snapshot.last_rpc_error_class,
        Some(RobinhoodRpcErrorClass::Transport)
    );
    assert!(!snapshot.connected);
}

/// A bare hostname with no scheme — a DNS failure — is caught by the
/// endpoint-literal pass rather than the URL-shape one.
#[test]
fn a_dns_error_naming_only_the_host_is_still_redacted() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_error(
        RobinhoodRpcErrorClass::Transport,
        "dns error: failed to lookup address information for rpc.rhn.invalid",
        7,
    );
    assert_snapshot_is_clean(&health.snapshot());
}

/// A hostile or misconfigured node's own `message` field is text this
/// service does not control, and it reaches the same surface.
#[test]
fn a_node_supplied_message_echoing_the_url_is_redacted() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.record_error(
        RobinhoodRpcErrorClass::RpcMethod,
        &format!("Robinhood EVM RPC method error (code -32000): upstream {TEST_RPC_URL} refused"),
        7,
    );
    let snapshot = health.snapshot();
    assert_snapshot_is_clean(&snapshot);
    // The JSON-RPC code is not a secret and must survive.
    let message = snapshot.last_rpc_error.expect("an error was recorded");
    assert!(message.contains("-32000"), "{message}");
}

/// Halt details are constructed internally today, but they reach the same
/// unauthenticated surface, so the same filter applies to them.
#[test]
fn a_halt_detail_is_redacted_too() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
    health.set_halt(Some(crate::ledger::RobinhoodHalt {
        reason: RobinhoodHaltReason::ChainIdMismatch,
        detail: format!("endpoint {TEST_RPC_URL} reports chain id 1"),
        halted_at: 12,
    }));
    let snapshot = health.snapshot();
    assert_snapshot_is_clean(&snapshot);
    let halt = snapshot.halt.expect("a halt was recorded");
    assert_eq!(halt.reason, RobinhoodHaltReason::ChainIdMismatch);
    // The diagnostic half of the detail survives.
    assert!(
        halt.detail.contains("reports chain id 1"),
        "{}",
        halt.detail
    );
}

/// An unconfigured deployment holds no endpoint literals, but must still
/// refuse to publish a URL it is handed.
#[test]
fn an_unconfigured_health_still_strips_a_url_it_is_given() {
    let health = RobinhoodHealth::unconfigured();
    health.record_error(
        RobinhoodRpcErrorClass::Transport,
        "error sending request for url (https://someone:else@elsewhere.invalid/x)",
        7,
    );
    let message = health.snapshot().last_rpc_error.expect("recorded");
    assert!(!message.contains("://"), "{message}");
    assert!(!message.contains("elsewhere.invalid"), "{message}");
}

/// The class is derived from the error's TYPE, so it says whether the
/// endpoint was reached even though the message has been redacted.
#[test]
fn the_error_class_survives_redaction_and_decides_connected() {
    for (class, expected_connected) in [
        (RobinhoodRpcErrorClass::Transport, false),
        (RobinhoodRpcErrorClass::RpcMethod, true),
        (RobinhoodRpcErrorClass::MalformedResponse, true),
        (RobinhoodRpcErrorClass::Decode, true),
        (RobinhoodRpcErrorClass::ChainDisagreement, true),
        (RobinhoodRpcErrorClass::Ledger, true),
    ] {
        let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, TEST_RPC_URL, 0);
        health.record_error(class, TEST_RPC_URL, 1);
        let snapshot = health.snapshot();
        assert_eq!(snapshot.last_rpc_error_class, Some(class));
        assert_eq!(
            snapshot.connected,
            expected_connected,
            "{} must report reached_endpoint = {expected_connected}",
            class.as_str()
        );
        assert_snapshot_is_clean(&snapshot);
    }
}
