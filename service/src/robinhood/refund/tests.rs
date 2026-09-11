//! Refund tests.
//!
//! A refund and a settlement are opposite, irreversible answers to the
//! same question, so most of these are about the guarantee that neither
//! can follow the other.

use super::*;
use crate::ledger::{RequestState, RobinhoodTxKind, RobinhoodTxState};
use crate::robinhood::calls::{
    Obligation, OBLIGATION_STATUS_ABANDONED, OBLIGATION_STATUS_PENDING, OBLIGATION_STATUS_REFUNDED,
    OBLIGATION_STATUS_SETTLED,
};
use crate::robinhood::settlement::{SettlementReport, Settler};
use crate::robinhood::signer::{DevEvmAuthSigner, EvmAuthSigner};
use crate::robinhood::testkit::{signer_key, submitter_key, MockNode, BRIDGE, DEPOSITOR};
use crate::robinhood::Submitter;
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

/// An `RhnToGlc` request parked in `ManualReview` — the only state a
/// refund begins from.
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
                     'robinhood', ?3, ?4, 1, 100, 'undeliverable destination')",
            rusqlite::params![
                (PRINCIPAL_CANONICAL) as i64,
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
                amount: crate::evm::EvmU256::from_u128(PRINCIPAL_18DP),
            },
        );
    });
}

#[tokio::test]
async fn a_refund_binds_the_obligations_own_depositor_and_principal() {
    // Neither the destination nor the amount is chosen — by an operator,
    // by this service, or by the signers. Both are read from the chain.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);

    let tx_id = begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .expect("a parked deposit may be refunded");

    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.kind, RobinhoodTxKind::Refund);
    assert_eq!(tx.action, crate::robinhood::auth::ACTION_REFUND);
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(
        tx.recipient,
        Some(DEPOSITOR.to_bytes()),
        "the recipient is the obligation's own depositor, read from the chain"
    );
    assert_eq!(
        crate::evm::EvmU256::from_be_bytes(tx.amount_robinhood.unwrap()),
        crate::evm::EvmU256::from_u128(PRINCIPAL_18DP),
        "the amount is the obligation's own principal — exactly, no fee, no partial"
    );
    // And the request has entered the one-way refund lifecycle.
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::RefundPending
    );
}

#[tokio::test]
async fn a_refund_is_never_the_ledgers_gross_or_net() {
    // The request's own accounting is 500 GLC gross with a fee withheld.
    // A refund returns the PRINCIPAL, which is the gross — and it is
    // taken from the chain, not from either ledger column.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);
    let request = ledger.get_request(request_id).unwrap().unwrap();

    let tx_id = begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .unwrap();
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    let refunded = crate::evm::EvmU256::from_be_bytes(tx.amount_robinhood.unwrap())
        .try_to_u128()
        .unwrap();
    assert_eq!(refunded, PRINCIPAL_18DP);
    assert_ne!(
        refunded,
        u128::from(request.net_amount_atomic) * 10_000_000_000,
        "a refund is never the post-fee net"
    );
}

#[tokio::test]
async fn a_settled_request_can_never_be_refunded() {
    // Ledger-side mutual exclusion, checked against the settlement row.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);

    // Plant a settlement row for the same request.
    ledger
        .begin_robinhood_tx(
            &crate::ledger::NewRobinhoodTx {
                kind: RobinhoodTxKind::Settlement,
                request_id: Some(request_id),
                rebalance_request_id: None,
                route: Some(Route::RhnToGlc),
                bridge_contract: BRIDGE.to_bytes(),
                chain_id: 4663,
                contract_request_id: [0x11; 32],
                obligation_index: Some(42),
                recipient: None,
                amount_robinhood: None,
                signer_epoch: 7,
                expiry: 1_800_000_000,
                auth_digest: [0x22; 32],
            },
            900,
        )
        .unwrap();

    assert!(matches!(
        begin_refund(&settler, &mut ledger, request_id, 1_000).await,
        Err(RefundError::AlreadySettled { .. })
    ));
}

#[tokio::test]
async fn a_request_with_a_broadcast_goldcoin_payout_can_never_be_refunded() {
    // Second ledger-side check, against a DIFFERENT table: a Goldcoin
    // payout that reached the chain means the depositor was paid.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, txid, state, built_at)
             VALUES (?1, X'00', 1, 0, 0, X'00', X'9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c',
                     'Broadcast', 100)",
            [request_id],
        )
        .unwrap();
    assert!(matches!(
        begin_refund(&settler, &mut ledger, request_id, 1_000).await,
        Err(RefundError::AlreadyPaidOut { .. })
    ));
}

#[tokio::test]
async fn an_obligation_that_is_not_pending_on_chain_can_never_be_refunded() {
    // Third check, against the CHAIN: the contract itself would revert,
    // and refusing here means it costs no gas and no nonce.
    for status in [
        OBLIGATION_STATUS_SETTLED,
        OBLIGATION_STATUS_REFUNDED,
        OBLIGATION_STATUS_ABANDONED,
    ] {
        let node = MockNode::new(BRIDGE);
        let settler = settler(&node);
        let mut ledger = ledger();
        let request_id = parked_request(&ledger, 42);
        obligation(&node, 42, status);
        let err = begin_refund(&settler, &mut ledger, request_id, 1_000)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RefundError::ObligationNotPending { .. }),
            "status {status}: {err}"
        );
    }
}

#[tokio::test]
async fn a_refund_is_not_reachable_from_a_request_that_is_mid_payout() {
    // A refund is a deliberate decision made about a PARKED deposit, not
    // something that races an in-flight payout.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);
    for state in [
        "SourceFinalized",
        "DestinationSubmitted",
        "DestinationConfirmed",
        "Settled",
    ] {
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE bridge_requests SET state = ?2 WHERE id = ?1",
                rusqlite::params![request_id, state],
            )
            .unwrap();
        assert!(
            matches!(
                begin_refund(&settler, &mut ledger, request_id, 1_000).await,
                Err(RefundError::NotRefundable { .. })
            ),
            "a refund must not begin from {state}"
        );
    }
}

#[tokio::test]
async fn a_glc_to_rhn_request_cannot_be_refunded_on_robinhood() {
    // Its refund belongs on the GOLDCOIN side, through the existing
    // GlcToSol refund lifecycle.
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain)
             VALUES ('GlcToRhn', 'ManualReview', 100, 300, 3, 97, 97, X'ab', 100, 'goldcoin')",
            [],
        )
        .unwrap();
    let request_id = ledger.conn_for_tests().last_insert_rowid();
    assert!(matches!(
        begin_refund(&settler, &mut ledger, request_id, 1_000).await,
        Err(RefundError::WrongDirection { .. })
    ));
}

#[tokio::test]
async fn beginning_a_refund_twice_resumes_the_same_one() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);

    let first = begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            begin_refund(&settler, &mut ledger, request_id, 2_000)
                .await
                .unwrap(),
            first,
            "a second call must resume, never create a second refund"
        );
    }
    let count: i64 = ledger
        .conn_for_tests()
        .query_row(
            "SELECT COUNT(*) FROM robinhood_transactions WHERE kind = 'Refund'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_refund_broadcasts_confirms_and_reaches_the_terminal_refunded_state() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);
    let tx_id = begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .unwrap();

    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    assert_eq!(report.errors, Vec::<String>::new());
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Broadcast);

    node.mine(tx.tx_hash.unwrap(), 400, true);
    node.mark_executed(tx.action, tx.contract_request_id);
    node.with(|s| s.head = 402);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;

    assert_eq!(report.errors, Vec::<String>::new());
    assert_eq!(
        ledger.get_robinhood_tx(tx_id).unwrap().unwrap().state,
        RobinhoodTxState::Finalized
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Refunded
    );
}

#[tokio::test]
async fn a_refunded_request_can_never_then_be_settled() {
    // The one-way refund lifecycle. Even with a settlement row forced
    // into place, the terminal transition refuses.
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    ledger
        .mark_robinhood_refund_pending(request_id, 1_000)
        .unwrap();
    ledger
        .mark_robinhood_refund_confirmed(request_id, 1_100)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Refunded
    );
    assert!(matches!(
        ledger.mark_robinhood_settlement_confirmed(request_id, 1_200),
        Err(crate::ledger::LedgerError::RobinhoodTxInvalid { .. })
    ));
}

#[tokio::test]
async fn a_settled_request_can_never_then_be_refunded() {
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'DestinationConfirmed' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (?1, X'00', 1, 0, 0, X'00', 'Confirmed', 100)",
            [request_id],
        )
        .unwrap();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            PRINCIPAL_CANONICAL,
            0,
            PRINCIPAL_CANONICAL,
            1,
            1,
            100,
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET reserved_liquidity = ?1, pending_obligations = ?1,
                total_reserve_balance = ?1 WHERE direction = 'GoldcoinReserve'",
            [(PRINCIPAL_CANONICAL - 1_500_000_000) as i64],
        )
        .unwrap();
    ledger
        .mark_robinhood_settlement_confirmed(request_id, 1_000)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    assert!(matches!(
        ledger.mark_robinhood_refund_pending(request_id, 1_100),
        Err(crate::ledger::LedgerError::RobinhoodTxInvalid { .. })
    ));
}

#[tokio::test]
async fn a_reverted_refund_needs_a_human() {
    let node = MockNode::new(BRIDGE);
    let settler = settler(&node);
    let mut ledger = ledger();
    let request_id = parked_request(&ledger, 42);
    obligation(&node, 42, OBLIGATION_STATUS_PENDING);
    let tx_id = begin_refund(&settler, &mut ledger, request_id, 1_000)
        .await
        .unwrap();
    let mut report = SettlementReport::default();
    settler
        .tick_broadcast(&mut ledger, 1_100, &mut report)
        .await;
    let tx = ledger.get_robinhood_tx(tx_id).unwrap().unwrap();
    node.mine(tx.tx_hash.unwrap(), 400, false);
    node.with(|s| s.head = 410);
    settler.tick_receipts(&mut ledger, 1_200, &mut report).await;

    assert_eq!(
        ledger.get_robinhood_tx(tx_id).unwrap().unwrap().state,
        RobinhoodTxState::ManualReview
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a depositor's principal is still held — that needs a human"
    );
}
