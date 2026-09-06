//! The indexer's behaviour against a scriptable in-memory chain.
//!
//! Every test here is deterministic and offline: [`MockChain`] is a list
//! of blocks, so a reorg is expressed by replacing a suffix of that list
//! — the same thing a reorg is on a real chain — rather than by scripting
//! the sequence of answers somebody expected the node to give.

use std::collections::VecDeque;

use super::*;
use crate::evm::networks::{ROBINHOOD_MAINNET_CHAIN_ID, ROBINHOOD_TESTNET_CHAIN_ID};
use crate::ledger::{Ledger, RobinhoodFinality};
use crate::robinhood::rpc::EvmRpcError;
use crate::robinhood::testkit::{
    block_hash, test_config, DepositParams, MockChain, MockEvmRpc, BRIDGE,
};
use crate::routes::Route;

const CHAIN_ID: crate::evm::EvmChainId = ROBINHOOD_TESTNET_CHAIN_ID;

fn indexer_with(
    chain: MockChain,
    start_block: u64,
    confirmation_depth: u64,
    max_range: u64,
) -> RobinhoodIndexer<MockEvmRpc> {
    let ledger = Ledger::open_in_memory().expect("in-memory ledger");
    let config = test_config(CHAIN_ID, start_block, confirmation_depth, max_range);
    let health = crate::robinhood::health::RobinhoodHealth::new(CHAIN_ID, 0);
    RobinhoodIndexer::new(MockEvmRpc::new(chain), ledger, config, health)
}

fn progressed(outcome: &RobinhoodTickOutcome) -> (u64, Option<u64>, u32, u32, usize) {
    match outcome {
        RobinhoodTickOutcome::Progressed {
            head,
            cursor,
            recorded,
            already_recorded,
            finalized,
            ..
        } => (*head, *cursor, *recorded, *already_recorded, *finalized),
        other => panic!("expected progress, got {other:?}"),
    }
}

// ------------------------------------------------------- the happy path --

#[tokio::test]
async fn scans_from_the_configured_start_block_and_records_a_deposit() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    let mut indexer = indexer_with(chain, 5, 12, 1_000);

    let outcome = indexer.tick(100).await.expect("ticks");
    let (head, cursor, recorded, _, finalized) = progressed(&outcome);
    assert_eq!(head, 20);
    assert_eq!(cursor, Some(20));
    assert_eq!(recorded, 1);
    // Block 10 at head 20 is 11 deep; the depth is 12, so not yet.
    assert_eq!(finalized, 0);

    let rows = indexer.ledger().robinhood_observations().expect("reads");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].observation.obligation_index, 0);
    assert_eq!(rows[0].observation.route, Route::RhnToGlc);
    assert_eq!(rows[0].observation.amount_canonical_atomic, 250);
    assert_eq!(rows[0].observation.source_contract, BRIDGE.to_bytes());
    assert_eq!(rows[0].observation.block_number, 10);
    assert_eq!(rows[0].finality, RobinhoodFinality::Provisional);
}

/// A deposit BELOW the configured start block is intentionally outside
/// this deployment's scope — the same meaning the Goldcoin indexer's
/// initial checkpoint carries.
#[tokio::test]
async fn never_scans_below_the_configured_start_block() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 3, 250));
    let mut indexer = indexer_with(chain, 10, 12, 1_000);

    indexer.tick(100).await.expect("ticks");
    assert!(indexer
        .ledger()
        .robinhood_observations()
        .expect("reads")
        .is_empty());
}

#[tokio::test]
async fn promotes_to_final_once_the_confirmation_depth_is_reached() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");

    indexer.rpc.with(|chain| chain.extend(1));
    let outcome = indexer.tick(101).await.expect("ticks");
    let (head, _, _, _, finalized) = progressed(&outcome);
    assert_eq!(head, 21);
    assert_eq!(finalized, 1);
    assert_eq!(
        indexer.ledger().robinhood_observations().expect("reads")[0].finality,
        RobinhoodFinality::Final
    );
}

/// The cursor is what makes a restart safe: a second tick with no new
/// blocks rescans nothing and records nothing.
#[tokio::test]
async fn a_second_tick_with_no_new_blocks_scans_nothing() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");

    let outcome = indexer.tick(101).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Progressed { blocks_scanned, .. } => assert_eq!(blocks_scanned, 0),
        other => panic!("expected progress, got {other:?}"),
    }
    assert_eq!(
        indexer
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        1
    );
}

/// Restart safety, stated directly: a fresh indexer over the SAME ledger
/// resumes from the cursor and re-observes nothing.
#[tokio::test]
async fn a_restart_resumes_from_the_cursor_and_re_records_nothing() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    let rpc = MockEvmRpc::new(chain);
    let mut ledger_path = Ledger::open_in_memory().expect("in-memory ledger");

    // First process.
    {
        let config = test_config(CHAIN_ID, 0, 12, 1_000);
        let health = crate::robinhood::health::RobinhoodHealth::new(CHAIN_ID, 0);
        let mut indexer = RobinhoodIndexer::new(
            rpc.clone(),
            std::mem::replace(
                &mut ledger_path,
                Ledger::open_in_memory().expect("placeholder"),
            ),
            config,
            health,
        );
        indexer.tick(100).await.expect("ticks");
        ledger_path = indexer.ledger;
    }

    // Second process, same ledger, same chain.
    let config = test_config(CHAIN_ID, 0, 12, 1_000);
    let health = crate::robinhood::health::RobinhoodHealth::new(CHAIN_ID, 0);
    let mut restarted = RobinhoodIndexer::new(rpc, ledger_path, config, health);
    let outcome = restarted.tick(200).await.expect("ticks");
    let (_, cursor, recorded, _, _) = progressed(&outcome);
    assert_eq!(cursor, Some(20));
    assert_eq!(recorded, 0);
    assert_eq!(
        restarted
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        1
    );
}

/// Chunking must not skip a block. The deposit sits in the last block of
/// the second chunk, which is where an off-by-one in the range arithmetic
/// would lose it.
#[tokio::test]
async fn chunked_scanning_covers_every_block_with_no_gaps() {
    let mut chain = MockChain::of_length(CHAIN_ID, 31);
    chain.add_deposit(&DepositParams::valid(0, 19, 100));
    chain.add_deposit(&DepositParams {
        tx_seed: 77,
        ..DepositParams::valid(1, 20, 100)
    });
    let mut indexer = indexer_with(chain, 0, 12, 10);

    let outcome = indexer.tick(100).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Progressed {
            blocks_scanned,
            recorded,
            cursor,
            ..
        } => {
            assert_eq!(blocks_scanned, 31);
            assert_eq!(recorded, 2);
            assert_eq!(cursor, Some(30));
        }
        other => panic!("expected progress, got {other:?}"),
    }
}

/// Rescanning an overlapping range is free, which is what lets the cursor
/// be rolled back without fear.
#[tokio::test]
async fn rescanning_an_overlap_is_idempotent() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");

    // Force a rescan by rewinding the cursor to before the deposit,
    // leaving the observation in place.
    indexer
        .ledger
        .robinhood_apply_scan_range(&[], &[], 5, block_hash(5, 0), 12, 101)
        .expect("rewinds the cursor");
    indexer
        .ledger
        .conn_for_tests()
        .execute(
            "DELETE FROM robinhood_scanned_blocks WHERE block_number > 5",
            [],
        )
        .expect("drops the later anchors");

    let outcome = indexer.tick(102).await.expect("ticks");
    let (_, _, recorded, already, _) = progressed(&outcome);
    assert_eq!(recorded, 0);
    assert_eq!(already, 1);
    assert_eq!(
        indexer
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        1
    );
}

// ------------------------------------------------------------- reorgs --

#[tokio::test]
async fn a_provisional_reorg_is_reconciled_and_the_deposit_is_re_observed() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 14, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");
    assert_eq!(
        indexer
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        1
    );

    // The chain reorgs from block 12 onward; the deposit is gone.
    indexer.rpc.with(|chain| chain.reorg_from(12, 18, 1));

    let outcome = indexer.tick(101).await.expect("ticks");
    match &outcome {
        RobinhoodTickOutcome::Progressed { reorg: Some(r), .. } => {
            // The rollback target is the newest anchor that still agrees,
            // which is at or below the true fork point — never above it.
            assert!(
                r.fork_block <= 12,
                "rolled back to {} which is above the fork at 12",
                r.fork_block
            );
            assert_eq!(r.old_cursor_block, 15);
            assert_eq!(r.orphaned_observations, 1);
        }
        other => panic!("expected a reconciled reorg, got {other:?}"),
    }
    let rows = indexer.ledger().robinhood_observations().expect("reads");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].finality, RobinhoodFinality::Reorged);
}

/// The same deposit reappearing on the new chain claims the identity its
/// tombstoned predecessor released.
#[tokio::test]
async fn a_re_included_deposit_claims_the_tombstoned_identity() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 14, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");

    indexer.rpc.with(|chain| {
        chain.reorg_from(12, 18, 1);
        // Re-included one block later, on the new fork.
        chain.add_deposit(&DepositParams {
            block_number: 15,
            block_fork: 1,
            ..DepositParams::valid(0, 15, 250)
        });
    });

    indexer.tick(101).await.expect("ticks");
    let rows = indexer.ledger().robinhood_observations().expect("reads");
    assert_eq!(rows.len(), 2);
    let live: Vec<_> = rows
        .iter()
        .filter(|r| r.finality != RobinhoodFinality::Reorged)
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].observation.block_number, 15);
}

/// The incident. A reorg that reaches back past a FINALIZED observation
/// is not reconciled — it halts.
#[tokio::test]
async fn a_post_finality_reorg_halts_instead_of_rolling_back() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    // Depth 2, so block 10 is final as soon as the head reaches 11.
    let mut indexer = indexer_with(chain, 0, 2, 1_000);
    indexer.tick(100).await.expect("ticks");
    assert_eq!(
        indexer.ledger().robinhood_observations().expect("reads")[0].finality,
        RobinhoodFinality::Final
    );

    indexer.rpc.with(|chain| chain.reorg_from(8, 20, 1));
    let outcome = indexer.tick(101).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Halted { reason, .. } => {
            assert_eq!(
                reason,
                crate::ledger::RobinhoodHaltReason::PostFinalityReorg
            );
        }
        other => panic!("expected a halt, got {other:?}"),
    }
    // The finalized observation is untouched, and the halt persists.
    assert_eq!(
        indexer.ledger().robinhood_observations().expect("reads")[0].finality,
        RobinhoodFinality::Final
    );
    assert!(indexer.ledger().robinhood_halt().expect("reads").is_some());
}

/// A fork point older than every retained anchor cannot be proven, so it
/// is never guessed at.
#[tokio::test]
async fn a_reorg_beyond_every_retained_anchor_halts() {
    let chain = MockChain::of_length(CHAIN_ID, 16);
    let mut indexer = indexer_with(chain, 10, 2, 1_000);
    indexer.tick(100).await.expect("ticks");

    // Every block changes, including the one below the retention floor.
    indexer.rpc.with(|chain| chain.reorg_from(0, 20, 9));
    let outcome = indexer.tick(101).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Halted { reason, .. } => assert_eq!(
            reason,
            crate::ledger::RobinhoodHaltReason::ReorgBeyondRetainedAnchors
        ),
        other => panic!("expected a halt, got {other:?}"),
    }
}

// -------------------------------------------------------------- halts --

/// Checked every tick, not once at startup: an endpoint can be repointed
/// underneath a running process.
#[tokio::test]
async fn a_chain_id_change_mid_run_halts() {
    let chain = MockChain::of_length(CHAIN_ID, 16);
    let mut indexer = indexer_with(chain, 0, 2, 1_000);
    indexer.tick(100).await.expect("ticks");

    indexer
        .rpc
        .with(|chain| chain.chain_id = ROBINHOOD_MAINNET_CHAIN_ID);
    let outcome = indexer.tick(101).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Halted { reason, detail } => {
            assert_eq!(reason, crate::ledger::RobinhoodHaltReason::ChainIdMismatch);
            assert!(detail.contains(&ROBINHOOD_MAINNET_CHAIN_ID.get().to_string()));
        }
        other => panic!("expected a halt, got {other:?}"),
    }
}

/// A halt survives a restart, because it is persisted rather than held in
/// process memory — a bounce must not look like a fix.
#[tokio::test]
async fn a_halt_survives_a_restart_and_stops_all_further_work() {
    let chain = MockChain::of_length(CHAIN_ID, 16);
    let mut indexer = indexer_with(chain, 0, 2, 1_000);
    indexer
        .rpc
        .with(|chain| chain.chain_id = ROBINHOOD_MAINNET_CHAIN_ID);
    indexer.tick(100).await.expect("ticks");

    // The endpoint is fixed, but the halt stands.
    indexer.rpc.with(|chain| chain.chain_id = CHAIN_ID);
    let calls_before = indexer.rpc.with(|chain| chain.calls.len());
    let outcome = indexer.tick(101).await.expect("ticks");
    assert!(matches!(outcome, RobinhoodTickOutcome::Halted { .. }));
    // A halted tick makes NO RPC call at all.
    assert_eq!(indexer.rpc.with(|chain| chain.calls.len()), calls_before);

    indexer.ledger.robinhood_clear_halt(102).expect("clears");
    assert!(matches!(
        indexer.tick(103).await.expect("ticks"),
        RobinhoodTickOutcome::Progressed { .. }
    ));
}

/// The contract cannot emit a deposit for an outbound route, so seeing
/// one means the configured address is not the expected contract.
#[tokio::test]
async fn an_outbound_route_in_a_deposit_log_halts() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams {
        // ROUTE_GLC_TO_RHN — a payout route.
        contract_route_id: 0x01,
        ..DepositParams::valid(0, 10, 250)
    });
    let mut indexer = indexer_with(chain, 0, 12, 1_000);

    let outcome = indexer.tick(100).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Halted { reason, .. } => assert_eq!(
            reason,
            crate::ledger::RobinhoodHaltReason::UnexpectedContractRoute
        ),
        other => panic!("expected a halt, got {other:?}"),
    }
    // Nothing was recorded, and the cursor did not advance past it.
    assert!(indexer
        .ledger()
        .robinhood_observations()
        .expect("reads")
        .is_empty());
    assert_eq!(
        indexer.ledger().robinhood_scan_cursor().expect("reads"),
        None
    );
}

/// A conflicting duplicate halts, and the range it was in is rolled back
/// whole.
#[tokio::test]
async fn a_conflicting_duplicate_halts_and_leaves_the_cursor_where_it_was() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");
    let cursor_before = indexer.ledger().robinhood_scan_cursor().expect("reads");

    // The same obligation index reappears later, saying something else.
    indexer.rpc.with(|chain| {
        chain.extend(4);
        chain.add_deposit(&DepositParams {
            tx_seed: 999,
            amount: 999 * crate::robinhood::testkit::CANONICAL_SCALE,
            ..DepositParams::valid(0, 18, 999)
        });
    });

    let outcome = indexer.tick(101).await.expect("ticks");
    match outcome {
        RobinhoodTickOutcome::Halted { reason, detail } => {
            assert_eq!(
                reason,
                crate::ledger::RobinhoodHaltReason::ObservationConflict
            );
            assert!(detail.contains("obligation 0"), "detail was {detail:?}");
        }
        other => panic!("expected a halt, got {other:?}"),
    }
    assert_eq!(
        indexer.ledger().robinhood_scan_cursor().expect("reads"),
        cursor_before
    );
    assert_eq!(
        indexer
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        1
    );
}

// --------------------------------------------------- failure handling --

/// A transport failure is an error, not a halt: nothing is recorded, the
/// cursor stays put, and the next tick simply retries.
#[tokio::test]
async fn a_transport_failure_fails_the_tick_without_halting_or_moving_the_cursor() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    chain.fail_next = VecDeque::from(vec![
        // Three attempts, all failing, exhausts the retry budget.
        EvmRpcError::Transport("connection refused".into()),
        EvmRpcError::Transport("connection refused".into()),
        EvmRpcError::Transport("connection refused".into()),
    ]);
    let mut indexer = indexer_with(chain, 0, 12, 1_000);

    let error = indexer.tick(100).await.expect_err("fails");
    assert!(matches!(error, RobinhoodIndexerError::NodeUnavailable(_)));
    assert_eq!(
        indexer.ledger().robinhood_scan_cursor().expect("reads"),
        None
    );
    assert!(indexer.ledger().robinhood_halt().expect("reads").is_none());

    // And it recovers on the next tick with no operator action.
    let outcome = indexer.tick(101).await.expect("ticks");
    let (_, _, recorded, _, _) = progressed(&outcome);
    assert_eq!(recorded, 1);
}

#[tokio::test]
async fn a_transport_failure_is_published_to_the_health_state() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.fail_next = VecDeque::from(vec![
        EvmRpcError::Transport("connection refused".into()),
        EvmRpcError::Transport("connection refused".into()),
        EvmRpcError::Transport("connection refused".into()),
    ]);
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect_err("fails");

    let snapshot = indexer.health().snapshot();
    assert!(snapshot.configured);
    assert!(!snapshot.connected);
    assert!(snapshot.last_rpc_error.is_some());
    assert_eq!(snapshot.last_rpc_error_unix, Some(100));
}

#[tokio::test]
async fn a_successful_tick_publishes_every_health_field() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 5, 250));
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");

    let snapshot = indexer.health().snapshot();
    assert!(snapshot.configured);
    assert!(snapshot.connected);
    assert_eq!(snapshot.expected_chain_id, Some(CHAIN_ID.get()));
    assert_eq!(snapshot.observed_chain_id, Some(CHAIN_ID.get()));
    assert_eq!(snapshot.head_block, Some(20));
    // head 20, depth 12 -> blocks at or below 9 are final.
    assert_eq!(snapshot.finalized_block, Some(9));
    assert_eq!(snapshot.cursor_block, Some(20));
    assert_eq!(snapshot.lag_blocks, Some(0));
    assert_eq!(snapshot.last_success_unix, Some(100));
    assert_eq!(snapshot.halt, None);
    assert_eq!(snapshot.observations.finalized, 1);
}

/// A log the node itself flags as no longer canonical is not recorded.
/// Nothing is lost: the next tick's anchor check is what decides
/// canonicality.
#[tokio::test]
async fn a_removed_log_is_skipped() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    chain.blocks[10].logs[0].removed = true;
    let mut indexer = indexer_with(chain, 0, 12, 1_000);

    let outcome = indexer.tick(100).await.expect("ticks");
    let (_, cursor, recorded, _, _) = progressed(&outcome);
    assert_eq!(recorded, 0);
    assert_eq!(cursor, Some(15));
}

/// The block hash is part of what an observation asserts, so one the
/// endpoint does not itself stand behind is refused rather than recorded.
#[tokio::test]
async fn a_log_claiming_a_block_hash_the_endpoint_disagrees_with_is_refused() {
    let mut chain = MockChain::of_length(CHAIN_ID, 16);
    chain.add_deposit(&DepositParams::valid(0, 10, 250));
    // The block's own hash is changed, leaving the log's claim stale.
    chain.blocks[10].hash = block_hash(10, 7);
    let mut indexer = indexer_with(chain, 0, 12, 1_000);

    let error = indexer.tick(100).await.expect_err("refuses");
    assert!(matches!(
        error,
        RobinhoodIndexerError::LogBlockHashMismatch { block: 10, .. }
    ));
    assert!(indexer
        .ledger()
        .robinhood_observations()
        .expect("reads")
        .is_empty());
}

// ------------------------------------------- observation is not settlement --

/// The property this whole phase exists to preserve: a valid, decoded,
/// recorded deposit on a DISABLED route changes nothing about
/// settlement.
#[tokio::test]
async fn observing_a_deposit_settles_nothing_and_opens_no_route() {
    let mut chain = MockChain::of_length(CHAIN_ID, 21);
    chain.add_deposit(&DepositParams::valid(0, 5, 250));
    chain.add_deposit(&DepositParams {
        contract_route_id: Route::RhnToSol
            .contract_route_id()
            .expect("RhnToSol has a contract route id"),
        tx_seed: 42,
        ..DepositParams::valid(1, 6, 100)
    });
    let mut indexer = indexer_with(chain, 0, 12, 1_000);
    indexer.tick(100).await.expect("ticks");

    // Both deposits were seen...
    assert_eq!(
        indexer
            .ledger()
            .robinhood_observations()
            .expect("reads")
            .len(),
        2
    );

    // ...and neither produced a bridge request of any kind.
    let requests: i64 = indexer
        .ledger()
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .expect("counts");
    assert_eq!(requests, 0);

    // The route gate is exactly as closed as it was before.
    let gate = crate::routes::RouteGate::legacy_only();
    for route in [Route::RhnToGlc, Route::RhnToSol] {
        assert!(!gate.is_enabled(indexer.ledger(), route));
    }

    // Phase F gave `RhnToGlc` a settlement `Direction`, so the firewall
    // that guarded it is now the route GATE rather than the absence of a
    // value — asserted above, and unchanged by anything the indexer did.
    // For `RhnToSol` the original, stronger guarantee still holds: there
    // is no `Direction` to reach any value-moving function with, and the
    // database cannot spell one either.
    assert_eq!(Route::RhnToSol.as_direction(), None);
    assert_eq!(Route::SolToRhn.as_direction(), None);
    for unspellable in ["SolToRhn", "RhnToSol"] {
        assert!(
            indexer
                .ledger()
                .conn_for_tests()
                .execute(
                    "INSERT INTO bridge_requests
                        (direction, state, gross_amount_atomic, recipient, created_at,
                         source_chain)
                     VALUES (?1, 'AwaitingDeposit', 1, X'00', 1, 'robinhood')",
                    [unspellable],
                )
                .is_err(),
            "the database must refuse a {unspellable} settlement row",
        );
    }
}

#[test]
fn the_finality_frontier_matches_the_goldcoin_depth_arithmetic() {
    // depth 1 -> the head block itself is final.
    assert_eq!(finalized_frontier(100, 1), Some(100));
    assert_eq!(finalized_frontier(100, 12), Some(89));
    // A chain shorter than the depth finalizes nothing.
    assert_eq!(finalized_frontier(3, 12), None);
}
