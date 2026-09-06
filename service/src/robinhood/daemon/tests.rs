//! The loop's own behaviour: backoff, shutdown, and the fact that a
//! halted indexer keeps reporting.

use std::time::Duration;

use super::*;
use crate::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID;
use crate::ledger::Ledger;
use crate::robinhood::indexer::RobinhoodIndexer;
use crate::robinhood::testkit::{test_config, DepositParams, MockChain, MockEvmRpc};

const CHAIN_ID: crate::evm::EvmChainId = ROBINHOOD_TESTNET_CHAIN_ID;

fn config() -> RobinhoodLoopConfig {
    RobinhoodLoopConfig {
        tick_interval: Duration::from_millis(1),
        max_backoff: Duration::from_millis(4),
    }
}

fn indexer(chain: MockChain) -> RobinhoodIndexer<MockEvmRpc> {
    RobinhoodIndexer::new(
        MockEvmRpc::new(chain),
        Ledger::open_in_memory().expect("in-memory ledger"),
        test_config(CHAIN_ID, 0, 12, 1_000),
        crate::robinhood::health::RobinhoodHealth::new(CHAIN_ID, 0),
    )
}

#[test]
fn backoff_doubles_per_consecutive_failure_and_is_capped() {
    let base = Duration::from_millis(100);
    let max = Duration::from_millis(700);
    assert_eq!(tick_backoff_delay(base, max, 0), base);
    assert_eq!(tick_backoff_delay(base, max, 1), Duration::from_millis(200));
    assert_eq!(tick_backoff_delay(base, max, 2), Duration::from_millis(400));
    // Capped, never unbounded.
    assert_eq!(tick_backoff_delay(base, max, 3), max);
    assert_eq!(tick_backoff_delay(base, max, 30), max);
}

/// A shutdown already requested before the first tick means no tick runs
/// at all — and, with it, no RPC call.
#[tokio::test]
async fn a_pre_set_shutdown_runs_no_tick() {
    let mut indexer = indexer(MockChain::of_length(CHAIN_ID, 5));
    let (tx, rx) = tokio::sync::watch::channel(true);
    let ticks = run(&mut indexer, config(), rx, || 0).await;
    assert_eq!(ticks, 0);
    assert!(indexer.rpc.with(|chain| chain.calls.is_empty()));
    drop(tx);
}

#[tokio::test]
async fn the_loop_ticks_until_shutdown_and_finishes_its_work() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 5, 250));
    let mut indexer = indexer(chain);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = tx.send(true);
    });
    let ticks = run(&mut indexer, config(), rx, || 100).await;
    stopper.await.expect("stopper joins");

    assert!(ticks >= 1, "expected at least one tick, ran {ticks}");
    // The deposit was observed — the loop really did drive a full tick.
    assert_eq!(
        indexer
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        1
    );
}

/// A halted indexer must keep ticking: its ticks are what re-publish the
/// halt to the health state, and an indexer that went quiet would be
/// indistinguishable from one that recovered.
#[tokio::test]
async fn a_halted_indexer_keeps_ticking_and_keeps_reporting() {
    let mut indexer = indexer(MockChain::of_length(CHAIN_ID, 5));
    indexer
        .ledger_mut_for_tests()
        .robinhood_record_halt(
            crate::ledger::RobinhoodHaltReason::ObservationConflict,
            "an earlier incident",
            1,
        )
        .expect("halts");

    let (tx, rx) = tokio::sync::watch::channel(false);
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = tx.send(true);
    });
    let ticks = run(&mut indexer, config(), rx, || 100).await;
    stopper.await.expect("stopper joins");

    assert!(ticks >= 1);
    let snapshot = indexer.health().snapshot();
    assert_eq!(
        snapshot.halt.expect("halt is reported").reason,
        crate::ledger::RobinhoodHaltReason::ObservationConflict
    );
}
