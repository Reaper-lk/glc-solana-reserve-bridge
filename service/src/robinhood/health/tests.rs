//! The health state's reporting contract.

use super::*;
use crate::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID;
use crate::ledger::{RobinhoodHaltReason, RobinhoodObservationSummary};

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
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 1_700);
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
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 1_700);
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
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 0);
    health.record_tick(ROBINHOOD_TESTNET_CHAIN_ID, 50, None, Some(60), summary(), 1);
    assert_eq!(health.snapshot().lag_blocks, Some(0));
}

/// A definitive method error means the endpoint WAS reached; reporting it
/// as disconnected would point an operator at the network.
#[test]
fn only_a_transport_failure_clears_connected() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 0);
    health.record_tick(ROBINHOOD_TESTNET_CHAIN_ID, 10, None, None, summary(), 1);

    health.record_error("filter too wide", true, 2);
    let snapshot = health.snapshot();
    assert!(snapshot.connected);
    assert_eq!(snapshot.last_rpc_error.as_deref(), Some("filter too wide"));
    assert_eq!(snapshot.last_rpc_error_unix, Some(2));

    health.record_error("connection refused", false, 3);
    assert!(!health.snapshot().connected);
}

/// The last error survives a later success, so a flapping endpoint is
/// still visible; the two timestamps together are what say which is
/// current.
#[test]
fn the_last_error_is_kept_after_a_later_success() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 0);
    health.record_error("connection refused", false, 5);
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
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 0);
    health.record_reorg(40);
    health.record_reorg(1);
    let snapshot = health.snapshot();
    assert_eq!(snapshot.deepest_reorg_blocks, 40);
    assert_eq!(snapshot.reorgs_reconciled, 2);
}

#[test]
fn a_halt_is_mirrored_and_clearable() {
    let health = RobinhoodHealth::new(ROBINHOOD_TESTNET_CHAIN_ID, 0);
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
