//! The two Solana<->Robinhood routes through the REAL settlement engine:
//! `SolToRhn` is the same payout pipeline as `GlcToRhn` under route
//! `0x03`, and `RhnToSol` the same settlement pipeline as `RhnToGlc` under
//! route `0x04`. Every phase runs against the scriptable node and a real
//! in-memory ledger, exactly as the sibling tests do for the Goldcoin pair.

use super::*;
use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic};
use crate::ledger::{RequestAmounts, ReserveDirection, SolFoldOutcome};
use crate::robinhood::auth::{
    derive_request_id, solana_source_identity, ACTION_PAYOUT, ACTION_SETTLE,
};
use crate::robinhood::testkit::{PROTOCOL_ROBINHOOD, PROTOCOL_SOLANA};

const SOL_RECIPIENT: [u8; 32] = [0x51; 32];
const MINT_DECIMALS: u8 = 6;

fn open_cross_routes(node: &MockNode) {
    node.with(|s| {
        s.contract.route_enabled.insert(0x03, true);
        s.contract.route_enabled.insert(0x04, true);
    });
}

/// A reserve holding `balance`, with NOTHING reserved against it yet —
/// unlike the sibling `configure_robinhood_reserve`, which seeds the row
/// as already carrying a seeded request's reservation. The folds here are
/// real, so they take their own reservation.
fn fund_reserve(ledger: &mut Ledger, reserve: ReserveDirection, balance: u64) {
    ledger
        .configure_reserve(reserve, balance, 0, balance, balance / 2, balance / 4, 100)
        .expect("configures the reserve");
}

/// A `SolToRhn` request whose Solana deposit is final, folded through
/// the real fold so every column is what production writes.
fn seed_sol_to_rhn(ledger: &mut Ledger, obligation_index: u64, gross_canonical: u64) -> i64 {
    let fb = compute_fee_at_bps(CanonicalAtomic(gross_canonical), 300).unwrap();
    match ledger
        .fold_sol_deposit_to_robinhood(
            obligation_index,
            RequestAmounts {
                gross_atomic: fb.gross.0,
                fee_bps: fb.fee_bps,
                fee_atomic: fb.fee.0,
                net_atomic: fb.net.0,
                net_destination_atomic: fb.net.0,
            },
            [0x11; 32],
            Some(RECIPIENT),
            b"0x",
            true,
            None,
            100,
        )
        .unwrap()
    {
        SolFoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("{other:?}"),
    }
}

/// An `RhnToSol` request whose Solana RELEASE confirmed at `finalized` —
/// the only state a settlement may be authorized from — folded and
/// advanced through the real ledger functions.
fn seed_rhn_to_sol_released(
    ledger: &mut Ledger,
    obligation_index: u64,
    gross_canonical: u64,
    confirm_release: bool,
) -> i64 {
    let row = crate::ledger::RobinhoodObservationRow {
        id: obligation_index as i64 + 1,
        observation: crate::ledger::RobinhoodDepositObservation {
            source_contract: BRIDGE.to_bytes(),
            obligation_index,
            route: Route::RhnToSol,
            depositor: DEPOSITOR.to_bytes(),
            destination: SOL_RECIPIENT.to_vec(),
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(
                u128::from(gross_canonical) * 10_000_000_000,
            )
            .to_be_bytes(),
            amount_canonical_atomic: gross_canonical,
            tx_hash: {
                let mut h = [0xaa; 32];
                h[0] = obligation_index as u8;
                h
            },
            log_index: 0,
            block_number: 50,
            block_hash: [0xbb; 32],
        },
        finality: crate::ledger::RobinhoodFinality::Final,
        observed_at: 100,
        finalized_at: Some(200),
        reorged_at: None,
    };
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, 4, 'RhnToSol', ?4, ?5, ?6, ?7, ?8, 0, 50, ?9,
                     'Final', 100, 200)",
            rusqlite::params![
                row.id,
                &row.observation.source_contract[..],
                obligation_index as i64,
                &row.observation.depositor[..],
                row.observation.destination,
                &row.observation.amount_robinhood_atomic[..],
                gross_canonical as i64,
                &row.observation.tx_hash[..],
                &row.observation.block_hash[..],
            ],
        )
        .unwrap();
    let request_id = match crate::robinhood::fold::fold_observation_to_solana(
        ledger,
        &row,
        300,
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap()
    {
        crate::robinhood::fold::FoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    ledger
        .record_release_submitted(request_id, [0x77; 64], 400)
        .unwrap();
    if confirm_release {
        ledger.mark_release_confirmed(request_id, 500).unwrap();
    }
    request_id
}

fn pending_obligation_on(node: &MockNode, index: u64, route: u8, amount_18dp: u128) {
    node.with(|s| {
        s.contract.obligation_count = s.contract.obligation_count.max(index + 1);
        s.contract.obligations.insert(
            index,
            Obligation {
                depositor: DEPOSITOR,
                status: OBLIGATION_STATUS_PENDING,
                route,
                amount: crate::evm::EvmU256::from_u128(amount_18dp),
            },
        );
    });
}

async fn drive_to_finality(
    node: &MockNode,
    settler: &Settler<MockNode>,
    ledger: &mut Ledger,
    tx_id: i64,
) {
    let mut report = SettlementReport::default();
    settler.tick_broadcast(ledger, 1_100, &mut report).await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Broadcast);
    let hash = tx.tx_hash.unwrap();
    node.mine(hash, 200, true);
    node.mark_executed(tx.action, tx.contract_request_id);
    node.with(|s| s.head = 202);
    settler.tick_receipts(ledger, 1_300, &mut report).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        ledger.get_robinhood_tx(tx_id).unwrap().unwrap().state,
        RobinhoodTxState::Finalized
    );
}

// =====================================================================
// SolToRhn — the payout pipeline under route 0x03
// =====================================================================

#[tokio::test]
async fn sol_to_rhn_pays_out_under_its_own_route_and_stops_at_destination_confirmed() {
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    let gross = 1_000_000_000u64; // 10 GLC
    fund_reserve(&mut ledger, ReserveDirection::RobinhoodReserve, 970_000_000);
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    let request_id = seed_sol_to_rhn(&mut ledger, 7, gross);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .expect("a payout operation exists");
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(
        tx.route,
        Some(Route::SolToRhn),
        "the contract route the payout binds"
    );
    assert_eq!(tx.action, ACTION_PAYOUT);
    assert_eq!(tx.recipient, Some(RECIPIENT));
    assert_eq!(
        crate::evm::EvmU256::from_be_bytes(tx.amount_robinhood.unwrap()),
        crate::evm::EvmU256::from_u128(970_000_000u128 * 10_000_000_000),
        "the canonical net widened by 10^10"
    );
    // The contract request id is derived from the SOLANA source identity
    // — program id, obligation index, row id — under route 0x03.
    let expected = derive_request_id(
        ACTION_PAYOUT,
        Route::SolToRhn,
        node.verified_deployment().domain(),
        &solana_source_identity(&glc_reserve_bridge_shared::PROGRAM_ID_BYTES, 7, request_id),
    )
    .unwrap();
    assert_eq!(tx.contract_request_id, expected);
    // And the chain pair bound is (Solana, Robinhood), read from the
    // deployment.
    assert_eq!(
        node.verified_deployment()
            .chains_for(Route::SolToRhn)
            .unwrap(),
        crate::robinhood::auth::ProtocolChainPair {
            source: PROTOCOL_SOLANA,
            dest: PROTOCOL_ROBINHOOD
        }
    );

    drive_to_finality(&node, &settler, &mut ledger, tx.id).await;

    // Finality is the DESTINATION leg: the request is not settled until
    // the Solana obligation is closed by the orchestrator.
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::DestinationConfirmed);
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!((balance, reserved, pending), (0, 0, 0));
    assert_eq!(
        ledger
            .settled_liquidity(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        970_000_000
    );
    // The fee accrued on the SOURCE (Solana) row.
    let fee: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT accrued_fees_atomic FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fee, 30_000_000);

    // Re-running every phase changes nothing.
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 2_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 2_100, &mut report)
        .await;
    settler.tick_receipts(&mut ledger, 2_300, &mut report).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(node.with(|s| s.broadcasts.len()), 1);
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationConfirmed
    );
}

#[tokio::test]
async fn sol_to_rhn_and_glc_to_rhn_never_share_a_contract_request_id() {
    // Same recipient, same net, same ledger — a Goldcoin-funded and a
    // Solana-funded payout are two different authorizations, so a quorum
    // for one can never be replayed as the other.
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    // The GlcToRhn seed expects its reservation pre-applied, so the row
    // is seeded as the sibling helper does and then topped up for the
    // real SolToRhn fold.
    configure_robinhood_reserve(&mut ledger, 970_000_000);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = total_reserve_balance + 970000000\n             WHERE direction = 'RobinhoodReserve'",
            [],
        )
        .unwrap();
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    let glc = seed_glc_to_rhn(&ledger, 970_000_000);
    let sol = seed_sol_to_rhn(&mut ledger, 0, 1_000_000_000);
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let a = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, glc)
        .unwrap()
        .unwrap();
    let b = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, sol)
        .unwrap()
        .unwrap();
    assert_eq!(a.route, Some(Route::GlcToRhn));
    assert_eq!(b.route, Some(Route::SolToRhn));
    assert_ne!(a.contract_request_id, b.contract_request_id);
    assert_ne!(a.auth_digest, b.auth_digest);
    assert_eq!(
        a.amount_robinhood, b.amount_robinhood,
        "same value, different authorization"
    );
}

#[tokio::test]
async fn a_closed_sol_to_rhn_gate_creates_no_operation_while_glc_to_rhn_proceeds() {
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    // The GlcToRhn seed expects its reservation pre-applied, so the row
    // is seeded as the sibling helper does and then topped up for the
    // real SolToRhn fold.
    configure_robinhood_reserve(&mut ledger, 970_000_000);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = total_reserve_balance + 970000000\n             WHERE direction = 'RobinhoodReserve'",
            [],
        )
        .unwrap();
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    let glc = seed_glc_to_rhn(&ledger, 970_000_000);
    let sol = seed_sol_to_rhn(&mut ledger, 0, 1_000_000_000);
    let mut report = SettlementReport::default();
    settler
        .tick_authorize_gated(
            &mut ledger,
            &|route| route != Route::SolToRhn,
            1_000,
            &mut report,
        )
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert!(ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, glc)
        .unwrap()
        .is_some());
    assert!(
        ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Payout, sol)
            .unwrap()
            .is_none(),
        "a closed route has no operation created for it"
    );
    assert_eq!(
        ledger.get_request(sol).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "the request waits, unchanged, for the route to open"
    );
    // Open it: the same request is picked up.
    settler
        .tick_authorize_gated(&mut ledger, &|_| true, 1_001, &mut report)
        .await;
    assert!(ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, sol)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn a_contract_with_sol_to_rhn_disabled_refuses_the_broadcast_and_consumes_no_nonce() {
    // The service-side gates are open; the contract's own flag is not.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    fund_reserve(&mut ledger, ReserveDirection::RobinhoodReserve, 970_000_000);
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    let request_id = seed_sol_to_rhn(&mut ledger, 0, 1_000_000_000);
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("SolToRhn") || report.errors[0].contains("disabled"));
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(tx.nonce, None, "refused before a nonce was allocated");
    assert_eq!(node.with(|s| s.broadcasts.len()), 0);
}

// =====================================================================
// RhnToSol — the settlement pipeline under route 0x04
// =====================================================================

#[tokio::test]
async fn rhn_to_sol_settles_the_obligation_only_after_the_release_is_final() {
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    fund_reserve(
        &mut ledger,
        ReserveDirection::RobinhoodReserve,
        1_000_000_000,
    );
    let gross = 500_000_000u64;
    pending_obligation_on(&node, 3, 0x04, u128::from(gross) * 10_000_000_000);

    // Release submitted but NOT final: nothing is authorized.
    let request_id = seed_rhn_to_sol_released(&mut ledger, 3, gross, false);
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert!(ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .is_none());

    // Release final: the settlement is authorized under route 0x04
    // against the obligation's own index.
    ledger.mark_release_confirmed(request_id, 500).unwrap();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .expect("a settlement operation exists");
    assert_eq!(tx.route, Some(Route::RhnToSol));
    assert_eq!(tx.action, ACTION_SETTLE);
    assert_eq!(tx.obligation_index, Some(3));
    let expected = derive_request_id(
        ACTION_SETTLE,
        Route::RhnToSol,
        node.verified_deployment().domain(),
        &crate::robinhood::auth::obligation_identity(3),
    )
    .unwrap();
    assert_eq!(tx.contract_request_id, expected);

    let solana_before = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    drive_to_finality(&node, &settler, &mut ledger, tx.id).await;
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    // The observation mirrors the on-chain settled status, and no reserve
    // moved at this step — the Solana reserve moved when the release
    // confirmed.
    let settled: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT settled FROM robinhood_deposit_observations WHERE folded_request_id = ?1",
            [request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(settled, 1);
    assert_eq!(
        ledger
            .reserve_snapshot(ReserveDirection::SolanaReserve)
            .unwrap(),
        solana_before
    );
}

#[tokio::test]
async fn rhn_to_sol_and_rhn_to_glc_settlements_are_distinct_authorizations() {
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    configure_goldcoin_reserve(&mut ledger, 970_000_000);
    fund_reserve(
        &mut ledger,
        ReserveDirection::RobinhoodReserve,
        1_000_000_000,
    );
    pending_obligation(&node, 0, 10_000_000_000_000_000_000);
    pending_obligation_on(&node, 1, 0x04, 5_000_000_000_000_000_000);
    let glc = seed_rhn_to_glc_paid_out(&ledger, 0, 970_000_000, 6);
    let sol = seed_rhn_to_sol_released(&mut ledger, 1, 500_000_000, true);
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let a = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, glc)
        .unwrap()
        .unwrap();
    let b = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, sol)
        .unwrap()
        .unwrap();
    assert_eq!(a.route, Some(Route::RhnToGlc));
    assert_eq!(b.route, Some(Route::RhnToSol));
    assert_ne!(a.contract_request_id, b.contract_request_id);
    assert_ne!(a.auth_digest, b.auth_digest);
}

#[tokio::test]
async fn a_reverted_cross_route_operation_parks_the_request_with_the_reason() {
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    fund_reserve(&mut ledger, ReserveDirection::RobinhoodReserve, 970_000_000);
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    let request_id = seed_sol_to_rhn(&mut ledger, 0, 1_000_000_000);
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    node.mine(tx.tx_hash.unwrap(), 200, false);
    node.with(|s| s.head = 202);
    settler.tick_receipts(&mut ledger, 1_300, &mut report).await;
    assert_eq!(report.reverted, 1);
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert!(request
        .manual_review_note
        .as_deref()
        .unwrap()
        .contains("reverted on Robinhood"));
    // The reservation was released; the source deposit is intact and
    // visible for a human to refund on Solana.
    let (_, _, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!((reserved, pending), (0, 0));
    assert!(
        ledger
            .solana_refund_db_checks(request_id)
            .unwrap()
            .direction_ok
    );
}

/// The LOOP consults the gate per route: with `SolToRhn` closed and
/// `GlcToRhn` open, a tick authorizes the Goldcoin-funded payout and
/// leaves the Solana-funded one exactly as it was.
#[tokio::test]
async fn the_settlement_loop_gates_each_cross_route_on_its_own() {
    let node = MockNode::new(BRIDGE);
    open_cross_routes(&node);
    let settler = settler(&node);
    let mut ledger = ledger();
    configure_robinhood_reserve(&mut ledger, 970_000_000);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = total_reserve_balance + 970000000
             WHERE direction = 'RobinhoodReserve'",
            [],
        )
        .unwrap();
    fund_reserve(&mut ledger, ReserveDirection::SolanaReserve, 1_000_000_000);
    let glc = seed_glc_to_rhn(&ledger, 970_000_000);
    let sol = seed_sol_to_rhn(&mut ledger, 0, 1_000_000_000);

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(15)).await;
        let _ = shutdown_tx.send(true);
    });
    crate::robinhood::daemon::run_settlement(
        &settler,
        &mut ledger,
        |_: &Ledger, route| route != Route::SolToRhn,
        crate::robinhood::daemon::RobinhoodLoopConfig {
            tick_interval: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        },
        shutdown_rx,
        || 1_000,
    )
    .await;
    stopper.await.unwrap();

    assert!(ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, glc)
        .unwrap()
        .is_some());
    assert!(ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, sol)
        .unwrap()
        .is_none());
    assert_eq!(
        ledger.get_request(sol).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}
