use std::collections::VecDeque;
use std::sync::Mutex;

use super::*;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::rpc::{
    BlockHeader, DecodedTransaction, ListUnspentEntry, TxOut as GoldcoinTxOut,
};
use crate::goldcoin::split::{build_unsigned_split_tx, SplitPlan};
use crate::ledger::Ledger;

const SOURCE_TXID: [u8; 32] = [0xAA; 32];
const SOURCE_VOUT: u32 = 0;
const SCRIPT_PUBKEY_HEX: &str = "76a914aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa88ac";

fn sample_plan(source_txid: [u8; 32], source_vout: u32) -> SplitPlan {
    SplitPlan {
        source: VaultUtxo {
            txid: source_txid,
            vout: source_vout,
            amount_atomic: 10_000 * 100_000_000,
            script_pubkey_hex: SCRIPT_PUBKEY_HEX.to_string(),
        },
        vault_script_pubkey: crate::goldcoin::hex::decode_vec(SCRIPT_PUBKEY_HEX).unwrap(),
        output_amounts: vec![4_770 * 100_000_000, 4_770 * 100_000_000],
        fee_atomic: 100_000,
    }
}

/// Builds and signs (`Built -> Signed`) a split for `(source_txid,
/// source_vout)`, exactly as `cmd_split_vault_utxo`'s fresh-sign path
/// would have left it right before a broadcast attempt that never got a
/// definitive answer. The "signed" bytes are a stand-in — this recovery
/// path never verifies signatures, only reuses whatever raw bytes were
/// persisted, so the unsigned transaction's own serialization is a
/// perfectly good stand-in payload for tests that are entirely about
/// resubmission/idempotency/ordering, never signature validity.
fn seed_signed_split(
    ledger: &mut Ledger,
    source_txid: [u8; 32],
    source_vout: u32,
) -> (i64, String) {
    let plan = sample_plan(source_txid, source_vout);
    let unsigned_tx = build_unsigned_split_tx(&plan);
    let unsigned_hex = crate::goldcoin::hex::encode(&unsigned_tx.serialize());
    let split_id = ledger
        .record_vault_utxo_split_built(&plan, 4_770 * 100_000_000, &unsigned_hex, "test", 1_000)
        .unwrap();
    let signed_hex = unsigned_hex;
    ledger
        .record_vault_utxo_split_signed(split_id, &signed_hex, 1_010)
        .unwrap();
    (split_id, signed_hex)
}

/// A minimal `GoldcoinRpc` test double — recovery only ever calls
/// `send_raw_transaction`; every other method is unreachable from this
/// module's tests and left `unimplemented!()` so a test that accidentally
/// exercises one fails loudly rather than silently returning a fake
/// value. Responses are a scripted queue, consumed one per call — lets a
/// single test drive "fails N times then succeeds," including across
/// multiple separate `recover_stuck_vault_utxo_split` invocations
/// (simulating an operator re-running the command later).
struct TestGoldcoinRpc {
    responses: Mutex<VecDeque<Result<BroadcastOutcome, RpcError>>>,
    submitted: Mutex<Vec<String>>,
}

impl TestGoldcoinRpc {
    fn scripted(responses: Vec<Result<BroadcastOutcome, RpcError>>) -> Self {
        TestGoldcoinRpc {
            responses: Mutex::new(responses.into()),
            submitted: Mutex::new(Vec::new()),
        }
    }

    fn submitted(&self) -> Vec<String> {
        self.submitted.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.submitted.lock().unwrap().len()
    }
}

impl GoldcoinRpc for TestGoldcoinRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        unimplemented!("not exercised by split_recovery tests")
    }
    async fn get_block_hash(&self, _height: i64) -> Result<String, RpcError> {
        unimplemented!("not exercised by split_recovery tests")
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("not exercised by split_recovery tests")
    }
    async fn get_raw_transaction(&self, _txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        unimplemented!("not exercised by split_recovery tests")
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<GoldcoinTxOut>, RpcError> {
        unimplemented!("not exercised by split_recovery tests")
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        self.submitted.lock().unwrap().push(hex.to_string());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                panic!("TestGoldcoinRpc: send_raw_transaction called more times than scripted")
            })
    }
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<ListUnspentEntry>, RpcError> {
        unimplemented!("not exercised by split_recovery tests")
    }
}

fn accepted(txid: &str) -> Result<BroadcastOutcome, RpcError> {
    Ok(BroadcastOutcome::Accepted {
        txid: txid.to_string(),
    })
}

fn transport_err() -> Result<BroadcastOutcome, RpcError> {
    Err(RpcError::Transport(
        "malformed response body (HTTP 200 OK, 0 bytes): EOF while parsing a value at line 1 \
         column 0 — body starts with: \"\""
            .to_string(),
    ))
}

/// Test 1: Signed -> retry -> Broadcast.
#[tokio::test]
async fn recovery_resubmits_signed_split_and_reaches_broadcast() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (split_id, signed_hex_before) = seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let rpc = TestGoldcoinRpc::scripted(vec![accepted("node-reported-txid-ignored")]);

    let outcome =
        recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
            .await
            .unwrap();
    let SplitRecoveryOutcome::Broadcast { split_id: id, txid } = outcome else {
        panic!("expected Broadcast, got {outcome:?}")
    };
    assert_eq!(id, split_id);

    let expected_txid = crate::goldcoin::tx::txid_of_serialized(
        &crate::goldcoin::hex::decode_vec(&signed_hex_before).unwrap(),
    );
    assert_eq!(
        txid, expected_txid,
        "the recovered txid must be independently computed from the exact submitted bytes, \
         never trusted from the RPC's own reported string"
    );

    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(split.state, "Broadcast");
    assert_eq!(split.txid, Some(txid));
}

/// Test 2: RPC transport/decode failure leaves Signed; a later retry
/// recovers. `call_with_retry`'s own 3 attempts are exhausted first
/// (proving a single stuck call doesn't quietly succeed), then a
/// completely separate, later invocation succeeds.
#[tokio::test]
async fn transport_or_decode_failure_leaves_signed_and_a_later_retry_recovers() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let rpc = TestGoldcoinRpc::scripted(vec![transport_err(), transport_err(), transport_err()]);

    let err = recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SplitRecoveryError::Rpc(RpcError::Transport(_))
    ));
    assert_eq!(
        rpc.call_count(),
        3,
        "call_with_retry must have exhausted all 3 attempts"
    );
    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(
        split.state, "Signed",
        "a failed recovery attempt must leave the split exactly as it was"
    );
    assert!(split.txid.is_none());

    // A later, completely separate call (the operator re-running the
    // command after the node recovers) succeeds normally.
    let rpc2 = TestGoldcoinRpc::scripted(vec![accepted("ignored")]);
    let outcome =
        recover_stuck_vault_utxo_split(&mut ledger, &rpc2, SOURCE_TXID, SOURCE_VOUT, 3_000)
            .await
            .unwrap();
    assert!(matches!(outcome, SplitRecoveryOutcome::Broadcast { .. }));
    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(split.state, "Broadcast");
}

/// Test 3: retry does not re-sign or rebuild — the exact stored
/// `signed_tx_hex`/`unsigned_tx_hex` are byte-identical before and after,
/// and the exact same bytes are what gets submitted to the RPC.
#[tokio::test]
async fn recovery_never_resigns_or_rebuilds_the_stored_transaction() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (split_id, signed_hex_before) = seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let before = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(before.id, split_id);

    let rpc = TestGoldcoinRpc::scripted(vec![accepted("ignored")]);
    recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
        .await
        .unwrap();

    assert_eq!(
        rpc.submitted(),
        vec![signed_hex_before.clone()],
        "the exact stored signed_tx_hex must be what gets submitted, verbatim"
    );
    let after = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(after.unsigned_tx_hex, before.unsigned_tx_hex);
    assert_eq!(after.signed_tx_hex, before.signed_tx_hex);
    assert_eq!(
        after.signed_tx_hex.as_deref(),
        Some(signed_hex_before.as_str())
    );
}

/// Test 4a: an "already in chain" (-27, normalized to
/// `BroadcastOutcome::AlreadyInChain`) response is treated as a
/// successful broadcast.
#[tokio::test]
async fn already_in_chain_response_is_treated_as_successful_broadcast() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let rpc = TestGoldcoinRpc::scripted(vec![Ok(BroadcastOutcome::AlreadyInChain)]);

    let outcome =
        recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
            .await
            .unwrap();
    assert!(matches!(outcome, SplitRecoveryOutcome::Broadcast { .. }));
    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(split.state, "Broadcast");
}

/// Test 4b: an "already known"/"already have transaction" (-26,
/// normalized to `BroadcastOutcome::AlreadyInMempool`) response is ALSO
/// treated as a successful broadcast — the mempool-not-yet-mined case,
/// distinct from -27's already-mined case.
#[tokio::test]
async fn already_in_mempool_response_is_treated_as_successful_broadcast() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let rpc = TestGoldcoinRpc::scripted(vec![Ok(BroadcastOutcome::AlreadyInMempool)]);

    let outcome =
        recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
            .await
            .unwrap();
    assert!(matches!(outcome, SplitRecoveryOutcome::Broadcast { .. }));
    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(split.state, "Broadcast");
}

/// Test 5: restart recovery. A file-backed ledger, dropped and reopened
/// (simulating a daemon/CLI restart), still recovers correctly — and
/// calling recovery again after it already succeeded is a safe,
/// non-mutating, non-broadcasting no-op (`AlreadyDone`), never a second
/// RPC call.
#[tokio::test]
async fn restart_then_recovery_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    }
    // Simulated restart: a brand-new `Ledger` handle over the same
    // on-disk database.
    let mut restarted = Ledger::open(&path).unwrap();
    let rpc = TestGoldcoinRpc::scripted(vec![accepted("ignored")]);
    let outcome =
        recover_stuck_vault_utxo_split(&mut restarted, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
            .await
            .unwrap();
    assert!(matches!(outcome, SplitRecoveryOutcome::Broadcast { .. }));
    assert_eq!(rpc.call_count(), 1);

    // Calling recovery again — even after another simulated restart —
    // must never resubmit anything.
    let mut restarted_again = Ledger::open(&path).unwrap();
    let outcome2 =
        recover_stuck_vault_utxo_split(&mut restarted_again, &rpc, SOURCE_TXID, SOURCE_VOUT, 3_000)
            .await
            .unwrap();
    assert!(matches!(
        outcome2,
        SplitRecoveryOutcome::AlreadyDone { state, .. } if state == "Broadcast"
    ));
    assert_eq!(
        rpc.call_count(),
        1,
        "an already-Broadcast split must never be resubmitted"
    );
}

/// Test 6: source outpoint uniqueness — recovery never creates a second
/// split row, and the structural `UNIQUE(source_txid, source_vout)`
/// protection is untouched: attempting to build a brand-new split for
/// the same outpoint still refuses.
#[tokio::test]
async fn recovery_never_creates_a_second_split_row_for_the_same_source_outpoint() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (split_id, _) = seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let rpc = TestGoldcoinRpc::scripted(vec![accepted("ignored")]);

    recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
        .await
        .unwrap();

    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(
        split.id, split_id,
        "the SAME row must have been updated, never a new one"
    );

    let plan = sample_plan(SOURCE_TXID, SOURCE_VOUT);
    let err = ledger
        .record_vault_utxo_split_built(&plan, 4_770 * 100_000_000, "00", "second attempt", 4_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::VaultUtxoAlreadySplit { .. }),
        "the structural UNIQUE(source_txid, source_vout) protection must still refuse a second split, got {err:?}"
    );
}

#[tokio::test]
async fn missing_inputs_is_a_conflict_never_silently_retried() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    seed_signed_split(&mut ledger, SOURCE_TXID, SOURCE_VOUT);
    let rpc = TestGoldcoinRpc::scripted(vec![Ok(BroadcastOutcome::MissingInputs)]);

    let err = recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
        .await
        .unwrap_err();
    assert!(matches!(err, SplitRecoveryError::BroadcastConflict(_)));
    let split = ledger
        .get_vault_utxo_split(SOURCE_TXID, SOURCE_VOUT)
        .unwrap()
        .unwrap();
    assert_eq!(
        split.state, "Signed",
        "a conflict must never mutate the split"
    );
}

#[tokio::test]
async fn recovering_an_unknown_source_outpoint_is_refused() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let rpc = TestGoldcoinRpc::scripted(vec![]);
    let err = recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
        .await
        .unwrap_err();
    assert!(matches!(err, SplitRecoveryError::NotFound { .. }));
    assert_eq!(
        rpc.call_count(),
        0,
        "a not-found split must never contact the RPC"
    );
}

#[tokio::test]
async fn recovering_a_built_but_never_signed_split_is_refused() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let plan = sample_plan(SOURCE_TXID, SOURCE_VOUT);
    let unsigned_tx = build_unsigned_split_tx(&plan);
    let unsigned_hex = crate::goldcoin::hex::encode(&unsigned_tx.serialize());
    ledger
        .record_vault_utxo_split_built(&plan, 4_770 * 100_000_000, &unsigned_hex, "test", 1_000)
        .unwrap();
    let rpc = TestGoldcoinRpc::scripted(vec![]);

    let err = recover_stuck_vault_utxo_split(&mut ledger, &rpc, SOURCE_TXID, SOURCE_VOUT, 2_000)
        .await
        .unwrap_err();
    assert!(matches!(err, SplitRecoveryError::NotRecoverable(_, state) if state == "Built"));
    assert_eq!(
        rpc.call_count(),
        0,
        "a Built (never signed) split must never contact the RPC — out of scope for this recovery path"
    );
}
