//! End-to-end settlement tests for both executable routes.
//!
//! Every test drives the REAL phases against a scriptable node and a real
//! in-memory ledger, so the ordering guarantees are exercised through the
//! same write transactions and the same database constraints production
//! uses. Nothing here contacts a live endpoint.

use super::*;
use crate::ledger::{RequestState, RobinhoodTxKind, RobinhoodTxState};
use crate::robinhood::calls::{Obligation, OBLIGATION_STATUS_PENDING};
use crate::robinhood::signer::{DevEvmAuthSigner, EvmAuthSigner};
use crate::robinhood::testkit::{
    signer_key, submitter_key, MockNode, SendBehaviour, BRIDGE, DEPOSITOR,
};
use crate::robinhood::Submitter;

const RECIPIENT: [u8; 20] = [0xec; 20];
const GLC_TXID: [u8; 32] = [0x9c; 32];

fn ledger() -> Ledger {
    Ledger::open_in_memory().expect("an in-memory ledger")
}

/// A settler wired to `node`, with a full 3-signer dev pool.
fn settler(node: &MockNode) -> Settler<MockNode> {
    settler_with_signers(node, vec![0, 1, 2])
}

fn settler_with_signers(node: &MockNode, indexes: Vec<u8>) -> Settler<MockNode> {
    let config = node.settlement_config();
    let signers: Vec<Box<dyn EvmAuthSigner>> = indexes
        .into_iter()
        .map(|i| Box::new(DevEvmAuthSigner::new(signer_key(i))) as Box<dyn EvmAuthSigner>)
        .collect();
    Settler::new(
        node.clone(),
        Submitter::from_key(submitter_key(), &config).expect("the configured submitter key"),
        signers,
        node.verified_deployment(),
        config,
        Duration::from_secs(5),
        crate::goldcoin::address::Network::Testnet,
        // The Goldcoin confirmation depth a `RhnToGlc` payout must reach
        // before its obligation may be settled.
        6,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
}

/// A `GlcToRhn` request whose Goldcoin deposit is final: the state the
/// payout phase picks up.
fn seed_glc_to_rhn(ledger: &Ledger, net_canonical: u64) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                 source_txid, source_vout, source_confirmations, source_finalized_at)
             VALUES ('GlcToRhn', 'SourceFinalized', ?1, 300, ?2, ?3, ?3, ?4, 100, 'goldcoin',
                     ?5, 0, 6, 100)",
            rusqlite::params![
                // gross such that a 300bps fee leaves exactly `net`.
                gross_for_net(net_canonical) as i64,
                (gross_for_net(net_canonical) - net_canonical) as i64,
                net_canonical as i64,
                &RECIPIENT[..],
                &GLC_TXID[..],
            ],
        )
        .expect("seeds a GlcToRhn request");
    ledger.conn_for_tests().last_insert_rowid()
}

/// The gross a 300 bps fee reduces to exactly `net`.
fn gross_for_net(net: u64) -> u64 {
    // fee = floor(gross * 300 / 10_000); pick a gross that divides evenly.
    let gross = net * 10_000 / 9_700;
    let breakdown =
        crate::amount_conversion::compute_fee(crate::amount_conversion::CanonicalAtomic(gross))
            .expect("a representable gross");
    assert_eq!(breakdown.net.0, net, "test fixture must reconcile exactly");
    gross
}

/// An `RhnToGlc` request whose GOLDCOIN PAYOUT has confirmed — the only
/// state a settlement may be authorized from.
fn seed_rhn_to_glc_paid_out(
    ledger: &Ledger,
    obligation_index: u64,
    net_canonical: u64,
    confirmations: i64,
) -> i64 {
    let gross = gross_for_net(net_canonical);
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                 source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at)
             VALUES ('RhnToGlc', 'DestinationConfirmed', ?1, 300, ?2, ?3, ?3, X'6162',
                     100, 'robinhood', ?4, ?5, 1, 100)",
            rusqlite::params![
                gross as i64,
                (gross - net_canonical) as i64,
                net_canonical as i64,
                &BRIDGE.to_bytes()[..],
                obligation_index as i64,
            ],
        )
        .expect("seeds an RhnToGlc request");
    let request_id = ledger.conn_for_tests().last_insert_rowid();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, txid, state, built_at, confirmations)
             VALUES (?1, X'00', ?2, 0, 0, X'00', ?3, 'Confirmed', 100, ?4)",
            rusqlite::params![
                request_id,
                net_canonical as i64,
                &GLC_TXID[..],
                confirmations
            ],
        )
        .expect("seeds a confirmed Goldcoin payout");
    request_id
}

fn pending_obligation(node: &MockNode, index: u64, amount_18dp: u128) {
    node.with(|s| {
        s.contract.obligation_count = s.contract.obligation_count.max(index + 1);
        s.contract.obligations.insert(
            index,
            Obligation {
                depositor: DEPOSITOR,
                status: OBLIGATION_STATUS_PENDING,
                route: 0x02,
                amount: crate::evm::EvmU256::from_u128(amount_18dp),
            },
        );
    });
}

/// Configures the Robinhood reserve with room for `capacity` canonical
/// units, so a `GlcToRhn` settlement's bookkeeping has a row to move.
fn configure_robinhood_reserve(ledger: &mut Ledger, capacity: u64) {
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::RobinhoodReserve,
            capacity,
            0,
            capacity,
            capacity / 2,
            capacity / 4,
            100,
        )
        .expect("configures the Robinhood reserve");
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, reserved_liquidity = ?1,
                pending_obligations = ?1 WHERE direction = 'RobinhoodReserve'",
            [capacity as i64],
        )
        .expect("seeds the reserve as holding this request's reservation");
}

fn configure_goldcoin_reserve(ledger: &mut Ledger, capacity: u64) {
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            capacity,
            0,
            capacity,
            capacity / 2,
            capacity / 4,
            100,
        )
        .expect("configures the Goldcoin reserve");
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, reserved_liquidity = ?1,
                pending_obligations = ?1 WHERE direction = 'GoldcoinReserve'",
            [capacity as i64],
        )
        .expect("seeds the reserve");
}

// =====================================================================
// GlcToRhn — happy path
// =====================================================================

#[tokio::test]
async fn glc_to_rhn_authorizes_broadcasts_and_settles_exactly_once() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64; // 9.7 GLC in canonical 8dp
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    // ---- authorize ----
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .expect("an operation row exists");
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(tx.route, Route::GlcToRhn);
    assert_eq!(tx.action, crate::robinhood::auth::ACTION_PAYOUT);
    assert_eq!(tx.recipient, Some(RECIPIENT));
    // The exact 18-decimal amount: the canonical net widened by 10^10.
    assert_eq!(
        crate::evm::EvmU256::from_be_bytes(tx.amount_robinhood.unwrap()),
        crate::evm::EvmU256::from_u128(u128::from(net) * 10_000_000_000)
    );
    assert_eq!(ledger.robinhood_auth_signatures(tx.id).unwrap().len(), 2);

    // ---- broadcast ----
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Broadcast);
    assert!(tx.nonce.is_some(), "the nonce is persisted before the send");
    assert!(
        tx.raw_tx.is_some(),
        "the bytes are persisted before the send"
    );
    assert_eq!(node.with(|s| s.broadcasts.len()), 1);

    // ---- receipt, below the depth ----
    let hash = tx.tx_hash.unwrap();
    node.mine(hash, 200, true);
    node.mark_executed(tx.action, tx.contract_request_id);
    node.with(|s| s.head = 201);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;
    assert_eq!(
        ledger.get_robinhood_tx(tx.id).unwrap().unwrap().state,
        RobinhoodTxState::Included,
        "included is not finished"
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "the request must not complete before the payout is final"
    );

    // ---- receipt, at the depth ----
    node.with(|s| s.head = 202);
    settler.tick_receipts(&mut ledger, 1_300, &mut report).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        ledger.get_robinhood_tx(tx.id).unwrap().unwrap().state,
        RobinhoodTxState::Finalized
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );

    // The reserve bookkeeping moved exactly once.
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!((balance, reserved, pending), (0, 0, 0));
    assert_eq!(
        ledger
            .settled_liquidity(crate::ledger::ReserveDirection::RobinhoodReserve)
            .unwrap(),
        net
    );
}

#[tokio::test]
async fn a_duplicate_tick_produces_no_second_payout() {
    // The single most important idempotency property. Every phase is run
    // twice at every stage; exactly one transaction must ever be sent.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    for _ in 0..3 {
        settler
            .tick_authorize(&mut ledger, 1_000, &mut report)
            .await;
    }
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        ledger.robinhood_auth_signatures(tx.id).unwrap().len(),
        2,
        "re-authorizing must not add signatures"
    );

    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let first = node.with(|s| s.broadcasts[0].raw.clone());
    let nonce = ledger.get_robinhood_tx(tx.id).unwrap().unwrap().nonce;

    // Re-broadcasting sends the IDENTICAL bytes under the SAME nonce.
    for _ in 0..3 {
        settler
            .tick_broadcast(&mut ledger, 1_100, &mut report)
            .await;
    }
    let sends = node.with(|s| s.broadcasts.clone());
    assert!(
        sends.iter().all(|b| b.raw == first),
        "every send must be the same transaction"
    );
    assert_eq!(
        ledger.get_robinhood_tx(tx.id).unwrap().unwrap().nonce,
        nonce,
        "no re-broadcast may allocate a second nonce"
    );

    // And exactly one operation row exists, ever.
    let rows: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM robinhood_transactions WHERE request_id = ?1",
            [request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn the_contract_side_gate_stops_a_broadcast_that_a_service_flag_would_allow() {
    // A service-side flag is NECESSARY and NOT SUFFICIENT. Every one of
    // these is a contract state this service does not control.
    for (name, mutate) in [
        (
            "route disabled",
            Box::new(|s: &mut crate::robinhood::testkit::MockNodeState| {
                s.contract.route_enabled.insert(0x01, false);
            }) as Box<dyn Fn(&mut crate::robinhood::testkit::MockNodeState)>,
        ),
        (
            "payouts paused",
            Box::new(|s: &mut crate::robinhood::testkit::MockNodeState| {
                s.contract.payouts_paused = true;
            }),
        ),
        (
            "migrated",
            Box::new(|s: &mut crate::robinhood::testkit::MockNodeState| {
                s.contract.migrated = true;
            }),
        ),
    ] {
        let node = MockNode::new(BRIDGE);
        let settler = settler(&node);
        let mut ledger = ledger();
        let net = 970_000_000u64;
        configure_robinhood_reserve(&mut ledger, net);
        let request_id = seed_glc_to_rhn(&ledger, net);
        let mut report = SettlementReport::default();

        settler
            .tick_authorize(&mut ledger, 1_000, &mut report)
            .await;
        node.with(|s| mutate(s));
        settler
            .tick_broadcast(&mut ledger, 1_100, &mut report)
            .await;

        assert_eq!(
            node.with(|s| s.broadcasts.len()),
            0,
            "{name}: nothing may be broadcast when the contract refuses"
        );
        let tx = ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            tx.state,
            RobinhoodTxState::Authorized,
            "{name}: the operation stays authorized and un-nonced"
        );
        assert_eq!(tx.nonce, None, "{name}: no nonce may be burned");
    }
}

#[tokio::test]
async fn a_rotated_signer_epoch_invalidates_the_authorization_before_it_is_sent() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    // A rotation: every signature the outgoing set produced is now void.
    node.with(|s| s.contract.signer_epoch += 1);
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;

    assert_eq!(node.with(|s| s.broadcasts.len()), 0);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("epoch") || e.contains("rotated")),
        "the refusal must name the epoch: {:?}",
        report.errors
    );
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(tx.nonce, None);
}

#[tokio::test]
async fn an_operation_the_contract_has_already_executed_is_never_retried() {
    // The settlement witness of last resort: a broadcast this service
    // lost track of. The correct response is to record it, never to send
    // a second one.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    node.mark_executed(tx.action, tx.contract_request_id);

    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(
        node.with(|s| s.broadcasts.len()),
        0,
        "an already-executed operation must not be sent again"
    );
    let tx = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::ManualReview);
    assert!(tx.failure_reason.unwrap().contains("ALREADY EXECUTED"));
}

#[tokio::test]
async fn a_reverted_receipt_parks_both_the_transaction_and_the_request() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    let nonce = tx.nonce;

    // Mined and REVERTED.
    node.mine(tx.tx_hash.unwrap(), 200, false);
    node.with(|s| s.head = 210);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;

    let tx = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::ManualReview);
    assert_eq!(tx.receipt_status, Some(0));
    assert_eq!(tx.nonce, nonce, "the nonce is not reallocated");
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a user is owed money that did not move — that needs a human"
    );

    // A further tick must NOT retry it under a fresh nonce.
    settler
        .tick_broadcast(&mut ledger, 1_300, &mut report)
        .await;
    assert_eq!(
        node.with(|s| s.broadcasts.len()),
        1,
        "a reverted operation must never be auto-retried"
    );
}

#[tokio::test]
async fn a_missing_receipt_leaves_the_operation_pending_and_re_sends_the_same_bytes() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    // No receipt is ever mined.
    settler.tick_receipts(&mut ledger, 1_150, &mut report).await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        tx.state,
        RobinhoodTxState::Broadcast,
        "no receipt means pending — not failed, and not settled"
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

#[tokio::test]
async fn a_broadcast_that_never_resolves_becomes_an_incident_rather_than_looping() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;

    // Far past the incident threshold, still no receipt.
    let later = 1_100 + crate::robinhood::submitter::UNRESOLVED_BROADCAST_INCIDENT_SECS;
    settler
        .tick_broadcast(&mut ledger, later, &mut report)
        .await;

    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(tx.state, RobinhoodTxState::ManualReview);
    assert!(
        tx.failure_reason.unwrap().contains("Do NOT re-authorize"),
        "the operator instruction must be explicit"
    );
}

#[tokio::test]
async fn an_uncertain_broadcast_never_allocates_a_second_nonce() {
    // The transport failed: the bytes may or may not have reached the
    // node. Building a second transaction would mean that in the "already
    // arrived" case BOTH could mine.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    node.with(|s| s.send_behaviour.push_back(SendBehaviour::Transport));
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;

    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    let nonce = tx.nonce.expect("a nonce was allocated before the send");
    let raw = tx.raw_tx.clone().expect("the bytes were persisted");
    assert_eq!(
        tx.state,
        RobinhoodTxState::Broadcast,
        "an uncertain send is recorded as broadcast — the safe direction"
    );

    // Every subsequent tick re-sends the SAME bytes under the SAME nonce.
    settler
        .tick_broadcast(&mut ledger, 1_200, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(tx.nonce, Some(nonce));
    assert_eq!(tx.raw_tx, Some(raw));
}

#[tokio::test]
async fn an_expired_authorization_is_parked_rather_than_broadcast() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    // Past its expiry. The contract would revert on this; refusing here
    // means it costs no gas and no nonce.
    settler
        .tick_broadcast(&mut ledger, tx.expiry as i64 + 1, &mut report)
        .await;
    assert_eq!(node.with(|s| s.broadcasts.len()), 0);
    let tx = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::ManualReview);
    assert!(tx.failure_reason.unwrap().contains("expired"));
}

#[tokio::test]
async fn without_a_quorum_nothing_is_authorized_and_nothing_is_sent() {
    // The production posture until the remote EIP-712 signer protocol
    // ships: an empty signer pool must fail closed, loudly.
    let node = MockNode::new(BRIDGE);
    let settler = settler_with_signers(&node, vec![0]);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert!(!report.errors.is_empty(), "the refusal must be reported");
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Authorizing);

    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(node.with(|s| s.broadcasts.len()), 0);
}

// =====================================================================
// RhnToGlc — settlement ordering
// =====================================================================

#[tokio::test]
async fn a_settlement_is_never_authorized_before_the_goldcoin_payout_confirms() {
    // THE ordering property. `executeSettlement` closes the obligation
    // and destroys the refund path, so it must follow irreversible
    // Goldcoin delivery — never precede it.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_goldcoin_reserve(&mut ledger, net);
    // Only 5 confirmations, against a required depth of 6.
    let request_id = seed_rhn_to_glc_paid_out(&ledger, 42, net, 5);
    pending_obligation(&node, 42, u128::from(net) * 10_000_000_000);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert!(
        ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
            .unwrap()
            .is_none()
            || ledger
                .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
                .unwrap()
                .unwrap()
                .state
                == RobinhoodTxState::Authorizing,
        "a shallow payout must not produce an authorized settlement"
    );
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(
        node.with(|s| s.broadcasts.len()),
        0,
        "nothing may be settled while the Goldcoin payout is still shallow"
    );

    // Deepen the payout past the required depth: now it may settle.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE goldcoin_payouts SET confirmations = 6 WHERE request_id = ?1",
            [request_id],
        )
        .unwrap();
    settler
        .tick_authorize(&mut ledger, 1_200, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .expect("a settlement is authorized once the payout is deep enough");
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(tx.action, crate::robinhood::auth::ACTION_SETTLE);
    assert_eq!(tx.obligation_index, Some(42));
    assert_eq!(
        tx.recipient, None,
        "a settlement moves nothing and names no recipient"
    );
    assert_eq!(tx.amount_robinhood, None);
}

#[tokio::test]
async fn a_settlement_with_no_goldcoin_payout_at_all_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_goldcoin_reserve(&mut ledger, net);
    let request_id = seed_rhn_to_glc_paid_out(&ledger, 42, net, 6);
    // Remove the payout row entirely.
    ledger
        .conn_for_tests()
        .execute(
            "DELETE FROM goldcoin_payouts WHERE request_id = ?1",
            [request_id],
        )
        .unwrap();
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("no Goldcoin payout")),
        "{:?}",
        report.errors
    );
    assert!(ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn rhn_to_glc_settles_the_request_and_the_observation_only_after_finality() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_goldcoin_reserve(&mut ledger, net);
    let request_id = seed_rhn_to_glc_paid_out(&ledger, 42, net, 6);
    pending_obligation(&node, 42, u128::from(net) * 10_000_000_000);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .unwrap();

    node.mine(tx.tx_hash.unwrap(), 300, true);
    node.mark_executed(tx.action, tx.contract_request_id);

    // One confirmation short of the required depth of 3: block 300 at
    // head 301 is depth 2.
    node.with(|s| s.head = 301);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationConfirmed
    );

    // At depth 3: settled.
    node.with(|s| s.head = 302);
    settler.tick_receipts(&mut ledger, 1_300, &mut report).await;
    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    // The Goldcoin payout row is closed too.
    assert_eq!(
        ledger
            .get_goldcoin_payout(request_id)
            .unwrap()
            .unwrap()
            .state,
        "Completed"
    );
    // The Goldcoin reserve bookkeeping moved exactly once.
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!((balance, reserved, pending), (0, 0, 0));
}

#[tokio::test]
async fn a_reverted_settlement_needs_a_human_and_is_not_retried() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_goldcoin_reserve(&mut ledger, net);
    let request_id = seed_rhn_to_glc_paid_out(&ledger, 42, net, 6);
    pending_obligation(&node, 42, u128::from(net) * 10_000_000_000);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .unwrap();
    node.mine(tx.tx_hash.unwrap(), 300, false);
    node.with(|s| s.head = 310);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;

    assert_eq!(
        ledger.get_robinhood_tx(tx.id).unwrap().unwrap().state,
        RobinhoodTxState::ManualReview
    );
    // The obligation is still refundable after its Goldcoin payout
    // confirmed — a state that absolutely needs a human.
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    settler
        .tick_broadcast(&mut ledger, 1_300, &mut report)
        .await;
    assert_eq!(node.with(|s| s.broadcasts.len()), 1, "no auto-retry");
}

#[tokio::test]
async fn a_successful_receipt_that_emitted_no_bridge_event_is_not_treated_as_success() {
    // `status = 1` means the transaction did not revert. It does not mean
    // it did what this service intended.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();

    // A successful receipt with NO log from the bridge contract.
    let hash = tx.tx_hash.unwrap();
    node.mine(hash, 200, true);
    node.with(|s| {
        if let Some(r) = s.receipts.get_mut(&hash) {
            r.logs.clear();
        }
        s.head = 210;
    });
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;

    assert!(
        report.errors.iter().any(|e| e.contains("NO event")),
        "{:?}",
        report.errors
    );
    assert_ne!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled,
        "a request must never complete on a receipt that did not do the operation"
    );
}

#[tokio::test]
async fn a_successful_receipt_the_replay_guard_does_not_confirm_is_not_treated_as_success() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let net = 970_000_000u64;
    configure_robinhood_reserve(&mut ledger, net);
    let request_id = seed_glc_to_rhn(&ledger, net);
    let mut report = SettlementReport::default();

    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let tx = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
        .unwrap();
    // Mined with a bridge event, but `requestExecuted` still says no.
    node.mine(tx.tx_hash.unwrap(), 200, true);
    node.with(|s| s.head = 210);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;

    assert!(
        report.errors.iter().any(|e| e.contains("replay guard")),
        "{:?}",
        report.errors
    );
    assert_ne!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
}

// ------------------------------------ blocker I: a closed route pays out --
//
// The Goldcoin deposit pipeline now serves `GlcToRhn`, so a request in
// `SourceFinalized` with a real, final Goldcoin deposit behind it is a
// state this build can genuinely reach. What must NOT follow from that is
// a payout while the route is shut.

/// A `GlcToRhn` request whose deposit is final is a perfectly safe thing
/// to hold: it is observed, recorded, and left alone. The settlement loop
/// skips BOTH the authorize and the broadcast phases when the route gate
/// is closed, so no authorization is requested from any signer and no
/// transaction reaches the chain.
///
/// This is asserted at the loop, not at `tick_authorize`: the gate is
/// consulted once per tick in `daemon::run_settlement` and decides which
/// phases run at all, so testing the phase in isolation would test the
/// wrong thing.
#[tokio::test]
async fn a_source_finalized_glc_to_rhn_request_is_not_paid_out_while_the_route_is_closed() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_glc_to_rhn(&ledger, 1_000_000);

    // Run the real loop for a while with the gate CLOSED.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let stopper = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = shutdown_tx.send(true);
    });
    let ticks = crate::robinhood::daemon::run_settlement(
        &settler,
        &mut ledger,
        |_: &Ledger| false,
        crate::robinhood::daemon::RobinhoodLoopConfig {
            tick_interval: Duration::from_millis(1),
            max_backoff: Duration::from_millis(2),
        },
        shutdown_rx,
        || 1_000,
    )
    .await;
    stopper.await.unwrap();
    assert!(ticks > 0, "the loop must actually have ticked");

    // The request is untouched and, crucially, no payout was begun.
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        request.state,
        RequestState::SourceFinalized,
        "a closed route must leave a funded request exactly where it is"
    );
    assert!(
        ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
            .unwrap()
            .is_none(),
        "no payout row may be created while the route is closed"
    );
    assert!(
        node.with(|state| state.broadcasts.is_empty()),
        "nothing may reach the chain while the route is closed"
    );

    // And with the gate OPEN the same request IS picked up — so the
    // assertion above is about the gate, not about an inert fixture.
    let mut report = SettlementReport::default();
    settler
        .tick_authorize(&mut ledger, 1_000, &mut report)
        .await;
    assert!(
        ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
            .unwrap()
            .is_some(),
        "the fixture must be genuinely payable once the gate opens: {:?}",
        report.errors
    );
}

/// Blocker J's no-automatic-refund rule: a `GlcToRhn` request parked in
/// `ManualReview` is never refunded by a daemon tick — not when the route
/// is closed, and not when it is open.
///
/// Refund initiation stays an explicit operator act through
/// `glc-admin refund-glc --execute`, under the Goldcoin pause and the
/// full two-halves proof. A loop that refunded on its own would be
/// deciding to move real vault funds on a schedule, which is exactly the
/// authority the runbook reserves for a human.
#[tokio::test]
async fn a_parked_glc_to_rhn_request_is_never_refunded_by_a_settlement_tick() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_glc_to_rhn(&ledger, 1_000_000);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests
                SET state = 'ManualReview', manual_review_note = 'deposit_amount_mismatch'
              WHERE id = ?1",
            [request_id],
        )
        .unwrap();

    for route_open in [false, true] {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let stopper = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            let _ = shutdown_tx.send(true);
        });
        crate::robinhood::daemon::run_settlement(
            &settler,
            &mut ledger,
            move |_: &Ledger| route_open,
            crate::robinhood::daemon::RobinhoodLoopConfig {
                tick_interval: Duration::from_millis(1),
                max_backoff: Duration::from_millis(2),
            },
            shutdown_rx,
            || 1_000,
        )
        .await;
        stopper.await.unwrap();

        assert!(
            ledger.get_goldcoin_refund(request_id).unwrap().is_none(),
            "route_open={route_open}: no tick may create a Goldcoin refund"
        );
        assert_eq!(
            ledger.get_request(request_id).unwrap().unwrap().state,
            RequestState::ManualReview,
            "route_open={route_open}: a parked request stays parked"
        );
        assert!(
            ledger
                .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
                .unwrap()
                .is_none(),
            "route_open={route_open}: and no payout is authorized from ManualReview either"
        );
    }

    // The refund path itself still regards it as refundable — so the
    // absence above is the daemon declining to act, not the request being
    // ineligible.
    let checks = ledger.glc_refund_db_checks(request_id).unwrap();
    assert!(checks.no_robinhood_payout_started);
    assert_eq!(checks.refusal, None, "{checks:?}");
}
