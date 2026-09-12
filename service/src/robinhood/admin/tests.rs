//! Operator inspection and recovery.
//!
//! The shape every test here checks is the module's own rule: an
//! assessment is a PREVIEW of an execution, never a precondition for it,
//! and the execution re-runs every check itself. So the interesting cases
//! are the ones where the two could disagree — and must not.

use super::*;
use crate::ledger::{ReserveDirection, RobinhoodTxKind, RobinhoodTxState};
use crate::robinhood::calls::{Obligation, OBLIGATION_STATUS_PENDING, OBLIGATION_STATUS_SETTLED};
use crate::robinhood::settlement::Settler;
use crate::robinhood::signer::{DevEvmAuthSigner, EvmAuthSigner};
use crate::robinhood::testkit::{signer_key, submitter_key, MockNode, BRIDGE, DEPOSITOR};
use crate::robinhood::Submitter;
use std::str::FromStr;
use std::time::Duration;

const PRINCIPAL_18DP: u128 = 500_000_000_000_000_000_000; // 500 GLC
const PRINCIPAL_CANONICAL: u64 = 50_000_000_000; // 500 GLC at 8dp

fn ledger() -> Ledger {
    Ledger::open_in_memory().expect("an in-memory ledger")
}

fn settler(node: &MockNode) -> Settler<MockNode> {
    let config = node.settlement_config();
    let signers: Vec<Box<dyn EvmAuthSigner>> = (0..3)
        .map(|i| Box::new(DevEvmAuthSigner::new(signer_key(i))) as Box<dyn EvmAuthSigner>)
        .collect();
    Settler::new(
        node.clone(),
        Submitter::from_key(submitter_key(), &config).unwrap(),
        signers,
        node.verified_deployment(),
        config,
        Duration::from_secs(5),
        crate::goldcoin::address::Network::Testnet,
        6,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
}

/// An `RhnToGlc` request parked in `ManualReview` — the state a refund
/// begins from.
fn parked_request(ledger: &Ledger, obligation_index: u64) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                 source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, manual_review_note)
             VALUES ('RhnToGlc', 'ManualReview', ?1, 300, 1500000000, ?2, ?2, X'6162', 100,
                     'robinhood', ?3, ?4, 1, 100, 'route closed at fold')",
            rusqlite::params![
                PRINCIPAL_CANONICAL as i64,
                (PRINCIPAL_CANONICAL - 1_500_000_000) as i64,
                &BRIDGE.to_bytes()[..],
                obligation_index as i64,
            ],
        )
        .expect("seeds a parked request");
    ledger.conn_for_tests().last_insert_rowid()
}

fn obligation(node: &MockNode, index: u64, status: u8) {
    node.with(|s| {
        s.contract.obligation_count = s.contract.obligation_count.max(index + 1);
        s.contract.obligations.insert(
            index,
            Obligation {
                depositor: DEPOSITOR,
                status,
                route: 0x02,
                amount: EvmU256::from_u128(PRINCIPAL_18DP),
            },
        );
    });
}

fn named(checks: &[Check], name: &str) -> Check {
    checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check named {name} in {checks:#?}"))
        .clone()
}

// ------------------------------------------------------ manual review --

#[test]
fn the_manual_review_queue_lists_parked_rhn_to_glc_requests() {
    let ledger = ledger();
    let a = parked_request(&ledger, 1);
    let b = parked_request(&ledger, 2);

    let queue = manual_review_queue(&ledger).expect("reads");
    assert_eq!(queue.len(), 2);
    let ids: Vec<i64> = queue.iter().map(|i| i.request_id).collect();
    assert!(ids.contains(&a) && ids.contains(&b));

    let item = &queue[0];
    assert_eq!(item.obligation_index, Some(1));
    assert_eq!(item.gross_amount_atomic, PRINCIPAL_CANONICAL);
    assert_eq!(item.reason.as_deref(), Some("route closed at fold"));
    // Neither answer has been given yet.
    assert!(!item.has_refund);
    assert!(!item.has_settlement);
    assert_eq!(item.operation_state, None);
}

/// A `GlcToRhn` request parks for Goldcoin-side reasons and is served by
/// the existing Goldcoin tooling. Listing it here would invite an
/// operator to reach for a Robinhood refund for a deposit that never
/// touched Robinhood.
#[test]
fn the_queue_is_scoped_to_the_inbound_route() {
    let ledger = ledger();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain)
             VALUES ('GlcToRhn', 'ManualReview', 100, 300, 3, 97, 97, X'6162', 1, 'goldcoin')",
            [],
        )
        .unwrap();
    assert!(manual_review_queue(&ledger).unwrap().is_empty());
}

// ------------------------------------------------ refund assessment --

#[tokio::test]
async fn an_eligible_refund_passes_every_ledger_check_and_then_executes() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 7);
    obligation(&node, 7, OBLIGATION_STATUS_PENDING);

    let assessment = refund_assessment(&ledger, request_id).expect("assesses");
    assert!(assessment.ledger_eligible, "{:#?}", assessment.checks);
    assert!(assessment.existing_refund.is_none());
    assert_eq!(assessment.obligation_index, Some(7));
    // Stated explicitly rather than implied: the chain half is ahead.
    assert!(named(&assessment.checks, "chain_checks_deferred").ok);

    // The execution re-runs everything itself, reads the obligation back
    // from the chain, and collects a real quorum.
    let tx_id = crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect("an eligible refund begins");

    let views = txs_for_request(&ledger, request_id).expect("reads");
    let refund = views
        .iter()
        .find(|t| t.kind == RobinhoodTxKind::Refund)
        .expect("a refund operation exists");
    assert_eq!(refund.id, tx_id);
    // The recipient and amount came from the OBLIGATION, not from the
    // ledger row and not from an operator.
    assert_eq!(
        refund.recipient.as_deref(),
        Some(DEPOSITOR.to_checksum_string().as_str())
    );
    assert_eq!(
        refund.amount_robinhood_atomic.as_deref(),
        Some(PRINCIPAL_18DP.to_string().as_str())
    );
    assert_eq!(
        refund.signatures_collected,
        crate::robinhood::SIGNER_THRESHOLD
    );

    // And the assessment now reports the existing operation rather than
    // inviting a second one.
    let again = refund_assessment(&ledger, request_id).expect("assesses");
    assert!(!again.ledger_eligible);
    assert!(again.existing_refund.is_some());
    assert!(!named(&again.checks, "no_refund_already_begun").ok);
}

#[test]
fn a_request_in_the_wrong_state_or_direction_is_refused() {
    let ledger = ledger();
    // Wrong direction.
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain)
             VALUES ('GlcToSol', 'ManualReview', 100, 300, 3, 97, 97, X'6162', 1, 'goldcoin')",
            [],
        )
        .unwrap();
    let wrong_direction = ledger.conn_for_tests().last_insert_rowid();
    let a = refund_assessment(&ledger, wrong_direction).unwrap();
    assert!(!a.ledger_eligible);
    assert!(!named(&a.checks, "direction_is_robinhood_sourced").ok);

    // Right direction, wrong state.
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at,
                 source_chain, source_contract, source_obligation_index)
             VALUES ('RhnToGlc', 'Settled', 100, 300, 3, 97, 97, X'6162', 1, 'robinhood', ?1, 3)",
            rusqlite::params![&BRIDGE.to_bytes()[..]],
        )
        .unwrap();
    let settled = ledger.conn_for_tests().last_insert_rowid();
    let b = refund_assessment(&ledger, settled).unwrap();
    assert!(!b.ledger_eligible);
    assert!(!named(&b.checks, "state_is_manual_review").ok);

    // A request that does not exist at all.
    let c = refund_assessment(&ledger, 99_999).unwrap();
    assert!(!c.ledger_eligible);
    assert!(!named(&c.checks, "request_exists").ok);
}

/// THE failure that loses real money: refunding a deposit already
/// settled. Both the assessment and the execution must refuse, and the
/// execution must not depend on the assessment having run.
#[tokio::test]
async fn a_settled_request_is_refused_by_both_the_assessment_and_the_execution() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 9);
    obligation(&node, 9, OBLIGATION_STATUS_PENDING);

    // A settlement operation exists for this request.
    ledger
        .begin_robinhood_tx(
            &crate::ledger::NewRobinhoodTx {
                kind: RobinhoodTxKind::Settlement,
                request_id: Some(request_id),
                rebalance_request_id: None,
                route: Some(Route::RhnToGlc),
                bridge_contract: BRIDGE.to_bytes(),
                chain_id: 4663,
                contract_request_id: [0x44; 32],
                obligation_index: Some(9),
                recipient: None,
                amount_robinhood: None,
                signer_epoch: 7,
                expiry: 9_999_999_999,
                auth_digest: [0x55; 32],
            },
            10,
        )
        .expect("seeds a settlement");

    let assessment = refund_assessment(&ledger, request_id).unwrap();
    assert!(!assessment.ledger_eligible);
    assert!(!named(&assessment.checks, "no_settlement_exists").ok);

    // The execution refuses independently — it does not consult the
    // assessment.
    let err = crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect_err("a settled request must never be refunded");
    assert!(
        matches!(err, crate::robinhood::RefundError::AlreadySettled { .. }),
        "{err:?}"
    );
}

/// And the mirror: a refunded deposit is never settled.
#[test]
fn the_settlement_assessment_refuses_a_request_that_has_a_refund() {
    let ledger = ledger();
    let request_id = parked_request(&ledger, 11);
    let mut ledger = ledger;
    ledger
        .begin_robinhood_tx(
            &crate::ledger::NewRobinhoodTx {
                kind: RobinhoodTxKind::Refund,
                request_id: Some(request_id),
                rebalance_request_id: None,
                route: Some(Route::RhnToGlc),
                bridge_contract: BRIDGE.to_bytes(),
                chain_id: 4663,
                contract_request_id: [0x66; 32],
                obligation_index: Some(11),
                recipient: Some(DEPOSITOR.to_bytes()),
                amount_robinhood: Some(EvmU256::from_u128(PRINCIPAL_18DP).to_be_bytes()),
                signer_epoch: 7,
                expiry: 9_999_999_999,
                auth_digest: [0x77; 32],
            },
            10,
        )
        .expect("seeds a refund");

    let assessment = settlement_assessment(&ledger, request_id).unwrap();
    assert!(!assessment.ledger_eligible);
    assert!(!named(&assessment.checks, "no_refund_exists").ok);
    // And the ordering property: a settlement is only reachable from
    // DestinationConfirmed, and this request is in ManualReview.
    assert!(!named(&assessment.checks, "goldcoin_payout_confirmed").ok);
}

/// An on-chain obligation that is no longer Pending is refused by the
/// EXECUTION, which is the only layer that can see it — and the
/// assessment says so rather than implying it checked.
#[tokio::test]
async fn an_obligation_that_is_not_pending_is_refused_at_execution() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 13);
    obligation(&node, 13, OBLIGATION_STATUS_SETTLED);

    // The ledger has no objection...
    assert!(
        refund_assessment(&ledger, request_id)
            .unwrap()
            .ledger_eligible
    );
    // ...and the chain does.
    let err = crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect_err("only a Pending obligation is refundable");
    assert!(
        matches!(
            err,
            crate::robinhood::RefundError::ObligationNotPending { .. }
        ),
        "{err:?}"
    );
}

// ------------------------------------------------ tx / nonce inspection --

#[tokio::test]
async fn operations_and_the_submitter_nonce_are_inspectable() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 21);
    obligation(&node, 21, OBLIGATION_STATUS_PENDING);
    let settler = settler(&node);
    crate::robinhood::begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .expect("begins");

    // Authorized but not yet broadcast: no nonce, no raw bytes.
    let open = open_operations(&ledger).expect("reads");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].state, RobinhoodTxState::Authorized);
    assert_eq!(open[0].nonce, None);
    assert!(!open[0].has_raw_tx);
    assert!(stalled_operations(&ledger).unwrap().is_empty());

    let state = submitter_state(&ledger, settler.submitter_address(), 4663).expect("reads");
    assert_eq!(
        state.submitter,
        settler.submitter_address().to_checksum_string()
    );
    // Nothing holds a nonce yet.
    assert_eq!(state.highest_allocated_nonce, None);
    assert!(state.in_flight.is_empty());

    // After broadcasting, the operation holds a nonce and the signed
    // bytes are persisted.
    let mut report = crate::robinhood::SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let state = submitter_state(&ledger, settler.submitter_address(), 4663).expect("reads");
    assert_eq!(state.in_flight.len(), 1);
    assert_eq!(state.highest_allocated_nonce, state.in_flight[0].nonce);
    assert!(state.in_flight[0].has_raw_tx);
    assert!(state.in_flight[0].tx_hash.is_some());

    // The allocated nonce survives the operation resolving. A maximum
    // taken over unresolved rows alone would drop back to `None` here and
    // show a nonce gap that does not exist.
    let allocated = state
        .highest_allocated_nonce
        .expect("a nonce was allocated");
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_transactions
                SET state = 'Finalized', finalized_at = 2000, receipt_status = 1,
                    receipt_block_number = 10, receipt_block_hash = ?1,
                    confirmations = 12
             WHERE nonce IS NOT NULL",
            rusqlite::params![&[0xabu8; 32][..]],
        )
        .unwrap();
    let after = submitter_state(&ledger, settler.submitter_address(), 4663).expect("reads");
    assert!(after.in_flight.is_empty());
    assert_eq!(after.highest_allocated_nonce, Some(allocated));
}

// ------------------------------------------------------ halt clearance --

fn halt(ledger: &mut Ledger, reason: RobinhoodHaltReason) {
    ledger
        .robinhood_record_halt(reason, "seeded by a test", 500)
        .expect("records a halt");
}

#[test]
fn clearing_an_unhalted_indexer_is_refused_rather_than_a_silent_no_op() {
    let mut ledger = ledger();
    let clearance = HaltClearance {
        expect_reason: Some(RobinhoodHaltReason::ChainIdMismatch),
        ..HaltClearance::default()
    };
    let checks = halt_clear_assessment(&ledger, &clearance).unwrap();
    assert!(!named(&checks, "indexer_is_halted").ok);
    let err = clear_halt(&mut ledger, &clearance, 600).expect_err("nothing to clear");
    assert!(
        matches!(
            err,
            HaltClearErrorOrLedger::Refused(HaltClearError::NotHalted)
        ),
        "{err:?}"
    );
}

/// The check that stops "make the red light go away": an operator who has
/// not diagnosed the halt cannot clear it.
#[test]
fn a_halt_cannot_be_cleared_without_naming_its_reason() {
    let mut ledger = ledger();
    halt(&mut ledger, RobinhoodHaltReason::ObservationConflict);

    let unnamed = HaltClearance::default();
    assert!(
        !named(
            &halt_clear_assessment(&ledger, &unnamed).unwrap(),
            "reason_named"
        )
        .ok
    );
    let err = clear_halt(&mut ledger, &unnamed, 600).expect_err("no reason named");
    assert!(
        matches!(
            err,
            HaltClearErrorOrLedger::Refused(HaltClearError::ReasonNotNamed { .. })
        ),
        "{err:?}"
    );

    // Naming the WRONG one is also a refusal — the cause has not been
    // diagnosed.
    let wrong = HaltClearance {
        expect_reason: Some(RobinhoodHaltReason::PostFinalityReorg),
        ..HaltClearance::default()
    };
    let err = clear_halt(&mut ledger, &wrong, 600).expect_err("wrong reason named");
    assert!(
        matches!(
            err,
            HaltClearErrorOrLedger::Refused(HaltClearError::ReasonMismatch { .. })
        ),
        "{err:?}"
    );

    // The right one clears it.
    let right = HaltClearance {
        expect_reason: Some(RobinhoodHaltReason::ObservationConflict),
        ..HaltClearance::default()
    };
    clear_halt(&mut ledger, &right, 600).expect("the diagnosed reason clears");
    assert!(halt_state(&ledger).unwrap().halt.is_none());
}

/// A reorg halt means a durable claim was wrong. Clearing it requires
/// reviewing the finalized observations that claim rested on.
#[test]
fn a_reorg_halt_requires_acknowledging_orphaned_finality() {
    for reason in [
        RobinhoodHaltReason::PostFinalityReorg,
        RobinhoodHaltReason::ReorgBeyondRetainedAnchors,
    ] {
        let mut ledger = ledger();
        halt(&mut ledger, reason);
        let unacknowledged = HaltClearance {
            expect_reason: Some(reason),
            ..HaltClearance::default()
        };
        assert!(
            !named(
                &halt_clear_assessment(&ledger, &unacknowledged).unwrap(),
                "orphaned_finality_acknowledged"
            )
            .ok
        );
        let err = clear_halt(&mut ledger, &unacknowledged, 600).expect_err("{reason:?}");
        assert!(
            matches!(
                err,
                HaltClearErrorOrLedger::Refused(
                    HaltClearError::OrphanedFinalityNotAcknowledged { .. }
                )
            ),
            "{err:?}"
        );

        let acknowledged = HaltClearance {
            expect_reason: Some(reason),
            acknowledge_orphaned_finality: true,
            ..HaltClearance::default()
        };
        clear_halt(&mut ledger, &acknowledged, 600).expect("acknowledged clears");
    }
}

/// A chain-id halt means the endpoint was not the configured network.
/// Clearing without re-reading it would simply re-halt on the next tick,
/// so the re-verification is required — and the CLI sets that flag only
/// after a live preflight passes.
#[test]
fn an_endpoint_identity_halt_requires_re_verification() {
    for reason in [
        RobinhoodHaltReason::ChainIdMismatch,
        RobinhoodHaltReason::UnexpectedContractRoute,
    ] {
        let mut ledger = ledger();
        halt(&mut ledger, reason);
        let unverified = HaltClearance {
            expect_reason: Some(reason),
            ..HaltClearance::default()
        };
        assert!(
            !named(
                &halt_clear_assessment(&ledger, &unverified).unwrap(),
                "endpoint_reverified"
            )
            .ok
        );
        let err = clear_halt(&mut ledger, &unverified, 600).expect_err("{reason:?}");
        assert!(
            matches!(
                err,
                HaltClearErrorOrLedger::Refused(HaltClearError::EndpointNotReverified)
            ),
            "{err:?}"
        );

        let verified = HaltClearance {
            expect_reason: Some(reason),
            endpoint_reverified: true,
            ..HaltClearance::default()
        };
        clear_halt(&mut ledger, &verified, 600).expect("re-verified clears");
    }
}

/// An unresolved broadcast is verified against the indexer's view of the
/// chain. A halt is not cleared underneath one.
#[tokio::test]
async fn a_halt_is_not_cleared_while_an_operation_is_in_flight() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 31);
    obligation(&node, 31, OBLIGATION_STATUS_PENDING);
    crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect("begins");
    halt(&mut ledger, RobinhoodHaltReason::ObservationConflict);

    let clearance = HaltClearance {
        expect_reason: Some(RobinhoodHaltReason::ObservationConflict),
        ..HaltClearance::default()
    };
    assert!(
        !named(
            &halt_clear_assessment(&ledger, &clearance).unwrap(),
            "no_operations_in_flight"
        )
        .ok
    );
    let err = clear_halt(&mut ledger, &clearance, 600).expect_err("in flight");
    assert!(
        matches!(
            err,
            HaltClearErrorOrLedger::Refused(HaltClearError::OperationsInFlight { count: 1 })
        ),
        "{err:?}"
    );
}

// ------------------------------------------------------------ reserve --

/// An unconfigured reserve is ABSENT, not empty. The fail-closed default
/// is "this reserve does not exist", and `Some(zeroes)` would say the
/// opposite.
#[test]
fn an_unconfigured_robinhood_reserve_reports_absent_not_empty() {
    let ledger = ledger();
    assert_eq!(reserve_report(&ledger, 1_000).unwrap(), None);
}

#[test]
fn the_robinhood_reserve_is_independent_and_never_netted() {
    let mut ledger = ledger();
    // (direction, initial_balance, protected_minimum, target, warning,
    //  critical, now) — deliberately different figures per reserve, so a
    //  test that accidentally read one for the other would fail.
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000,
            1_000,
            8_000,
            5_000,
            2_000,
            1,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            4_000,
            300,
            3_000,
            1_500,
            600,
            1,
        )
        .unwrap();

    let report = reserve_report(&ledger, 1_000).unwrap().expect("configured");
    // Exactly the Robinhood figures — the Goldcoin balance is not summed
    // into them and does not affect the protected floor.
    assert_eq!(report.balance_atomic, 4_000);
    assert_eq!(report.protected_minimum_atomic, 300);
    assert_eq!(report.reserved_liquidity_atomic, 0);
    assert_eq!(report.pending_obligations_atomic, 0);
    // available = balance - protected_minimum - reserved
    assert_eq!(report.available_capacity_atomic, 3_700);
    assert!(report.invariant_holds);
    assert!(!report.paused);

    // The Goldcoin reserve is untouched by any of it.
    let (goldcoin_balance, goldcoin_floor, _, _) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!(goldcoin_balance, 10_000);
    assert_eq!(goldcoin_floor, 1_000);
}

/// The protected floor is a floor: capacity is what is left ABOVE it, and
/// a balance below it breaks the invariant rather than reporting negative
/// capacity as if it were spendable.
#[test]
fn the_protected_floor_bounds_available_capacity() {
    let mut ledger = ledger();
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            // Balance exactly at the protected floor.
            1_000,
            1_000,
            3_000,
            2_000,
            1_500,
            1,
        )
        .unwrap();
    let report = reserve_report(&ledger, 1_000).unwrap().unwrap();
    // Balance exactly at the floor: solvent, but nothing available.
    assert_eq!(report.available_capacity_atomic, 0);
    assert!(report.invariant_holds);
}

// ------------------------------------------------------ route status --
//
// Availability is a conjunction of independent gates, and the tests below
// turn each one off in isolation. The property they collectively pin is
// the one the whole phase depends on: availability can become true only
// when a route is EXPLICITLY enabled everywhere, and every default answer
// is "no".

use crate::chains::ChainRegistry;
use crate::routes::{RouteGate, RoutesConfig};

fn ready() -> RobinhoodReadiness {
    RobinhoodReadiness {
        deployment_verified: true,
        signers_available: 3,
        signers_required: crate::robinhood::SIGNER_THRESHOLD,
        halted: None,
        chain_id_disagrees: false,
        never_connected: false,
        reserve_paused: false,
        reserve_unconfigured: false,
    }
}

/// The production registry for a verified deployment.
///
/// Note what this does NOT open: `GoldcoinAdapter::capability` refuses
/// all four Robinhood routes unconditionally, and `ensure_enabled`
/// consults BOTH legs of a route, so the adapter gate stays closed for
/// `GlcToRhn`/`RhnToGlc` even with a fully verified Robinhood adapter.
/// That is pinned by
/// [`the_goldcoin_adapter_still_closes_both_executable_routes`] and is a
/// remaining launch blocker, not something this phase changes.
fn production_gate(node: &MockNode) -> RouteGate {
    RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, false, false),
        ChainRegistry::with_verified_robinhood(node.verified_deployment()),
    )
}

/// A gate with every SERVICE-side gate open for the two executable
/// routes: config, adapter (both legs), and — once
/// [`ledger_with_open_routes`] is used — the ledger.
///
/// Identical to [`production_gate`], and deliberately so. It used to
/// substitute a test-only Goldcoin adapter, because the real one refused
/// `GlcToRhn` for want of a route-aware deposit pipeline (blocker I). Now
/// that the real adapter serves both Goldcoin<->Robinhood legs, a shim
/// would only be a way for these tests to stop testing the production
/// registry. The alias is kept because the name reads correctly at the
/// call sites below, which are about the gate being OPEN.
fn open_gate(node: &MockNode) -> RouteGate {
    production_gate(node)
}

/// Flips one route's `bridge_routes` row on. Raw SQL rather than
/// `Ledger::set_route_enabled`, so the two routes that setter refuses can
/// still be forced on by the deliberate-misconfiguration tests below.
/// Purely local to an in-memory ledger; enables nothing anywhere else.
fn enable_route_in_ledger(ledger: &Ledger, route: Route) {
    let n = ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_routes SET enabled = 1 WHERE route_id = ?1",
            [route.as_str()],
        )
        .expect("opens the ledger route gate");
    assert_eq!(n, 1, "schema v24 seeds a row for every route");
}

/// A ledger with both executable routes switched on — the third gate.
fn ledger_with_open_routes() -> Ledger {
    let ledger = ledger();
    enable_route_in_ledger(&ledger, Route::GlcToRhn);
    enable_route_in_ledger(&ledger, Route::RhnToGlc);
    ledger
}

fn status_for(statuses: &[RouteStatus], route: Route) -> &RouteStatus {
    statuses
        .iter()
        .find(|s| s.route == route.as_str())
        .expect("every Robinhood route is reported")
}

/// The shipping default: nothing enabled anywhere, nothing available.
#[test]
fn every_robinhood_route_is_unavailable_by_default() {
    let ledger = ledger();
    let gate = RouteGate::legacy_only();
    let statuses = route_status(&ledger, &gate, &ready(), |_| None);

    assert_eq!(statuses.len(), 4);
    for status in &statuses {
        assert!(!status.service_enabled, "{}", status.route);
        assert!(!status.effective_available, "{}", status.route);
        assert!(status.disabled_reason.is_some(), "{}", status.route);
    }
}

/// Enabled at every service-side gate, and STILL unavailable, because the
/// contract's own flag was not read. An unread flag is not a yes.
#[test]
fn a_service_enabled_route_is_unavailable_while_the_contract_flag_is_unknown() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| None);
    let glc_to_rhn = status_for(&statuses, Route::GlcToRhn);

    assert!(glc_to_rhn.service_enabled);
    assert_eq!(glc_to_rhn.contract_route_enabled, None);
    assert!(!glc_to_rhn.effective_available);
    assert_eq!(glc_to_rhn.disabled_reason, None);
    assert_eq!(glc_to_rhn.health_reason, None);
}

/// Service says yes, contract says no. Unavailable — and the reason is
/// not a service-side one, so `disabled_reason` stays empty.
#[test]
fn a_service_enabled_route_the_contract_disables_is_unavailable() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| Some(false));
    let glc_to_rhn = status_for(&statuses, Route::GlcToRhn);

    assert!(glc_to_rhn.service_enabled);
    assert_eq!(glc_to_rhn.contract_route_enabled, Some(false));
    assert!(!glc_to_rhn.effective_available);
}

/// Both gates open and the leg healthy — the only combination that
/// produces availability, and it took an explicit enablement at every
/// one of them to get here.
#[test]
fn availability_requires_both_gates_and_a_healthy_leg() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| Some(true));

    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        let status = status_for(&statuses, route);
        assert!(status.effective_available, "{}", status.route);
        assert!(status.implemented);
    }
    // The two Solana<->Robinhood routes are implemented (Phase H) but
    // stay unavailable here: `open_gate` opens only the Goldcoin pair.
    for route in [Route::SolToRhn, Route::RhnToSol] {
        let status = status_for(&statuses, route);
        assert!(!status.effective_available, "{}", status.route);
        assert!(!status.service_enabled, "{}", status.route);
        assert!(status.implemented, "{}", status.route);
    }
}

/// A halted indexer, a chain-id disagreement and an unformable quorum
/// each make an otherwise fully-enabled route unavailable — and each says
/// so through `health_reason` rather than `disabled_reason`, so an
/// operator is not sent to look at configuration.
#[test]
fn any_unhealthy_condition_makes_an_enabled_route_unavailable() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let gate = open_gate(&node);

    let cases: Vec<(&str, RobinhoodReadiness)> = vec![
        (
            "halted",
            RobinhoodReadiness {
                halted: Some(RobinhoodHaltReason::PostFinalityReorg),
                ..ready()
            },
        ),
        (
            "chain id",
            RobinhoodReadiness {
                chain_id_disagrees: true,
                ..ready()
            },
        ),
        (
            "quorum",
            RobinhoodReadiness {
                signers_available: 1,
                ..ready()
            },
        ),
        (
            "unverified deployment",
            RobinhoodReadiness {
                deployment_verified: false,
                ..ready()
            },
        ),
        (
            "never connected",
            RobinhoodReadiness {
                never_connected: true,
                ..ready()
            },
        ),
    ];

    for (label, readiness) in cases {
        let statuses = route_status(&ledger, &gate, &readiness, |_| Some(true));
        let status = status_for(&statuses, Route::GlcToRhn);
        assert!(
            !status.effective_available,
            "{label} must make the route unavailable"
        );
        assert!(
            status.health_reason.is_some(),
            "{label} must be reported as a HEALTH reason"
        );
        // Not a configuration problem — the gates are all open.
        assert_eq!(
            status.disabled_reason, None,
            "{label} is not a disabled_reason"
        );
    }
}

/// The most fundamental cause is named first, so an operator is pointed
/// at the root rather than at a symptom of it: a deployment that never
/// verified has no signer set to have a quorum of.
#[test]
fn the_health_reason_names_the_most_fundamental_cause_first() {
    let readiness = RobinhoodReadiness {
        deployment_verified: false,
        signers_available: 0,
        signers_required: 2,
        halted: Some(RobinhoodHaltReason::ChainIdMismatch),
        chain_id_disagrees: true,
        never_connected: true,
        reserve_paused: true,
        reserve_unconfigured: true,
    };
    let reason = readiness.health_reason().expect("unhealthy");
    assert!(
        reason.contains("no verified Robinhood deployment"),
        "{reason}"
    );
    assert!(readiness.health_reason().is_some());
    assert!(!readiness.signer_quorum_available());

    // With that fixed, the halt is next.
    let readiness = RobinhoodReadiness {
        deployment_verified: true,
        ..readiness
    };
    assert!(readiness.health_reason().unwrap().contains("HALTED"));
}

/// The third gate, in isolation: config and adapter both open, ledger
/// silent. Still closed, and it names the ledger as the refuser.
#[test]
fn the_ledger_gate_alone_keeps_a_route_closed() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger(); // every `bridge_routes` row at its seeded default
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| Some(true));
    let glc_to_rhn = status_for(&statuses, Route::GlcToRhn);

    assert!(!glc_to_rhn.service_enabled);
    assert!(!glc_to_rhn.effective_available);
    assert!(
        glc_to_rhn
            .disabled_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("ledger:")),
        "{:?}",
        glc_to_rhn.disabled_reason
    );
}

/// Blockers H and I together: the Goldcoin adapter no longer closes
/// either Goldcoin<->Robinhood route.
///
/// `RouteGate::ensure_enabled` requires BOTH legs of a route to be
/// operational, and `GoldcoinAdapter` used to refuse all four Robinhood
/// routes — so `RhnToGlc` was permanently closed by a leg that could in
/// fact serve it (blocker H, fixed: the existing vault payout machinery
/// genuinely sweeps that direction), and `GlcToRhn` was closed by a
/// deposit pipeline that really was `GlcToSol`-only (blocker I, fixed:
/// the pipeline now keys on `Direction::source_is_goldcoin`).
///
/// The ADAPTER gate is the only thing either fix moved. This test is
/// scoped to `service_enabled`, the field that reports it — the sibling
/// tests below turn every remaining gate off one at a time and prove
/// availability still needs all of them.
#[test]
fn the_goldcoin_adapter_leg_no_longer_closes_either_robinhood_route() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let statuses = route_status(&ledger, &production_gate(&node), &ready(), |_| Some(true));

    for route in [Route::RhnToGlc, Route::GlcToRhn] {
        let status = status_for(&statuses, route);
        assert!(
            status.service_enabled,
            "the adapter gate must no longer block {route:?}: {:?}",
            status.disabled_reason
        );
        assert!(
            !status
                .disabled_reason
                .as_deref()
                .is_some_and(|r| r.starts_with("adapter:")),
            "{route:?} must not be refused by the adapter gate: {:?}",
            status.disabled_reason
        );
    }
}

// ------------------------------- capability is not enablement (blocker H) --
//
// The whole point of widening `GoldcoinAdapter`: it must move the ADAPTER
// gate and nothing else. These tests turn each remaining gate off in
// isolation against a route whose both legs are now capable, and prove
// availability still requires every one of them.

/// Adapter capability alone opens nothing: with both legs operational but
/// no config entry, `RhnToGlc` is closed BY CONFIG.
#[test]
fn adapter_capability_alone_does_not_make_rhn_to_glc_available() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    // Default config: every Robinhood route false.
    let gate = RouteGate::new(
        RoutesConfig::default(),
        ChainRegistry::with_verified_robinhood(node.verified_deployment()),
    );
    let statuses = route_status(&ledger, &gate, &ready(), |_| Some(true));
    let status = status_for(&statuses, Route::RhnToGlc);

    assert!(!status.service_enabled);
    assert!(!status.effective_available);
    assert!(
        status
            .disabled_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("config:")),
        "{:?}",
        status.disabled_reason
    );
}

/// Config open, adapter open, ledger silent: closed BY THE LEDGER.
#[test]
fn adapter_capability_alone_does_not_bypass_the_ledger_gate() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger(); // every `bridge_routes` row at its seeded default
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| Some(true));
    let status = status_for(&statuses, Route::RhnToGlc);

    assert!(!status.service_enabled);
    assert!(
        status
            .disabled_reason
            .as_deref()
            .is_some_and(|r| r.starts_with("ledger:")),
        "{:?}",
        status.disabled_reason
    );
}

/// Every service-side gate open, contract flag false: still unavailable.
/// The contract is a gate this service does not control and cannot
/// override.
#[test]
fn a_contract_disabled_route_is_unavailable_however_capable_the_adapters_are() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| Some(false));
    let status = status_for(&statuses, Route::RhnToGlc);

    assert!(status.service_enabled, "the service side is fully open");
    assert_eq!(status.contract_route_enabled, Some(false));
    assert!(!status.effective_available);
}

/// And an UNREAD contract flag is not a yes.
#[test]
fn an_unread_contract_flag_is_not_treated_as_enabled() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let statuses = route_status(&ledger, &open_gate(&node), &ready(), |_| None);
    let status = status_for(&statuses, Route::RhnToGlc);

    assert!(status.service_enabled);
    assert_eq!(status.contract_route_enabled, None);
    assert!(!status.effective_available);
}

/// A paused or unconfigured Robinhood reserve makes an otherwise fully
/// open route unusable — and says so as a HEALTH reason, not a
/// configuration one.
#[test]
fn a_paused_or_unconfigured_reserve_makes_the_route_unavailable() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let gate = open_gate(&node);

    for (label, readiness) in [
        (
            "paused",
            RobinhoodReadiness {
                reserve_paused: true,
                ..ready()
            },
        ),
        (
            "unconfigured",
            RobinhoodReadiness {
                reserve_unconfigured: true,
                ..ready()
            },
        ),
    ] {
        let statuses = route_status(&ledger, &gate, &readiness, |_| Some(true));
        let status = status_for(&statuses, Route::RhnToGlc);
        assert!(
            !status.effective_available,
            "a {label} reserve must make the route unavailable"
        );
        assert!(status.health_reason.is_some(), "{label}");
        assert_eq!(
            status.disabled_reason, None,
            "{label} is not a config fault"
        );
    }
}

/// An unformable signer quorum, with everything else open.
#[test]
fn an_unformable_quorum_makes_the_route_unavailable() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes();
    let readiness = RobinhoodReadiness {
        signers_available: 1,
        ..ready()
    };
    let statuses = route_status(&ledger, &open_gate(&node), &readiness, |_| Some(true));
    let status = status_for(&statuses, Route::RhnToGlc);

    assert!(status.service_enabled);
    assert!(!status.effective_available);
    assert!(status
        .health_reason
        .as_deref()
        .is_some_and(|r| r.contains("authorization signers")));
}

/// ALL gates explicitly opened, and only then does availability become
/// true. Every one of them had to be turned on deliberately.
#[test]
fn rhn_to_glc_becomes_available_only_when_every_gate_is_explicitly_opened() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger_with_open_routes(); // ledger gate
    let gate = open_gate(&node); // config gate + both adapter legs
    let statuses = route_status(&ledger, &gate, &ready(), |_| Some(true)); // contract gate
    let status = status_for(&statuses, Route::RhnToGlc);

    assert!(status.implemented);
    assert!(status.service_enabled);
    assert_eq!(status.contract_route_enabled, Some(true));
    assert_eq!(status.disabled_reason, None);
    assert_eq!(status.health_reason, None);
    assert!(status.effective_available);
}

/// The two Solana<->Robinhood routes open EXACTLY like the Goldcoin
/// pair: every service gate must agree AND the contract must report the
/// route enabled. Closing any one of them closes the route, and the
/// default state on every gate is closed.
#[test]
fn the_solana_robinhood_routes_open_only_when_every_gate_agrees() {
    let node = MockNode::new(BRIDGE);
    let ledger = ledger();
    // Defaults: closed everywhere.
    let closed = RouteGate::new(
        RoutesConfig::default(),
        ChainRegistry::with_verified_robinhood(node.verified_deployment()),
    );
    let statuses = route_status(&ledger, &closed, &ready(), |_| Some(false));
    for route in [Route::SolToRhn, Route::RhnToSol] {
        let status = status_for(&statuses, route);
        assert!(status.implemented, "{}", status.route);
        assert!(!status.service_enabled, "{}", status.route);
        assert!(!status.effective_available, "{}", status.route);
        assert!(route.as_direction().is_some());
    }

    enable_route_in_ledger(&ledger, Route::SolToRhn);
    enable_route_in_ledger(&ledger, Route::RhnToSol);
    let gate = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        ChainRegistry::with_verified_robinhood(node.verified_deployment()),
    );
    // Service open, contract still closed: not available.
    let statuses = route_status(&ledger, &gate, &ready(), |_| Some(false));
    for route in [Route::SolToRhn, Route::RhnToSol] {
        let status = status_for(&statuses, route);
        assert!(status.service_enabled, "{}", status.route);
        assert!(!status.effective_available, "{}", status.route);
    }
    // Everything open: available.
    let statuses = route_status(&ledger, &gate, &ready(), |_| Some(true));
    for route in [Route::SolToRhn, Route::RhnToSol] {
        let status = status_for(&statuses, route);
        assert!(status.effective_available, "{}", status.route);
    }
    // Config closed for one cross route only: that route alone closes.
    let one_closed = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, false, true),
        ChainRegistry::with_verified_robinhood(node.verified_deployment()),
    );
    let statuses = route_status(&ledger, &one_closed, &ready(), |_| Some(true));
    assert!(!status_for(&statuses, Route::SolToRhn).service_enabled);
    assert!(status_for(&statuses, Route::RhnToSol).service_enabled);
}

// ============================================================
// A refund is liability resolution, not bridge traffic
// ============================================================
//
// It is legitimate for a refund to execute while routes are paused or
// disabled — returning a depositor's own principal is the resolution of a
// liability the bridge already incurred, not new traffic across it. These
// tests establish what that permission does NOT extend to.

/// The daemon can never MINT a refund authorization, whatever its route
/// gate says.
///
/// `tick_authorize` handles exactly two things: `GlcToRhn` in
/// `SourceFinalized` (a payout) and `RhnToGlc` in `DestinationConfirmed`
/// (a settlement). A request parked in `ManualReview` — the only state a
/// refund begins from — matches neither. So a refund exists only because
/// an operator ran `robinhood-refund --execute`; no amount of daemon
/// uptime produces one.
#[tokio::test]
async fn the_daemon_never_mints_a_refund_however_long_it_runs() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 41);
    obligation(&node, 41, OBLIGATION_STATUS_PENDING);
    let settler = settler(&node);

    // Every phase, with the route gate wide OPEN, several times over.
    for tick in 0..5 {
        let mut report = crate::robinhood::SettlementReport::default();
        settler.tick_fold(&mut ledger, true, 1_000 + tick, &mut report);
        settler
            .tick_authorize(&mut ledger, 1_000 + tick, &mut report)
            .await;
        settler
            .tick_broadcast(&mut ledger, 1_000 + tick, &mut report)
            .await;
        settler
            .tick_receipts(&mut ledger, 1_000 + tick, &mut report)
            .await;
    }

    assert!(
        ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)
            .unwrap()
            .is_none(),
        "no refund may exist without an explicit operator action"
    );
    // And the request is untouched — still parked, awaiting a human's
    // decision between resuming and refunding.
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
}

/// The refund pays the OBLIGATION's depositor and principal, never the
/// ledger row's.
///
/// The fixture is deliberately adversarial: the parked request's own
/// recorded amount is nothing like the obligation's, and its recipient is
/// a Goldcoin address entirely. If the refund read either, this test
/// would catch it.
#[tokio::test]
async fn a_refund_pays_the_obligations_own_depositor_and_principal_only() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 43);
    obligation(&node, 43, OBLIGATION_STATUS_PENDING);

    // Corrupt the ledger row's amounts — a tampered column must not be
    // able to change what a refund pays.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests
                SET gross_amount_atomic = 1, net_amount_atomic = 1, net_destination_atomic = 1
             WHERE id = ?1",
            rusqlite::params![request_id],
        )
        .unwrap();

    crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect("begins");

    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)
        .unwrap()
        .expect("a refund exists");
    // The obligation's values, not the row's.
    assert_eq!(tx.recipient, Some(DEPOSITOR.to_bytes()));
    assert_eq!(
        tx.amount_robinhood,
        Some(EvmU256::from_u128(PRINCIPAL_18DP).to_be_bytes())
    );
    // There is no code path, and no CLI flag, that supplies either.
}

/// A refund requires a real 2-of-3 quorum. One signer is not a quorum,
/// and the failure leaves no authorization behind.
#[tokio::test]
async fn a_refund_without_a_quorum_stores_no_authorization() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 45);
    obligation(&node, 45, OBLIGATION_STATUS_PENDING);

    let config = node.settlement_config();
    let one_signer: Vec<Box<dyn EvmAuthSigner>> =
        vec![Box::new(DevEvmAuthSigner::new(signer_key(0)))];
    let settler = Settler::new(
        node.clone(),
        Submitter::from_key(submitter_key(), &config).unwrap(),
        one_signer,
        node.verified_deployment(),
        config,
        Duration::from_secs(5),
        crate::goldcoin::address::Network::Testnet,
        6,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    );

    let err = crate::robinhood::begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .expect_err("one signer is not a quorum");
    assert!(
        err.to_string().contains("quorum") || err.to_string().contains("signer"),
        "{err}"
    );

    // The operation row may exist (it is written before signers are
    // asked, so a crash mid-collection resumes rather than re-minting),
    // but it carries NO signatures and can never be broadcast.
    if let Some(tx) = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)
        .unwrap()
    {
        assert_eq!(tx.state, RobinhoodTxState::Authorizing);
        assert!(ledger.robinhood_auth_signatures(tx.id).unwrap().is_empty());
        assert!(tx.raw_tx.is_none());
        assert!(tx.nonce.is_none());
    }
}

/// A refund creates no new obligation and no Goldcoin payout. It resolves
/// an existing liability; it does not add one.
#[tokio::test]
async fn a_refund_creates_no_new_obligation_and_no_payout() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 47);
    obligation(&node, 47, OBLIGATION_STATUS_PENDING);
    let obligations_before = node.with(|s| s.contract.obligation_count);

    crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect("begins");

    assert_eq!(
        node.with(|s| s.contract.obligation_count),
        obligations_before,
        "a refund must not create an obligation"
    );
    assert!(
        ledger.get_goldcoin_payout(request_id).unwrap().is_none(),
        "a refund must not create a Goldcoin payout"
    );
    // Exactly one operation for this request, and it is the refund.
    let views = txs_for_request(&ledger, request_id).unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].kind, RobinhoodTxKind::Refund);
}

/// Executing a refund while every route is closed does not widen what it
/// can do. The route gate governs new bridge traffic; it is not what
/// bounds a refund, and removing it grants nothing.
#[tokio::test]
async fn a_refund_under_a_closed_route_is_still_bounded_by_the_contracts_own_values() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 49);
    obligation(&node, 49, OBLIGATION_STATUS_PENDING);

    // Seeded `bridge_routes` defaults and default config: every route closed.
    let gate = RouteGate::legacy_only();
    assert!(!gate.is_enabled(&ledger, Route::RhnToGlc));

    crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_000)
        .await
        .expect("a refund is liability resolution and does not consult the route gate");

    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Refund, request_id)
        .unwrap()
        .expect("exists");
    // The bound that actually applies: the obligation's own recipient and
    // principal, and a quorum over a digest binding both.
    assert_eq!(tx.recipient, Some(DEPOSITOR.to_bytes()));
    assert_eq!(
        tx.amount_robinhood,
        Some(EvmU256::from_u128(PRINCIPAL_18DP).to_be_bytes())
    );
    assert_eq!(
        ledger.robinhood_auth_signatures(tx.id).unwrap().len(),
        crate::robinhood::SIGNER_THRESHOLD
    );
    // A second refund for the same request is impossible.
    let again = crate::robinhood::begin_refund(&settler(&node), &mut ledger, request_id, 1_100)
        .await
        .expect("idempotent");
    assert_eq!(again, tx.id, "re-running resumes the SAME operation");
}

// ------------------------------------------ destination rendering --

/// The 32-byte Solana pubkey V1 obligation #30's fold recorded as the
/// request's recipient — the bytes from the chain's own DepositCreated
/// log — and the base58 spelling the depositor typed.
const INCIDENT_SOL_PUBKEY: [u8; 32] = [
    0x57, 0x30, 0xac, 0xf5, 0x97, 0x9d, 0xd7, 0xca, 0x59, 0x04, 0x23, 0xc0, 0x5e, 0xee, 0x41, 0xea,
    0x5d, 0x31, 0xc5, 0x67, 0x3e, 0x6a, 0x8b, 0x59, 0x0c, 0x00, 0x6f, 0xc8, 0xc4, 0xac, 0x3d, 0x47,
];
const INCIDENT_SOL_BASE58: &str = "6sMX5pDw7cBaVohMadPEo9SksjbKjBKtvRBDpdtvVw8A";

/// An `RhnToSol` request parked in `ManualReview` with the raw 32-byte
/// pubkey as its recipient — exactly what `fold_observation_to_solana`
/// stores.
fn parked_sol_request(ledger: &Ledger, obligation_index: u64, recipient: &[u8]) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                 source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, manual_review_note)
             VALUES ('RhnToSol', 'ManualReview', 13500000000, 300, 405000000, 13095000000, 0,
                     ?1, 100, 'robinhood', ?2, ?3, 1, 100, 'route closed at fold')",
            rusqlite::params![recipient, &BRIDGE.to_bytes()[..], obligation_index as i64],
        )
        .expect("seeds a parked RhnToSol request");
    ledger.conn_for_tests().last_insert_rowid()
}

/// The regression: the incident's recipient bytes render as the base58
/// pubkey, byte-for-byte reversible, never as lossy UTF-8.
#[test]
fn an_rhn_to_sol_destination_renders_as_the_base58_pubkey() {
    assert_eq!(
        render_destination(Direction::RhnToSol, &INCIDENT_SOL_PUBKEY),
        INCIDENT_SOL_BASE58
    );
    // The spelling is exact: decoding it gives back the stored bytes.
    let decoded = solana_sdk::pubkey::Pubkey::from_str(INCIDENT_SOL_BASE58).unwrap();
    assert_eq!(decoded.to_bytes(), INCIDENT_SOL_PUBKEY);
    // And the old rendering was the garbage the operator saw.
    assert_ne!(
        String::from_utf8_lossy(&INCIDENT_SOL_PUBKEY),
        INCIDENT_SOL_BASE58
    );
}

#[test]
fn the_manual_review_queue_renders_each_route_in_its_own_destination_spelling() {
    let ledger = ledger();
    let glc = parked_request(&ledger, 2);
    let sol = parked_sol_request(&ledger, 30, &INCIDENT_SOL_PUBKEY);

    let queue = manual_review_queue(&ledger).expect("reads");
    let by_id = |id: i64| queue.iter().find(|i| i.request_id == id).unwrap();

    let glc_item = by_id(glc);
    assert_eq!(glc_item.direction, Direction::RhnToGlc);
    assert_eq!(
        glc_item.destination, "ab",
        "the Goldcoin address text, as before"
    );

    let sol_item = by_id(sol);
    assert_eq!(sol_item.direction, Direction::RhnToSol);
    assert_eq!(sol_item.obligation_index, Some(30));
    assert_eq!(sol_item.destination, INCIDENT_SOL_BASE58);
    assert_eq!(sol_item.gross_amount_atomic, 13_500_000_000);
    assert_eq!(sol_item.net_amount_atomic, 13_095_000_000);
}

/// Bytes that are not what the route records are shown as hex with their
/// length — never silently coerced into a plausible-looking address.
#[test]
fn an_unexpected_destination_shape_is_shown_as_hex_not_guessed() {
    // 20 bytes on a Solana route: not a pubkey.
    let rendered = render_destination(Direction::RhnToSol, &[0xab; 20]);
    assert!(rendered.starts_with("0xabab"), "{rendered}");
    assert!(rendered.contains("20 bytes"), "{rendered}");
    // Non-UTF-8 on the Goldcoin route.
    let rendered = render_destination(Direction::RhnToGlc, &[0xff, 0xfe]);
    assert!(rendered.starts_with("0xfffe"), "{rendered}");
    // A route no Robinhood-sourced fold produces.
    let rendered = render_destination(Direction::GlcToRhn, &[0x01; 20]);
    assert!(
        rendered.contains("not a destination this route records"),
        "{rendered}"
    );
}
