//! Tests for Robinhood reserve reconciliation.
//!
//! The properties under test are the ones that make this component safe
//! to run against mainnet on launch day, in the order they matter:
//!
//! 1. a funded bridge contract actually refreshes the reserve row, which
//!    is the bug this module exists to fix;
//! 2. a read that fails leaves the cached balance ALONE — it never
//!    invents, guesses or zeroes a balance;
//! 3. a real balance below the protected minimum breaches and pauses,
//!    through the existing path, and never auto-unpauses;
//! 4. none of it requires `[robinhood.settlement]`;
//! 5. none of it opens a route.
//!
//! Everything runs against `super::super::testkit::MockNode`, the same
//! scriptable node the settlement and preflight tests use. No test here
//! contacts a real endpoint.

use super::*;

use crate::amount_conversion::robinhood::CANONICAL_TO_ROBINHOOD_SCALE;
use crate::ledger::Ledger;
use crate::reconciliation::Classification;
use crate::robinhood::testkit::{MockNode, BRIDGE, TOKEN};
use crate::routes::{Route, RouteGate};

/// One whole GLC in Robinhood 18-decimal atomic units.
const ONE_GLC_ROBINHOOD: u128 = 1_000_000_000_000_000_000;
/// One whole GLC in canonical 8-decimal atomic units.
const ONE_GLC_CANONICAL: u64 = 100_000_000;

/// The production shape: 2,000,000,000,000 canonical atomic units of
/// protected minimum (20,000 GLC), which is the figure
/// `glc-admin robinhood-status` was reporting `available -2000000000000`
/// against.
const PROTECTED_MINIMUM: u64 = 2_000_000_000_000;

/// A ledger with a `RobinhoodReserve` row seeded exactly the way
/// `glc-bridge-daemon` seeds it: `initial_balance: 0`. That zero is the
/// starting condition every test here begins from, because it is the
/// starting condition production is in.
fn ledger_with_robinhood_reserve() -> Ledger {
    let mut ledger = Ledger::open_in_memory().expect("an in-memory ledger");
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            // initial_balance: 0 — exactly what `glc-bridge-daemon`
            // seeds, and the starting condition production is in.
            0,
            PROTECTED_MINIMUM,
            PROTECTED_MINIMUM * 8,
            PROTECTED_MINIMUM * 4,
            // `critical_reserve` must exceed `protected_minimum`.
            PROTECTED_MINIMUM * 2,
            0,
        )
        .expect("configure the Robinhood reserve");
    ledger
}

/// A ledger with NO `[reserve.robinhood]` equivalent — no Robinhood
/// reserve row at all.
fn ledger_without_robinhood_reserve() -> Ledger {
    Ledger::open_in_memory().expect("an in-memory ledger")
}

/// The reconciler under test, wired to `node` through the indexer config
/// alone. Note what is NOT passed: no settlement config, no submitter,
/// no authorization signers, no verified deployment.
fn reconciler(node: MockNode) -> ReserveReconciler<MockNode> {
    let config = node.indexer_config();
    ReserveReconciler::new(node, config, 0)
}

/// `MockNode::indexer_config` uses `confirmation_depth: 12` and the mock
/// head is 100, so reads land at block 89.
const EXPECTED_READ_BLOCK: u64 = 89;

fn skipped_reason(outcome: &ReserveTickOutcome) -> &str {
    match outcome {
        ReserveTickOutcome::Skipped { reason } => reason,
        other => panic!("expected a skip, got {other:?}"),
    }
}

fn reconciled(outcome: &ReserveTickOutcome) -> &ReconciliationReport {
    match outcome {
        ReserveTickOutcome::Reconciled { report, .. } => report,
        other => panic!("expected a reconciliation, got {other:?}"),
    }
}

// ------------------------------------------ the bug this module fixes --

#[tokio::test]
async fn a_funded_bridge_contract_refreshes_the_reserve_from_zero() {
    // The exact production numbers: a bridge holding 1,000,100 GLC at 18
    // decimals, a reserve row still seeded at 0, a protected minimum of
    // 2,000,000,000,000 canonical units.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();

    // Before: precisely what `glc-admin robinhood-status` printed.
    let (before, protected, _, obligations) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(before, 0);
    assert_eq!(protected, PROTECTED_MINIMUM);
    assert!(before < protected + obligations, "the invariant is broken");

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;

    let report = reconciled(&outcome);
    assert_eq!(report.direction, ReserveDirection::RobinhoodReserve);
    assert_eq!(report.cached_balance_before, 0);
    // 1,000,100 GLC in canonical 8-decimal units.
    assert_eq!(report.observed_balance, 1_000_100 * ONE_GLC_CANONICAL);
    assert_eq!(report.observed_balance, 100_010_000_000_000);
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);

    // After: the row carries the observed balance, and the invariant the
    // status command reported as false now holds.
    let (after, protected, _, obligations) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(after, 100_010_000_000_000);
    assert!(after >= protected + obligations, "the invariant now holds");
    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
}

#[tokio::test]
async fn the_balance_is_read_for_the_configured_bridge_on_the_configured_token() {
    // Requirement: `expected_token` and `bridge_contract`, both from
    // `[robinhood.indexer]`. A holder that is not the bridge must not be
    // what gets reconciled, which is the mistake a positional
    // (token, holder) pair invites.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 5 * ONE_GLC_ROBINHOOD);
    // A different holder is funded far more heavily; it must be ignored.
    node.set_token_balance(
        crate::robinhood::testkit::DEPOSITOR,
        900 * ONE_GLC_ROBINHOOD,
    );
    let mut ledger = ledger_with_robinhood_reserve();

    let reconciler = reconciler(node);
    assert_eq!(reconciler.observed_pair(), (TOKEN, BRIDGE));

    let outcome = reconciler.tick(&mut ledger, 1_780_000_000).await;
    assert_eq!(reconciled(&outcome).observed_balance, 5 * ONE_GLC_CANONICAL);
}

#[tokio::test]
async fn the_balance_is_read_at_confirmation_depth_not_at_the_head() {
    // A head-block balance can be reorged away; reconciling against one
    // would let a reorg present as an unexplained drop and auto-pause a
    // reserve that never lost anything.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 3 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    match outcome {
        ReserveTickOutcome::Reconciled { block, .. } => {
            // head 100, confirmation_depth 12 => head + 1 - depth = 89,
            // the same `head - block + 1` arithmetic the deposit indexer
            // uses.
            assert_eq!(block, EXPECTED_READ_BLOCK);
        }
        other => panic!("expected a reconciliation, got {other:?}"),
    }
}

// ------------------------------------------------- fail-closed reading --

#[tokio::test]
async fn a_failed_balance_read_does_not_overwrite_the_cached_balance() {
    // The core fail-closed property. Reconcile once against a real
    // balance, then break the endpoint: the cached figure must survive
    // untouched. A reader that zeroed or re-guessed here would turn a
    // transient RPC outage into a fabricated insolvency and an automatic
    // pause.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();
    let reconciler = reconciler(node);

    let outcome = reconciler.tick(&mut ledger, 1_780_000_000).await;
    assert_eq!(reconciled(&outcome).observed_balance, 100_010_000_000_000);

    reconciler.rpc.fail_calls("connection reset by peer");
    let outcome = reconciler.tick(&mut ledger, 1_780_000_060).await;

    assert!(skipped_reason(&outcome).contains("connection reset by peer"));
    let (after, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(
        after, 100_010_000_000_000,
        "the cached balance must be exactly what the last SUCCESSFUL read observed"
    );
    // And a failed read never pauses: nothing was observed, so nothing
    // was breached.
    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());

    // Recovery re-reads for real rather than staying stuck.
    reconciler.rpc.heal_calls();
    reconciler
        .rpc
        .set_token_balance(BRIDGE, 900_000 * ONE_GLC_ROBINHOOD);
    let outcome = reconciler.tick(&mut ledger, 1_780_000_120).await;
    assert_eq!(
        reconciled(&outcome).observed_balance,
        900_000 * ONE_GLC_CANONICAL
    );
}

#[tokio::test]
async fn a_skipped_tick_is_recorded_as_an_auditable_finding() {
    // A skip that left no trace would be indistinguishable from a tick
    // that never ran.
    let node = MockNode::new(BRIDGE);
    node.fail_calls("timed out");
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    assert!(skipped_reason(&outcome).contains("timed out"));

    let findings = ledger.reconciliation_findings_page(None, None, 10).unwrap();
    let skip = findings
        .iter()
        .find(|f| f.classification.starts_with("SKIPPED:"))
        .expect("the skip is recorded");
    assert_eq!(skip.direction, ReserveDirection::RobinhoodReserve);
    assert!(skip.classification.contains("timed out"));
    // A skip asserts no balance.
    assert_eq!(skip.observed, 0);
    assert_eq!(skip.delta, 0);
}

#[tokio::test]
async fn an_unreachable_endpoint_skips_rather_than_reconciling_zero() {
    // `eth_blockNumber` failing is a different failure from `eth_call`
    // failing, and both must end in a skip rather than in a zero.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    node.fail_head("no route to host");
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    assert!(skipped_reason(&outcome).contains("no route to host"));
    let (after, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(after, 0, "still the seeded value — nothing was invented");
}

#[tokio::test]
async fn a_repointed_endpoint_on_another_chain_is_refused_every_tick() {
    // The chain id is checked on EVERY tick, not once at startup: an
    // endpoint can be repointed underneath a running process, and
    // reconciling this reserve against another chain's balance is the
    // one way a read-only component could still cause a loss.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut config = node.indexer_config();
    config = crate::robinhood::RobinhoodIndexerConfig::new(
        config.rpc_url,
        crate::evm::EvmChainId::new(1).unwrap(), // not 4663
        config.bridge_contract,
        config.expected_token,
        config.start_block,
        config.confirmation_depth,
        config.poll_interval_ms,
        config.request_timeout_ms,
        config.max_log_block_range,
    )
    .unwrap();
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = ReserveReconciler::new(node, config, 0)
        .tick(&mut ledger, 1_780_000_000)
        .await;

    let reason = skipped_reason(&outcome);
    assert!(reason.contains("chain id 4663"), "{reason}");
    assert!(reason.contains("refusing to reconcile"), "{reason}");
    let (after, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(after, 0);
}

#[tokio::test]
async fn a_token_that_is_not_eighteen_decimal_is_refused() {
    // The amount model takes no decimals parameter, so this is the one
    // place the assumption behind every conversion is checked against
    // the chain. A 6-decimal token's balance is not this reserve's
    // balance, and treating it as one would mis-scale by 10^12.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    node.with(|s| s.contract.token_decimals = 6);
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    assert!(skipped_reason(&outcome).contains("decimals"));
    let (after, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(after, 0);
}

// --------------------------------------------------- breach and pause --

#[tokio::test]
async fn an_observed_balance_below_the_protected_minimum_breaches_and_pauses() {
    // A REAL read, showing a real shortfall. This must pause, through
    // the existing `reconciliation::reconcile` path, with the existing
    // hard-invariant reason.
    let node = MockNode::new(BRIDGE);
    // 19,999 GLC against a 20,000 GLC protected minimum.
    node.set_token_balance(BRIDGE, 19_999 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();
    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;

    let report = reconciled(&outcome);
    assert_eq!(report.observed_balance, 19_999 * ONE_GLC_CANONICAL);
    assert!(report.observed_balance < report.protected_minimum);
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
    assert!(ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());

    // The balance is still refreshed on a breach — pausing is not a
    // reason to keep reporting a stale figure.
    let (after, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(after, 19_999 * ONE_GLC_CANONICAL);
}

#[tokio::test]
async fn a_recovered_balance_never_auto_unpauses() {
    // Asymmetric pause: fast and automatic to pause, slow and manual to
    // resume. A transient dip that recovers must not silently resume
    // settlement.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 19_999 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();
    let reconciler = reconciler(node);

    let outcome = reconciler.tick(&mut ledger, 1_780_000_000).await;
    assert!(reconciled(&outcome).auto_paused);
    assert!(ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());

    // Refunded well above the floor.
    reconciler
        .rpc
        .set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let outcome = reconciler.tick(&mut ledger, 1_780_000_060).await;
    let report = reconciled(&outcome);
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);

    assert!(
        ledger
            .is_paused(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        "reconciliation must never resume a paused reserve — that is operator-only"
    );
}

#[tokio::test]
async fn an_unexplained_drop_beyond_tolerance_breaches() {
    // The second half of the reconciliation contract, reached through
    // the shared path rather than reimplemented here.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();
    let reconciler = reconciler(node);

    reconciler.tick(&mut ledger, 1_780_000_000).await;
    // Still comfortably above the protected minimum, so only the
    // unexplained-drop arm can fire.
    reconciler
        .rpc
        .set_token_balance(BRIDGE, 500_000 * ONE_GLC_ROBINHOOD);
    let outcome = reconciler.tick(&mut ledger, 1_780_000_060).await;

    let report = reconciled(&outcome);
    assert!(report.observed_balance > report.protected_minimum);
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}

// ------------------------------------------------------------- dust --

#[tokio::test]
async fn sub_canonical_dust_floors_reconciles_and_is_recorded() {
    // Anyone may transfer one wei of GLC to the bridge, for free. That
    // must not freeze reconciliation — but it must not pass unremarked
    // either.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD + 1);
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;

    match &outcome {
        ReserveTickOutcome::Reconciled {
            report,
            dust_remainder,
            ..
        } => {
            assert_eq!(*dust_remainder, 1);
            // The FLOOR — an understatement, which is the pause-safe
            // direction — not a rounded-up figure.
            assert_eq!(report.observed_balance, 1_000_100 * ONE_GLC_CANONICAL);
        }
        other => panic!("dust must not stop reconciliation, got {other:?}"),
    }

    let findings = ledger.reconciliation_findings_page(None, None, 10).unwrap();
    let note = findings
        .iter()
        .find(|f| f.classification.starts_with("NOTE:"))
        .expect("the dust is recorded as an auditable note");
    assert!(note.classification.contains("sub-canonical dust"));
    assert!(note.classification.contains("remainder 1"));
}

#[tokio::test]
async fn dust_just_below_a_whole_canonical_unit_still_only_floors() {
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(
        BRIDGE,
        1_000_100 * ONE_GLC_ROBINHOOD + (CANONICAL_TO_ROBINHOOD_SCALE - 1),
    );
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    match &outcome {
        ReserveTickOutcome::Reconciled {
            report,
            dust_remainder,
            ..
        } => {
            assert_eq!(*dust_remainder, CANONICAL_TO_ROBINHOOD_SCALE - 1);
            assert_eq!(report.observed_balance, 1_000_100 * ONE_GLC_CANONICAL);
        }
        other => panic!("expected a reconciliation, got {other:?}"),
    }
}

// -------------------------------------------- configuration boundaries --

#[tokio::test]
async fn no_settlement_configuration_is_required_to_reconcile() {
    // The whole point of requirement 8, asserted structurally rather
    // than by comment: the reconciler is constructed from the indexer
    // config alone. There is no settlement config, no submitter key, no
    // authorization signer and no verified deployment anywhere in this
    // test, and reconciliation completes against a real balance.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let indexer_config = node.indexer_config();
    let mut ledger = ledger_with_robinhood_reserve();

    let outcome = ReserveReconciler::new(node, indexer_config, 0)
        .tick(&mut ledger, 1_780_000_000)
        .await;

    assert_eq!(reconciled(&outcome).observed_balance, 100_010_000_000_000);
    let (after, _, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(after, 100_010_000_000_000);
}

#[tokio::test]
async fn reconciling_never_opens_a_robinhood_route() {
    // Reconciliation observes a balance. It must not be a back door to
    // enabling a route, and a funded, solvent, unpaused reserve must not
    // change that.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();

    // `legacy_only` is the resolved state of an unmodified production
    // deployment: the two Goldcoin<->Solana routes open, all four
    // Robinhood routes closed. That is production's state today.
    let gate = RouteGate::legacy_only();
    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        assert!(!gate.is_enabled(&ledger, route), "{route:?} starts closed");
    }

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    let report = reconciled(&outcome);
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);

    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        assert!(
            !gate.is_enabled(&ledger, route),
            "{route:?} must still be closed after reconciliation"
        );
    }
}

#[tokio::test]
async fn an_unconfigured_reserve_is_reported_rather_than_created() {
    // No `[reserve.robinhood]` means no row, and the fail-closed default
    // is "this reserve does not exist", never "this reserve is empty".
    // Reconciliation must not conjure the row into being, or an operator
    // who deliberately left the reserve unconfigured would find one.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_without_robinhood_reserve();

    let outcome = reconciler(node).tick(&mut ledger, 1_780_000_000).await;
    assert!(matches!(outcome, ReserveTickOutcome::NotConfigured));

    assert!(matches!(
        ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve),
        Err(crate::ledger::LedgerError::ReserveNotInitialized(_))
    ));
    // Not even a finding: there is no reserve to make a finding about.
    let findings = ledger.reconciliation_findings_page(None, None, 10).unwrap();
    assert!(findings.is_empty(), "{findings:?}");
}

#[tokio::test]
async fn the_goldcoin_and_solana_reserves_are_never_touched() {
    // Robinhood reconciliation writes to exactly one direction. The
    // other two reserves' rows, pause state and findings must be
    // byte-for-byte unaffected — they have their own reconcilers.
    let node = MockNode::new(BRIDGE);
    node.set_token_balance(BRIDGE, 1_000_100 * ONE_GLC_ROBINHOOD);
    let mut ledger = ledger_with_robinhood_reserve();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        ledger
            .configure_reserve(direction, 777_000, 1_000, 8_000, 4_000, 2_000, 0)
            .unwrap();
    }

    reconciler(node).tick(&mut ledger, 1_780_000_000).await;

    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        let (balance, protected, _, _) = ledger.reserve_snapshot(direction).unwrap();
        assert_eq!(balance, 777_000, "{direction:?} balance untouched");
        assert_eq!(protected, 1_000, "{direction:?} bounds untouched");
        assert!(!ledger.is_paused(direction).unwrap());
    }
    let findings = ledger.reconciliation_findings_page(None, None, 20).unwrap();
    assert!(
        findings
            .iter()
            .all(|f| f.direction == ReserveDirection::RobinhoodReserve),
        "only the Robinhood direction may be written: {findings:?}"
    );
}
