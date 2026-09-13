//! The chain-terminal settlement reconciliation, exercised on the exact
//! shape of request 4244 / V2 obligation #57 (2026-09-13) and on every
//! way the proof must refuse.

use std::time::Duration;

use super::*;
use crate::ledger::{RequestState, RobinhoodTx, RobinhoodTxKind, RobinhoodTxState};
use crate::robinhood::auth::ACTION_SETTLE;
use crate::robinhood::calls::{Obligation, OBLIGATION_STATUS_PENDING, OBLIGATION_STATUS_SETTLED};
use crate::robinhood::settlement::{SettlementReport, Settler};
use crate::robinhood::signer::{DevEvmAuthSigner, EvmAuthSigner};
use crate::robinhood::testkit::{signer_key, submitter_key, MockNode, BRIDGE, DEPOSITOR};
use crate::robinhood::Submitter;

const OBLIGATION: u64 = 57;
const NET: u64 = 1_880_000_000_000; // 18,880 GLC canonical 8dp — request 4244's net
const GROSS: u64 = 2_000_000_000_000;
const GLC_TXID: [u8; 32] = [0xd4; 32];
const DEST_HASH: [u8; 20] = [0x5a; 20];
const REQUIRED_GOLDCOIN_DEPTH: i64 = 6;
const REQUIRED_RHN_DEPTH: i64 = 3;

fn ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            NET,
            0,
            NET,
            NET / 2,
            NET / 4,
            100,
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = ?1, reserved_liquidity = ?1,
                pending_obligations = ?1 WHERE direction = 'GoldcoinReserve'",
            [NET as i64],
        )
        .unwrap();
    ledger
}

fn settler(node: &MockNode) -> Settler<MockNode> {
    let config = node.settlement_config();
    let signers: Vec<Box<dyn EvmAuthSigner>> = (0u8..3)
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
        REQUIRED_GOLDCOIN_DEPTH,
        crate::amount_conversion::BRIDGE_FEE_BPS,
    )
}

/// Request 4244's local shape: an RhnToGlc request whose Goldcoin payout
/// (18,880 GLC net to a real P2PKH address) confirmed at depth, bound to
/// the configured contract and to obligation #57.
fn seed_request(ledger: &Ledger, obligation: u64) -> i64 {
    let recipient = crate::goldcoin::address::encode_p2pkh(
        &DEST_HASH,
        crate::goldcoin::address::Network::Testnet,
    );
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, destination_txid, destination_confirmations)
             VALUES ('RhnToGlc', 'DestinationConfirmed', ?1, 600, ?2, ?3, ?3, ?4, ?5, 100,
                     'robinhood', ?6, ?7, 1, 100, ?8, 6)",
            rusqlite::params![
                GROSS as i64,
                (GROSS - NET) as i64,
                NET as i64,
                recipient.as_bytes(),
                &DEPOSITOR.to_bytes()[..],
                &BRIDGE.to_bytes()[..],
                obligation as i64,
                &GLC_TXID[..],
            ],
        )
        .unwrap();
    let request_id = ledger.conn_for_tests().last_insert_rowid();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, txid, state, built_at, confirmations)
             VALUES (?1, X'00', ?2, 0, 0, ?3, ?4, 'Confirmed', 100, 6)",
            rusqlite::params![request_id, NET as i64, &DEST_HASH[..], &GLC_TXID[..]],
        )
        .unwrap();
    request_id
}

fn pending_obligation(node: &MockNode, index: u64) {
    node.with(|s| {
        s.contract.obligation_count = s.contract.obligation_count.max(index + 1);
        s.contract.obligations.insert(
            index,
            Obligation {
                depositor: DEPOSITOR,
                status: OBLIGATION_STATUS_PENDING,
                route: 0x02,
                amount: crate::evm::EvmU256::from_u128(u128::from(GROSS) * 10_000_000_000),
            },
        );
    });
}

/// Authorizes and broadcasts the settlement, then burns the whole
/// replacement budget WITHOUT any receipt — request 4244's history
/// (broadcast_attempts 4, replacement_attempts 3). Returns the row as it
/// stood when the budget ran out, i.e. holding the LAST replacement's
/// hash, and the hash of the ORIGINAL broadcast.
async fn broadcast_and_exhaust(
    settler: &Settler<MockNode>,
    ledger: &mut Ledger,
    request_id: i64,
) -> (RobinhoodTx, [u8; 32]) {
    let mut report = SettlementReport::default();
    settler.tick_authorize(ledger, 1_000, &mut report).await;
    settler.tick_broadcast(ledger, 1_100, &mut report).await;
    let first = ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Settlement, request_id)
        .unwrap()
        .unwrap();
    assert_eq!(first.state, RobinhoodTxState::Broadcast);
    let original_hash = first.tx_hash.unwrap();
    // Three replacements, 120 s apart (the mock config's rebroadcast_after).
    for i in 1..=3 {
        settler
            .tick_broadcast(ledger, 1_100 + 120 * i, &mut report)
            .await;
    }
    let tx = ledger.get_robinhood_tx(first.id).unwrap().unwrap();
    assert_eq!(tx.replacement_attempts, 3);
    assert_eq!(tx.broadcast_attempts, 4);
    assert_ne!(
        tx.tx_hash.unwrap(),
        original_hash,
        "the row tracks the last replacement"
    );
    (tx, original_hash)
}

/// The chain's side of 4244: the ORIGINAL broadcast (nonce n) mined
/// successfully in `block`, the obligation Settled, the replay guard
/// executed, one ObligationSettled event.
fn chain_settles(node: &MockNode, tx: &RobinhoodTx, landed_hash: [u8; 32], block: u64) {
    node.mine(landed_hash, block, true);
    node.mark_executed(ACTION_SETTLE, tx.contract_request_id);
    node.with(|s| {
        s.contract.obligations.get_mut(&OBLIGATION).unwrap().status = OBLIGATION_STATUS_SETTLED;
        s.head = block + REQUIRED_RHN_DEPTH as u64;
    });
    let mut index_word = [0u8; 32];
    index_word[24..].copy_from_slice(&OBLIGATION.to_be_bytes());
    node.push_log(
        landed_hash,
        block,
        vec![
            obligation_settled_topic(),
            index_word,
            tx.contract_request_id,
        ],
    );
}

async fn prove_for(node: &MockNode, ledger: &mut Ledger, request_id: i64) -> ReconcileReport {
    prove(
        node,
        ledger,
        &node.verified_deployment(),
        submitter_key().address(),
        REQUIRED_GOLDCOIN_DEPTH,
        REQUIRED_RHN_DEPTH,
        request_id,
    )
    .await
    .unwrap()
}

fn refusal(report: &ReconcileReport) -> String {
    match &report.verdict {
        Verdict::Refuse(r) => r.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// The exact 4244 failure and its repair: the row parks with
/// ReplacementBudgetExhausted while the chain is silent; later the chain
/// proves the original broadcast executed; the command reconciles it
/// once through the normal completion body; a second run writes nothing.
#[tokio::test]
async fn request_4244_parked_by_an_exhausted_budget_is_reconciled_from_chain_evidence_once() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, original_hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;

    // Budget exhausted, chain still silent: parked — with the proof's
    // refusal spelled out, not a bare "budget exhausted".
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 1_600, &mut report)
        .await;
    let parked = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(parked.state, RobinhoodTxState::ManualReview);
    let reason = parked.failure_reason.clone().unwrap();
    assert!(reason.contains("replaced 3 times"), "{reason}");
    assert!(reason.contains("Chain-terminal proof"), "{reason}");
    assert!(reason.contains("not Settled"), "{reason}");
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationConfirmed
    );

    // The chain, as it really was: nonce 109 executed.
    chain_settles(&node, &tx, original_hash, 300);

    // Dry run: SAFE_TO_RECONCILE, nothing written.
    let before_log = ledger.state_log(request_id).unwrap().len();
    let report = prove_for(&node, &mut ledger, request_id).await;
    let Verdict::SafeToReconcile(proof) = report.verdict.clone() else {
        panic!("{}", report.render())
    };
    assert_eq!(proof.chain_tx_hash, original_hash);
    assert_eq!(proof.nonce, tx.nonce.unwrap());
    assert_eq!(proof.obligation_index, OBLIGATION);
    assert_eq!(report.chain_state, "Settled");
    assert_eq!(report.local_settlement_state, "ManualReview");
    assert_eq!(report.refunds, "none");
    assert_eq!(ledger.state_log(request_id).unwrap().len(), before_log);
    assert_eq!(
        ledger.get_robinhood_tx(tx.id).unwrap().unwrap().state,
        RobinhoodTxState::ManualReview
    );
    let rendered = report.render();
    assert!(rendered.contains("SAFE_TO_RECONCILE"), "{rendered}");

    // Execute (through the audited wrapper the CLI uses).
    let receipt =
        crate::admin_api::audited_robinhood_reconcile_settlement(&mut ledger, &proof, "cli:ops")
            .unwrap();
    assert_eq!(receipt.old_value.as_deref(), Some("DestinationConfirmed"));
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::Settled);
    let settled_at: Option<i64> = ledger
        .conn_for_tests()
        .query_row(
            "SELECT settled_at FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(settled_at.is_some());
    let row = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(row.state, RobinhoodTxState::Finalized);
    assert_eq!(
        row.tx_hash,
        Some(original_hash),
        "the LANDED hash, not the last replacement"
    );
    assert_eq!(row.receipt_status, Some(1));
    assert_eq!(row.receipt_block_number, Some(300));
    assert!(row.finalized_at.is_some());
    assert!(row
        .failure_reason
        .unwrap()
        .starts_with("chain_terminal_reconciliation"));
    let log = ledger.state_log(request_id).unwrap();
    let last = log.last().unwrap();
    assert_eq!(last.1, RequestState::Settled);
    assert_eq!(
        last.3.as_deref(),
        Some(Ledger::CHAIN_TERMINAL_RECONCILIATION_REASON)
    );
    assert_eq!(
        ledger
            .get_goldcoin_payout(request_id)
            .unwrap()
            .unwrap()
            .state,
        "Completed"
    );
    // Reserve bookkeeping: exactly what a normal completion does.
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!((balance, reserved, pending), (0, 0, 0));

    // Run #2: ALREADY_RECONCILED, zero writes.
    let log_len = ledger.state_log(request_id).unwrap().len();
    let updated_at = ledger.get_robinhood_tx(tx.id).unwrap().unwrap().updated_at;
    let again = prove_for(&node, &mut ledger, request_id).await;
    assert_eq!(again.verdict, Verdict::AlreadyReconciled);
    assert_eq!(
        apply(&mut ledger, &proof, "cli:ops", 9_999).unwrap(),
        crate::ledger::ReconcileOutcome::AlreadyReconciled
    );
    assert_eq!(ledger.state_log(request_id).unwrap().len(), log_len);
    assert_eq!(
        ledger.get_robinhood_tx(tx.id).unwrap().unwrap().updated_at,
        updated_at
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
}

/// The daemon-side fix: when the budget runs out and the chain ALREADY
/// shows the settlement executed, the row is completed on the spot —
/// never parked.
#[tokio::test]
async fn chain_settled_before_the_budget_ran_out_completes_instead_of_parking() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, original_hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, original_hash, 300);

    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 1_600, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let row = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    assert_eq!(row.state, RobinhoodTxState::Finalized);
    assert_eq!(row.tx_hash, Some(original_hash));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    let last = ledger
        .state_log(request_id)
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        last.3.as_deref(),
        Some(Ledger::CHAIN_TERMINAL_RECONCILIATION_REASON)
    );
    // Nothing further to do on later ticks; no second nonce was ever used.
    settler
        .tick_broadcast(&mut ledger, 1_800, &mut report)
        .await;
    assert_eq!(node.with(|s| s.broadcasts.len()), 4);
}

/// The AlreadyExecuted gate on re-entry (a lost broadcast write): a
/// settlement is completed through the proof, not parked.
#[tokio::test]
async fn an_already_executed_settlement_on_re_entry_is_completed_not_parked() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
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
    let hash = tx.tx_hash.unwrap();
    chain_settles(&node, &tx, hash, 300);
    // Simulate the lost local write: the row is back to Authorized (as
    // if `record_signed/broadcast` never landed) — the driver re-enters
    // sign_and_send and the gate answers AlreadyExecuted.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_transactions SET state = 'Authorized', tx_hash = NULL, raw_tx = NULL,
                    envelope = NULL, first_broadcast_at = NULL, last_broadcast_at = NULL,
                    broadcast_attempts = 0
              WHERE id = ?1",
            [tx.id],
        )
        .unwrap();
    settler
        .tick_broadcast(&mut ledger, 1_300, &mut report)
        .await;
    let row = ledger.get_robinhood_tx(tx.id).unwrap().unwrap();
    // raw_tx was wiped by the simulated loss, so the proof refuses on
    // "no signed transaction persisted" and the row is parked WITH the
    // refusal — the operator sees exactly what to check.
    assert_eq!(row.state, RobinhoodTxState::ManualReview);
    assert!(row
        .failure_reason
        .clone()
        .unwrap()
        .contains("ALREADY EXECUTED"));
    let reason = row.failure_reason.unwrap();
    assert!(
        reason.contains("never allocated a nonce") || reason.contains("no signed transaction"),
        "{reason}"
    );
}

#[tokio::test]
async fn wrong_request_id_is_refused() {
    let node = MockNode::new(BRIDGE);
    let mut ledger = ledger();
    let report = prove_for(&node, &mut ledger, 4244).await;
    assert!(refusal(&report).contains("does not exist"));
}

#[tokio::test]
async fn wrong_obligation_event_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, hash, 300);
    // Replace the event with one for another obligation index.
    node.with(|s| s.logs.clear());
    let mut other = [0u8; 32];
    other[31] = 58;
    node.push_log(
        hash,
        300,
        vec![obligation_settled_topic(), other, tx.contract_request_id],
    );
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("no ObligationSettled(57"),
        "{}",
        report.render()
    );
}

#[tokio::test]
async fn wrong_contract_is_refused_before_any_chain_read() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, hash, 300);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET source_contract = ?1 WHERE id = ?2",
            rusqlite::params![&[0x17u8; 20][..], request_id],
        )
        .unwrap();
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("recorded under contract"),
        "{}",
        report.render()
    );
}

#[tokio::test]
async fn wrong_submitter_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, hash, 300);
    node.set_receipt_from(hash, crate::evm::EvmAddress::from_bytes([0x77; 20]));
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("not the configured submitter"),
        "{}",
        report.render()
    );
}

/// Two of the duplicates the proof checks for are schema invariants —
/// one settlement row per request (`UNIQUE(kind, request_id)`) and one
/// request per `(chain, contract, obligation)` — so they cannot even be
/// seeded. The one that CAN exist, a second operation row carrying the
/// same requestId under another request, is refused.
#[tokio::test]
async fn a_duplicate_settlement_row_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, hash, 300);
    assert!(
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO bridge_requests (direction, state, gross_amount_atomic, fee_bps,
                    fee_amount_atomic, net_amount_atomic, net_destination_atomic, recipient,
                    created_at, source_chain, source_contract, source_obligation_index)
                 VALUES ('RhnToGlc', 'ManualReview', 1, 0, 0, 1, 1, X'00', 1, 'robinhood', ?1, ?2)",
                rusqlite::params![&BRIDGE.to_bytes()[..], OBLIGATION as i64],
            )
            .is_err(),
        "a second request for the same obligation is refused by the schema itself"
    );
    // A second operation row with this settlement's requestId, under an
    // unrelated request (on another contract so the UNIQUE index allows it).
    let other = seed_request(&ledger, OBLIGATION + 1);
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_transactions (kind, request_id, route, bridge_contract, chain_id,
                action, contract_request_id, obligation_index, signer_epoch, expiry, auth_digest,
                state, created_at, updated_at)
             SELECT kind, ?2, route, X'1717171717171717171717171717171717171717', chain_id,
                    action, contract_request_id, obligation_index, signer_epoch, expiry,
                    auth_digest, 'Authorizing', created_at, updated_at
               FROM robinhood_transactions WHERE id = ?1",
            rusqlite::params![tx.id, other],
        )
        .unwrap();
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("duplicate operation"),
        "{}",
        report.render()
    );
}

#[tokio::test]
async fn a_refund_or_closure_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, hash, 300);
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO request_closures (request_id, disposition, reference, note, actor,
                closed_at, from_state, manual_review_disposition)
             VALUES (?1, 'refunded_out_of_band', 'x', 'n', 'a', 1, 'ManualReview', 'normal')",
            [request_id],
        )
        .unwrap();
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("request closure"),
        "{}",
        report.render()
    );
    ledger
        .conn_for_tests()
        .execute(
            "DELETE FROM request_closures WHERE request_id = ?1",
            [request_id],
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'RefundPending' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("refund lifecycle"),
        "{}",
        report.render()
    );
}

#[tokio::test]
async fn a_missing_or_mismatched_destination_payout_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    chain_settles(&node, &tx, hash, 300);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE goldcoin_payouts SET payout_atomic = payout_atomic - 1 WHERE request_id = ?1",
            [request_id],
        )
        .unwrap();
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("payout amount"),
        "{}",
        report.render()
    );
    ledger
        .conn_for_tests()
        .execute(
            "DELETE FROM goldcoin_payouts WHERE request_id = ?1",
            [request_id],
        )
        .unwrap();
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("no Goldcoin payout row"),
        "{}",
        report.render()
    );
}

#[tokio::test]
async fn a_pending_obligation_or_unexecuted_guard_is_refused() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = seed_request(&ledger, OBLIGATION);
    pending_obligation(&node, OBLIGATION);
    let (tx, hash) = broadcast_and_exhaust(&settler, &mut ledger, request_id).await;
    // Chain silent: obligation Pending.
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("not Settled"),
        "{}",
        report.render()
    );
    // Obligation Settled but the replay guard does not agree: refused
    // (AlreadyExecuted-style evidence is required in FULL).
    node.mine(hash, 300, true);
    node.with(|s| {
        s.contract.obligations.get_mut(&OBLIGATION).unwrap().status = OBLIGATION_STATUS_SETTLED
    });
    let report = prove_for(&node, &mut ledger, request_id).await;
    assert!(
        refusal(&report).contains("requestExecuted"),
        "{}",
        report.render()
    );
    let _ = tx;
}
