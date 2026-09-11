//! `robinhood-reserve-init` against a mock node and an in-memory ledger:
//! the row it creates, the figure it seeds, and every refusal.

use super::*;

use crate::evm::EvmAddress;
use crate::ledger::RebalanceKind;
use crate::robinhood::testkit::{MockNode, TOKEN};

const NOW: i64 = 1_790_000_000;
/// 200 GLC in the token's 18-decimal unit.
const TWO_HUNDRED_GLC_18DP: u128 = 200 * 10u128.pow(18);
/// The same 200 GLC in the ledger's canonical 8-decimal unit.
const TWO_HUNDRED_GLC_CANONICAL: u64 = 200 * 100_000_000;

fn bridge() -> EvmAddress {
    EvmAddress::from_bytes([0xb2; 20])
}

/// A node whose bridge holds exactly 200 GLC — the successor deployment,
/// funded with a test reserve and nothing else.
fn funded_node() -> MockNode {
    let node = MockNode::new(bridge());
    node.set_token_balance(bridge(), TWO_HUNDRED_GLC_18DP);
    node
}

fn bounds() -> ReserveBounds {
    ReserveBounds {
        protected_minimum: 0,
        critical_reserve: 1,
        warning_reserve: 2,
        target_reserve: 3,
    }
}

fn ledger() -> Ledger {
    Ledger::open_in_memory().expect("an in-memory ledger")
}

async fn run_against(
    node: &MockNode,
    ledger: &mut Ledger,
    bounds: Option<ReserveBounds>,
) -> Result<ReserveInitOutcome, ReserveInitError> {
    run(
        node,
        ledger,
        &node.indexer_config(),
        &node.settlement_config(),
        bounds,
        NOW,
    )
    .await
}

// =====================================================================
// The happy path: a fresh ledger, the chain's figure, and nothing else
// =====================================================================

#[tokio::test]
async fn a_fresh_ledger_gets_a_row_seeded_with_exactly_the_onchain_balance() {
    let node = funded_node();
    let mut ledger = ledger();
    assert!(
        matches!(
            ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve),
            Err(LedgerError::ReserveNotInitialized(_))
        ),
        "no row before"
    );

    let outcome = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("a fresh ledger initializes");
    let ReserveInitOutcome::Initialized(report) = outcome else {
        panic!("expected Initialized, got {outcome:?}");
    };
    assert_eq!(report.bridge, bridge());
    assert_eq!(report.token, TOKEN);
    assert_eq!(
        report.observed_robinhood_atomic,
        EvmU256::from_u128(TWO_HUNDRED_GLC_18DP)
    );
    assert_eq!(report.balance_canonical.0, TWO_HUNDRED_GLC_CANONICAL);
    assert_eq!(report.bounds, bounds());

    // The row says exactly the chain's figure, the config's floor, and
    // zero everywhere else.
    let (balance, protected, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(balance, TWO_HUNDRED_GLC_CANONICAL, "200 GLC canonical");
    assert_eq!(protected, 0);
    assert_eq!(reserved, 0);
    assert_eq!(pending, 0);
    assert_eq!(
        ledger
            .accrued_fees(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        0
    );
    // No other reserve was touched.
    assert!(matches!(
        ledger.reserve_snapshot(ReserveDirection::GoldcoinReserve),
        Err(LedgerError::ReserveNotInitialized(_))
    ));
    assert!(matches!(
        ledger.reserve_snapshot(ReserveDirection::SolanaReserve),
        Err(LedgerError::ReserveNotInitialized(_))
    ));
    // Nothing was broadcast, and the node saw only reads.
    assert!(node.with(|s| s.broadcasts.is_empty()));
}

/// The protected minimum comes from the config, verbatim, and the
/// threshold band is installed with it — the same columns the daemon
/// writes from the same section.
#[tokio::test]
async fn the_protected_minimum_and_bands_come_from_the_config() {
    let node = funded_node();
    let mut ledger = ledger();
    let bounds = ReserveBounds {
        protected_minimum: 10 * 100_000_000,
        critical_reserve: 20 * 100_000_000,
        warning_reserve: 50 * 100_000_000,
        target_reserve: 150 * 100_000_000,
    };
    run_against(&node, &mut ledger, Some(bounds))
        .await
        .expect("initializes");
    let (balance, protected, _, _) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(balance, TWO_HUNDRED_GLC_CANONICAL);
    assert_eq!(protected, 10 * 100_000_000);
    let (_, protected_minimum, target, warning, critical) = ledger
        .reserve_thresholds(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(protected_minimum, 10 * 100_000_000);
    assert_eq!(target, 150 * 100_000_000);
    assert_eq!(warning, 50 * 100_000_000);
    assert_eq!(critical, 20 * 100_000_000);
}

/// A zero balance is a legitimate baseline for a deployment that has not
/// been funded yet; it is not an error.
#[tokio::test]
async fn an_unfunded_bridge_seeds_zero() {
    let node = MockNode::new(bridge());
    let mut ledger = ledger();
    let outcome = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("initializes at zero");
    assert_eq!(outcome.report().balance_canonical.0, 0);
    let (balance, ..) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(balance, 0);
}

// =====================================================================
// Idempotency: the same command twice writes nothing the second time
// =====================================================================

#[tokio::test]
async fn a_second_run_against_an_identical_row_is_a_no_op() {
    let node = funded_node();
    let mut ledger = ledger();
    run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("first run");
    let (before, ..) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();

    let outcome = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("second run is accepted");
    assert!(
        matches!(outcome, ReserveInitOutcome::AlreadyInitialized(_)),
        "{outcome:?}"
    );
    let (after, ..) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(before, after);
}

/// A row whose balance no longer matches the chain is a ledger with a
/// history, not a baseline to overwrite. Refused, with both figures.
#[tokio::test]
async fn an_existing_row_with_a_different_balance_is_refused() {
    let node = funded_node();
    let mut ledger = ledger();
    run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("first run");
    // The chain moves on (say, a withdrawal the ledger has not accounted).
    node.set_token_balance(bridge(), 150 * 10u128.pow(18));

    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("differs");
    assert!(
        matches!(
            err,
            ReserveInitError::RowExistsAndDiffers {
                balance,
                observed,
                ..
            } if balance == TWO_HUNDRED_GLC_CANONICAL && observed == 150 * 100_000_000
        ),
        "{err}"
    );
    // And the row still says what it said.
    let (balance, ..) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(balance, TWO_HUNDRED_GLC_CANONICAL);
}

/// A different configured floor is a different baseline too.
#[tokio::test]
async fn an_existing_row_with_a_different_protected_minimum_is_refused() {
    let node = funded_node();
    let mut ledger = ledger();
    run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("first run");
    let raised = ReserveBounds {
        protected_minimum: 5 * 100_000_000,
        critical_reserve: 6 * 100_000_000,
        ..bounds()
    };
    let err = run_against(&node, &mut ledger, Some(raised))
        .await
        .expect_err("different floor");
    assert!(
        matches!(
            err,
            ReserveInitError::RowExistsAndDiffers {
                protected_minimum: 0,
                configured_minimum,
                ..
            } if configured_minimum == 5 * 100_000_000
        ),
        "{err}"
    );
}

// =====================================================================
// The production-ledger guard: any Robinhood history refuses the baseline
// =====================================================================

/// A ledger that has accounted a Robinhood operation — here, a rebalance
/// request against the reserve — cannot honestly be baselined with zero
/// reserved and zero pending. This is what stops the command from being
/// pointed at the production ledger by mistake: that ledger has a row
/// (refused above) AND activity (refused here).
#[tokio::test]
async fn a_ledger_with_robinhood_activity_is_refused_before_any_write() {
    let node = funded_node();
    let mut ledger = ledger();
    // A rebalance needs a configured reserve to be proposed against, so
    // the history is created on top of a matching row. That also proves
    // the activity guard fires even when the row would otherwise have
    // been accepted as identical: activity is checked FIRST.
    run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect("baseline");
    ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            100_000_000,
            "a real operation",
            "ops:alice",
            2,
            NOW,
        )
        .expect("a rebalance is proposable against a configured reserve");

    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("activity");
    let detail = err.to_string();
    assert!(
        matches!(err, ReserveInitError::LedgerHasRobinhoodActivity { .. }),
        "{detail}"
    );
    assert!(detail.contains("1 rebalance request(s)"), "{detail}");
    assert!(detail.contains("isolated one, not this"), "{detail}");
}

// =====================================================================
// Fail closed on the chain and the config
// =====================================================================

#[tokio::test]
async fn an_rpc_failure_writes_nothing() {
    let node = funded_node();
    node.with(|s| s.call_failure = Some("connection reset by peer".to_string()));
    let mut ledger = ledger();
    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("rpc down");
    assert!(
        matches!(err, ReserveInitError::Preflight(_)),
        "the preflight is the first thing that reads the chain: {err}"
    );
    assert!(matches!(
        ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve),
        Err(LedgerError::ReserveNotInitialized(_))
    ));
}

#[tokio::test]
async fn a_bridge_custodying_a_different_token_than_configured_is_refused() {
    let node = funded_node();
    node.with(|s| s.contract.token = EvmAddress::from_bytes([0xee; 20]));
    let mut ledger = ledger();
    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("wrong token");
    assert!(
        matches!(
            err,
            ReserveInitError::Preflight(PreflightError::WrongToken { .. })
        ),
        "{err}"
    );
    assert!(matches!(
        ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve),
        Err(LedgerError::ReserveNotInitialized(_))
    ));
}

#[tokio::test]
async fn an_address_with_no_contract_is_refused() {
    let node = funded_node();
    node.with(|s| s.contract.bridge_code.clear());
    let mut ledger = ledger();
    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("no code");
    assert!(
        matches!(
            err,
            ReserveInitError::Preflight(PreflightError::NoContractCode { .. })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_contract_of_another_protocol_family_is_refused() {
    let node = funded_node();
    node.with(|s| s.contract.protocol_id = [0xab; 32]);
    let mut ledger = ledger();
    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("wrong family");
    assert!(
        matches!(
            err,
            ReserveInitError::Preflight(PreflightError::WrongProtocol { .. })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn a_missing_reserve_section_is_refused_before_the_chain_is_read() {
    let node = funded_node();
    let mut ledger = ledger();
    let err = run_against(&node, &mut ledger, None)
        .await
        .expect_err("no bounds");
    assert!(matches!(err, ReserveInitError::NoReserveBounds), "{err}");
    assert!(
        node.with(|s| s.calls.is_empty()),
        "nothing was read from the chain"
    );
}

#[tokio::test]
async fn a_critical_reserve_at_or_below_the_floor_is_refused() {
    let node = funded_node();
    let mut ledger = ledger();
    let bad = ReserveBounds {
        protected_minimum: 5,
        critical_reserve: 5,
        warning_reserve: 6,
        target_reserve: 7,
    };
    let err = run_against(&node, &mut ledger, Some(bad))
        .await
        .expect_err("bad bounds");
    assert!(
        matches!(
            err,
            ReserveInitError::BoundsInvalid {
                protected_minimum: 5,
                critical: 5
            }
        ),
        "{err}"
    );
}

/// A balance the 8-decimal ledger cannot carry exactly is refused rather
/// than rounded — the ledger must never claim a figure the chain does
/// not hold.
#[tokio::test]
async fn a_non_canonical_balance_is_refused_rather_than_rounded() {
    let node = funded_node();
    node.set_token_balance(bridge(), TWO_HUNDRED_GLC_18DP + 1);
    let mut ledger = ledger();
    let err = run_against(&node, &mut ledger, Some(bounds()))
        .await
        .expect_err("dust");
    assert!(
        matches!(err, ReserveInitError::NotCanonical { .. }),
        "{err}"
    );
    assert!(matches!(
        ledger.reserve_snapshot(ReserveDirection::RobinhoodReserve),
        Err(LedgerError::ReserveNotInitialized(_))
    ));
}
