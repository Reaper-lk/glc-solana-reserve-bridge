//! Confirmation-reconciliation tests for already-broadcast GlcToSol
//! refunds.
//!
//! The theme: this path may only ever READ the chain and, on agreement at
//! sufficient depth, advance one row. Every test therefore asserts not
//! just the outcome but that the stored evidence is untouched — the txid
//! in particular, which is the only record of where real vault money went.
//!
//! The fixture reproduces request #2477's shape: a `GlcToSol` request
//! parked on an amount mismatch, refunded, broadcast, and then left in
//! `RefundBroadcast` with a transaction that is in fact already confirmed.

use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::rpc::{DecodedScriptPubKey, DecodedVin, DecodedVout};
use crate::ledger::{
    CreateRequestOutcome, Direction, RequestAmounts, RequestState, ReserveDirection,
};

const DEPOSIT_VOUT: u32 = 1;
const PREV_TXID: [u8; 32] = [0xEA; 32];
const EXPECTED_GROSS: u64 = 2_910_000_000_000;
const OBSERVED: u64 = 2_905_000_000_000;
const SENDER_HASH: [u8; 20] = [0x5A; 20];
const INPUT_VOUT: u32 = 0;

/// Per-fixture identities. Several independent refunds coexist in one
/// ledger in the batch tests, and `goldcoin_refund_inputs` is UNIQUE on
/// `(txid, vout)`, so each fixture must fund itself from its own outpoint.
fn refund_txid(seed: u8) -> [u8; 32] {
    let mut t = [0xF7u8; 32];
    t[0] = seed;
    t
}
fn input_txid(seed: u8) -> [u8; 32] {
    let mut t = [0xCCu8; 32];
    t[0] = seed;
    t
}
fn deposit_txid(seed: u8) -> [u8; 32] {
    let mut t = [0xAAu8; 32];
    t[0] = seed;
    t
}
const INPUT_AMOUNT: u64 = 3_000_000_000_000;
const FEE: u64 = 50_000;
const REQUIRED: i64 = 6;

/// A read-only mock. It implements ONLY [`RefundConfirmationRpc`] — there
/// is deliberately no broadcast method to accidentally call, which is the
/// same guarantee the production path gets from the trait bound.
struct ReadOnlyNode {
    txs: HashMap<String, DecodedTransaction>,
    /// Transport-failure message, if the node is unreachable. Stored as a
    /// string because `RpcError` is deliberately not `Clone`.
    fail: Option<String>,
    reads: Mutex<Vec<String>>,
}

impl ReadOnlyNode {
    fn with_confirmations(confirmations: Option<i64>) -> Self {
        let mut txs = HashMap::new();
        txs.insert(
            crate::goldcoin::hex::encode(&refund_txid(0)),
            confirmed_refund_tx(confirmations),
        );
        Self {
            txs,
            fail: None,
            reads: Mutex::new(Vec::new()),
        }
    }
    /// The node is up but has never heard of the transaction — exactly
    /// what a pruned or resyncing node answers.
    fn unknown_transaction() -> Self {
        Self {
            txs: HashMap::new(),
            fail: None,
            reads: Mutex::new(Vec::new()),
        }
    }
    fn unreachable() -> Self {
        Self {
            txs: HashMap::new(),
            fail: Some("connection refused".to_string()),
            reads: Mutex::new(Vec::new()),
        }
    }
    fn tx_mut(&mut self) -> &mut DecodedTransaction {
        self.txs
            .get_mut(&crate::goldcoin::hex::encode(&refund_txid(0)))
            .unwrap()
    }
    fn read_count(&self) -> usize {
        self.reads.lock().unwrap().len()
    }
}

impl RefundConfirmationRpc for ReadOnlyNode {
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        self.reads.lock().unwrap().push(txid_hex.to_string());
        if let Some(e) = &self.fail {
            return Err(RpcError::Transport(e.clone()));
        }
        self.txs
            .get(txid_hex)
            .cloned()
            .ok_or_else(|| RpcError::Method {
                code: -5,
                message: "No such mempool or blockchain transaction".into(),
            })
    }
}

fn dvout(n: u32, value_atomic: u64, script_hex: &str) -> DecodedVout {
    DecodedVout {
        value: value_atomic as f64 / 100_000_000.0,
        n,
        script_pub_key: DecodedScriptPubKey {
            hex: script_hex.to_string(),
            kind: "pubkeyhash".to_string(),
        },
    }
}

/// The chain's view of the refund this fixture broadcast: output 0 pays
/// the sender the full observed principal, output 1 is vault change, and
/// the single input is the reserved outpoint.
fn confirmed_refund_tx(confirmations: Option<i64>) -> DecodedTransaction {
    confirmed_refund_tx_for(0, confirmations)
}

fn confirmed_refund_tx_for(seed: u8, confirmations: Option<i64>) -> DecodedTransaction {
    DecodedTransaction {
        txid: crate::goldcoin::hex::encode(&refund_txid(seed)),
        vin: vec![DecodedVin {
            txid: Some(crate::goldcoin::hex::encode(&input_txid(seed))),
            vout: Some(INPUT_VOUT),
            coinbase: None,
        }],
        vout: vec![
            dvout(
                0,
                OBSERVED,
                &crate::goldcoin::address::p2pkh_script_hex(&SENDER_HASH),
            ),
            dvout(1, INPUT_AMOUNT - OBSERVED - FEE, "a914aabb87"),
        ],
        confirmations,
    }
}

fn new_ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for d in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .configure_reserve(
                d,
                100_000_000_000_000,
                1_000,
                50_000_000_000_000,
                20_000_000_000_000,
                10_000,
                1_000,
            )
            .unwrap();
    }
    ledger
}

/// A root-vault UTXO the refund can reserve. `begin_goldcoin_refund`
/// requires its inputs to be `Available` in `vault_utxos` at reservation
/// time, exactly as production does.
fn insert_utxo(ledger: &mut Ledger, txid: [u8; 32], vout: u32, amount: u64) {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex,
                                      confirmations, first_seen_at, state)
             VALUES (?1, ?2, ?3, 'a914deadbeef87', 50, 1000, 'Available')",
            rusqlite::params![txid.as_slice(), vout, amount as i64],
        )
        .unwrap();
}

/// Drives a request all the way to `Broadcast` through the real ledger
/// transitions — the exact state #2477 has been sitting in.
fn broadcast_fixture(ledger: &mut Ledger, seed: u8) -> i64 {
    insert_utxo(ledger, input_txid(seed), INPUT_VOUT, INPUT_AMOUNT);
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            RequestAmounts {
                gross_atomic: EXPECTED_GROSS,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: EXPECTED_GROSS,
                net_destination_atomic: EXPECTED_GROSS,
            },
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected a reservation")
    };
    // Parks on the amount mismatch, exactly as the indexer does.
    ledger
        .record_glc_deposit_observed(
            request_id,
            deposit_txid(seed),
            DEPOSIT_VOUT,
            OBSERVED,
            10,
            [0xBB; 32],
            1_100,
        )
        .unwrap();
    ledger
        .begin_goldcoin_refund(
            request_id,
            OBSERVED,
            PREV_TXID,
            0,
            SENDER_HASH,
            "mfTestSenderAddress1111111111111111",
            FEE,
            &[VaultUtxo {
                txid: input_txid(seed),
                vout: INPUT_VOUT,
                amount_atomic: INPUT_AMOUNT,
                script_pubkey_hex: "a914deadbeef87".to_string(),
            }],
            "00",
            "refunding the mismatched deposit",
            "operator",
            1_200,
        )
        .unwrap();
    ledger
        .record_goldcoin_refund_signed(request_id, "00", 1_300)
        .unwrap();
    ledger
        .record_goldcoin_refund_broadcast(request_id, refund_txid(seed), 1_400)
        .unwrap();
    request_id
}

fn fixture() -> (Ledger, i64) {
    let mut ledger = new_ledger();
    let id = broadcast_fixture(&mut ledger, 0);
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::RefundBroadcast
    );
    (ledger, id)
}

fn reserved_liquidity(ledger: &Ledger) -> i64 {
    ledger
        .conn_for_tests()
        .query_row(
            "SELECT reserved_liquidity FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

/// The stored broadcast evidence must survive every path unchanged. Called
/// by every test, because a reconciliation that quietly rewrote the txid
/// would lose the only record of where real money went.
fn assert_evidence_intact(ledger: &Ledger, id: i64) {
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(
        row.txid,
        Some(refund_txid(0)),
        "the stored txid is authoritative and must never be replaced"
    );
    assert_eq!(row.refund_amount_atomic, OBSERVED);
    assert_eq!(row.refund_dest_p2pkh_hash, SENDER_HASH);
    assert_eq!(row.signed_tx_hex.as_deref(), Some("00"));
}

// ------------------------------------------------------- depth boundaries --

#[tokio::test]
async fn zero_confirmations_never_transitions() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(Some(0));

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RefundReconcileOutcome::Pending {
            confirmations: 0,
            required: REQUIRED
        }
    );
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Broadcast);
    assert_eq!(row.confirmations, 0);
    assert_eq!(
        reserved_liquidity(&ledger),
        EXPECTED_GROSS as i64,
        "capacity stays held until the refund actually confirms"
    );
    assert_evidence_intact(&ledger, id);
}

/// An unconfirmed transaction omits `confirmations` entirely on this node.
/// That is a real zero, not a missing reading — and must never be read as
/// "unknown, so assume fine".
#[tokio::test]
async fn an_absent_confirmations_field_is_treated_as_zero() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(None);

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RefundReconcileOutcome::Pending {
            confirmations: 0,
            required: REQUIRED
        }
    );
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
}

#[tokio::test]
async fn one_short_of_the_threshold_records_depth_but_does_not_transition() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(Some(REQUIRED - 1));

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RefundReconcileOutcome::Pending {
            confirmations: REQUIRED - 1,
            required: REQUIRED
        }
    );
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Broadcast);
    assert_eq!(
        row.confirmations,
        REQUIRED - 1,
        "the observed depth is recorded even when it does not yet qualify"
    );
    assert_eq!(reserved_liquidity(&ledger), EXPECTED_GROSS as i64);
    assert_evidence_intact(&ledger, id);
}

#[tokio::test]
async fn exactly_the_threshold_confirms() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(Some(REQUIRED));

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RefundReconcileOutcome::Confirmed {
            confirmations: REQUIRED
        }
    );
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Refunded);
    assert!(row.reservation_released);
    assert_eq!(row.refunded_at, Some(2_000));
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::Refunded
    );
    assert_eq!(
        reserved_liquidity(&ledger),
        0,
        "the terminal transition releases the stranded reservation"
    );
    assert_evidence_intact(&ledger, id);
}

#[tokio::test]
async fn above_the_threshold_confirms() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(Some(4_000));

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RefundReconcileOutcome::Confirmed {
            confirmations: 4_000
        }
    );
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Refunded
    );
    assert_evidence_intact(&ledger, id);
}

// --------------------------------------------------- the chain cannot answer --

#[tokio::test]
async fn an_rpc_failure_leaves_the_row_untouched() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::unreachable();

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    let RefundReconcileOutcome::Unavailable { reason } = outcome else {
        panic!("an unreachable node must never produce a verdict, got {outcome:?}")
    };
    assert!(reason.contains("connection refused"), "got: {reason}");
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Broadcast);
    assert_eq!(row.confirmations, 0, "no depth was invented");
    assert_eq!(reserved_liquidity(&ledger), EXPECTED_GROSS as i64);
    assert_evidence_intact(&ledger, id);
}

/// A node that does not know the transaction is NOT evidence the refund
/// failed — a pruned or resyncing node says exactly this about a
/// perfectly good, long-confirmed transaction.
#[tokio::test]
async fn a_transaction_the_node_does_not_know_leaves_the_row_untouched() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::unknown_transaction();

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    let RefundReconcileOutcome::Unavailable { reason } = outcome else {
        panic!("an unknown transaction must not be a verdict, got {outcome:?}")
    };
    assert!(reason.contains("No such mempool"), "got: {reason}");
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
    assert_evidence_intact(&ledger, id);
}

// ------------------------------------------------- the chain disagrees with us --

#[tokio::test]
async fn a_node_answering_about_a_different_transaction_is_refused() {
    let (mut ledger, id) = fixture();
    let mut node = ReadOnlyNode::with_confirmations(Some(100));
    node.tx_mut().txid = crate::goldcoin::hex::encode(&[0x99u8; 32]);

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    let RefundReconcileOutcome::Mismatch { reason } = outcome else {
        panic!("expected a mismatch, got {outcome:?}")
    };
    assert!(reason.contains("when asked about"), "got: {reason}");
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast,
        "a deep but WRONG transaction must never confirm this refund"
    );
    assert_eq!(reserved_liquidity(&ledger), EXPECTED_GROSS as i64);
    assert_evidence_intact(&ledger, id);
}

#[tokio::test]
async fn a_transaction_paying_the_wrong_destination_is_refused() {
    let (mut ledger, id) = fixture();
    let mut node = ReadOnlyNode::with_confirmations(Some(100));
    node.tx_mut().vout[0].script_pub_key.hex =
        crate::goldcoin::address::p2pkh_script_hex(&[0x11; 20]);

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    let RefundReconcileOutcome::Mismatch { reason } = outcome else {
        panic!("expected a mismatch, got {outcome:?}")
    };
    assert!(reason.contains("refund destination"), "got: {reason}");
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
    assert_evidence_intact(&ledger, id);
}

#[tokio::test]
async fn a_transaction_paying_the_wrong_amount_is_refused() {
    let (mut ledger, id) = fixture();
    let mut node = ReadOnlyNode::with_confirmations(Some(100));
    node.tx_mut().vout[0].value = (OBSERVED - 1) as f64 / 100_000_000.0;

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    let RefundReconcileOutcome::Mismatch { reason } = outcome else {
        panic!("expected a mismatch, got {outcome:?}")
    };
    assert!(reason.contains("recorded refund amount"), "got: {reason}");
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
}

#[tokio::test]
async fn a_transaction_spending_different_inputs_is_refused() {
    let (mut ledger, id) = fixture();
    let mut node = ReadOnlyNode::with_confirmations(Some(100));
    node.tx_mut().vin[0].txid = Some(crate::goldcoin::hex::encode(&[0x77u8; 32]));

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    let RefundReconcileOutcome::Mismatch { reason } = outcome else {
        panic!("expected a mismatch, got {outcome:?}")
    };
    assert!(
        reason.contains("not the outpoint reserved"),
        "got: {reason}"
    );
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
}

#[tokio::test]
async fn a_transaction_reporting_no_outputs_is_refused() {
    let (mut ledger, id) = fixture();
    let mut node = ReadOnlyNode::with_confirmations(Some(100));
    node.tx_mut().vout.clear();

    let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert!(matches!(outcome, RefundReconcileOutcome::Mismatch { .. }));
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
}

// ---------------------------------------------------------- idempotency --

#[tokio::test]
async fn an_already_refunded_row_is_a_safe_no_op() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(Some(REQUIRED));
    reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(reserved_liquidity(&ledger), 0);

    // The same row, seen again by a later pass at greater depth.
    let deeper = ReadOnlyNode::with_confirmations(Some(REQUIRED + 50));
    let outcome = reconcile_one(&deeper, &mut ledger, id, REQUIRED, 2_500)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RefundReconcileOutcome::AlreadyRefunded {
            confirmations: REQUIRED + 50
        }
    );
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Refunded);
    assert_eq!(row.confirmations, REQUIRED + 50, "depth still deepens");
    assert_eq!(
        row.refunded_at,
        Some(2_000),
        "the settlement timestamp is the first one, never overwritten"
    );
    assert_eq!(
        reserved_liquidity(&ledger),
        0,
        "capacity must never be released twice"
    );
}

#[tokio::test]
async fn repeated_reconciliation_is_idempotent() {
    let (mut ledger, id) = fixture();
    let node = ReadOnlyNode::with_confirmations(Some(REQUIRED + 1));

    for tick in 0..25 {
        reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000 + tick)
            .await
            .unwrap();
    }
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Refunded);
    assert_eq!(row.refunded_at, Some(2_000));
    assert_eq!(reserved_liquidity(&ledger), 0);
    assert_evidence_intact(&ledger, id);

    // Exactly one terminal transition was ever logged.
    let terminal = ledger
        .state_log(id)
        .unwrap()
        .into_iter()
        .filter(|(from, to, _, _)| {
            *from == Some(RequestState::RefundBroadcast) && *to == RequestState::Refunded
        })
        .count();
    assert_eq!(terminal, 1, "the request settled exactly once");
}

/// Crash safety: the ledger is reopened from disk between passes, which is
/// what a daemon restart actually looks like. A restart must neither
/// re-release capacity nor lose the settlement.
#[tokio::test]
async fn reconciliation_survives_a_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for d in [
            ReserveDirection::SolanaReserve,
            ReserveDirection::GoldcoinReserve,
        ] {
            ledger
                .configure_reserve(
                    d,
                    100_000_000_000_000,
                    1_000,
                    50_000_000_000_000,
                    20_000_000_000_000,
                    10_000,
                    1_000,
                )
                .unwrap();
        }
        broadcast_fixture(&mut ledger, 0)
    };

    // Pass 1: below threshold, in one process.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let node = ReadOnlyNode::with_confirmations(Some(1));
        reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
            .await
            .unwrap();
        assert_eq!(
            ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
            GoldcoinRefundState::Broadcast
        );
    }
    // Pass 2: a fresh process, now deep enough.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let node = ReadOnlyNode::with_confirmations(Some(REQUIRED));
        let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_100)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            RefundReconcileOutcome::Confirmed {
                confirmations: REQUIRED
            }
        );
        assert_eq!(reserved_liquidity(&ledger), 0);
    }
    // Pass 3: another restart, same row again.
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let node = ReadOnlyNode::with_confirmations(Some(REQUIRED + 9));
        let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_200)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            RefundReconcileOutcome::AlreadyRefunded { .. }
        ));
        assert_eq!(
            reserved_liquidity(&ledger),
            0,
            "a restart must never re-release capacity"
        );
        assert_evidence_intact(&ledger, id);
    }
}

// ------------------------------------------------------- state guards --

/// A refund that was never broadcast has no on-chain depth to reconcile,
/// and this pass must never be what advances it toward the money moving.
#[tokio::test]
async fn a_built_or_signed_refund_is_never_advanced() {
    for (state_sql, label) in [("Built", "Built"), ("Signed", "Signed")] {
        let (mut ledger, id) = fixture();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE goldcoin_refunds SET state = ?1, txid = NULL WHERE request_id = ?2",
                rusqlite::params![state_sql, id],
            )
            .unwrap();
        let node = ReadOnlyNode::with_confirmations(Some(1_000));

        let outcome = reconcile_one(&node, &mut ledger, id, REQUIRED, 2_000)
            .await
            .unwrap();
        let RefundReconcileOutcome::Unavailable { reason } = outcome else {
            panic!("{label}: expected no verdict, got {outcome:?}")
        };
        assert!(reason.contains("not broadcast"), "{label}: {reason}");
        assert_eq!(
            ledger
                .get_goldcoin_refund(id)
                .unwrap()
                .unwrap()
                .state
                .as_str(),
            state_sql,
            "{label}: the row must be untouched"
        );
        assert_eq!(
            node.read_count(),
            0,
            "{label}: a non-broadcast refund must not even be looked up on chain"
        );
    }
}

// ------------------------------------------------------------ the batch pass --

#[tokio::test]
async fn the_pass_reports_each_row_and_never_stops_on_one_bad_row() {
    let mut ledger = new_ledger();
    // Three independent broadcast refunds, each with its own txid.
    let ids: Vec<i64> = (0..3u8)
        .map(|s| broadcast_fixture(&mut ledger, s))
        .collect();

    // Row 0: deep and valid. Row 1: unknown to the node. Row 2: valid but
    // shallow. The middle failure must not hide row 2.
    let mut node = ReadOnlyNode::unknown_transaction();
    node.txs.insert(
        crate::goldcoin::hex::encode(&refund_txid(0)),
        confirmed_refund_tx_for(0, Some(REQUIRED + 3)),
    );
    node.txs.insert(
        crate::goldcoin::hex::encode(&refund_txid(2)),
        confirmed_refund_tx_for(2, Some(1)),
    );

    let (report, problems) = reconcile_broadcast_refunds(&node, &mut ledger, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(report.checked, 3);
    assert_eq!(report.confirmed, 1);
    assert_eq!(report.pending, 1);
    assert_eq!(report.unavailable, 1);
    assert_eq!(report.mismatched, 0);
    assert_eq!(problems.len(), 1);
    assert!(problems[0].contains(&format!("glc refund {}", ids[1])));

    assert_eq!(
        ledger.get_goldcoin_refund(ids[0]).unwrap().unwrap().state,
        GoldcoinRefundState::Refunded
    );
    assert_eq!(
        ledger.get_goldcoin_refund(ids[1]).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast,
        "an unreachable answer for one row must never settle it"
    );
    assert_eq!(
        ledger.get_goldcoin_refund(ids[2]).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast,
        "the shallow row behind the failing one was still checked"
    );
    assert_eq!(
        ledger
            .get_goldcoin_refund(ids[2])
            .unwrap()
            .unwrap()
            .confirmations,
        1,
        "and its observed depth was recorded"
    );

    // A second pass drains nothing new and re-reports the same problem —
    // the pass is safe to run on every tick forever.
    let (again, problems_again) = reconcile_broadcast_refunds(&node, &mut ledger, REQUIRED, 2_100)
        .await
        .unwrap();
    assert_eq!(again.checked, 2, "the settled row dropped out of the query");
    assert_eq!(again.confirmed, 0);
    assert_eq!(problems_again.len(), 1);
}

/// A mismatch is reported with an explicit instruction not to send another
/// refund — the whole failure mode this work exists to prevent.
#[tokio::test]
async fn a_mismatch_tells_the_operator_not_to_send_another_refund() {
    let mut ledger = new_ledger();
    let id = broadcast_fixture(&mut ledger, 0);
    let mut node = ReadOnlyNode::with_confirmations(Some(100));
    node.tx_mut().vout[0].value = 1.0;

    let (report, problems) = reconcile_broadcast_refunds(&node, &mut ledger, REQUIRED, 2_000)
        .await
        .unwrap();
    assert_eq!(report.mismatched, 1);
    assert_eq!(report.confirmed, 0);
    assert!(
        problems[0].contains("do NOT send another refund"),
        "got: {}",
        problems[0]
    );
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Broadcast
    );
    assert_evidence_intact(&ledger, id);
}
