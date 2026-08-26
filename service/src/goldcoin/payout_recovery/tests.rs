use sha2::{Digest, Sha256};

use super::*;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::multisig;
use crate::goldcoin::rpc::{
    BlockHeader, DecodedTransaction, ListUnspentEntry, RpcError, TxOut as GoldcoinTxOut,
};
use crate::ledger::{Ledger, ReserveDirection, SolFoldOutcome};
use crate::signing::goldcoin_vault::DevLedgerPayoutSource;
use crate::signing::signers::VaultSigner;

const TEST_SOLANA_DECIMALS: u8 = 6;
const TEST_SIGNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const TEST_THRESHOLD: usize = 2;

fn test_policy() -> crate::goldcoin::payout::PayoutPolicy {
    crate::goldcoin::payout::PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
    }
}

fn vault_and_signers() -> (MultisigVault, Vec<Box<dyn VaultSigner>>) {
    let signers = vec![
        crate::signing::goldcoin_vault::DevVaultSigner::generate(),
        crate::signing::goldcoin_vault::DevVaultSigner::generate(),
        crate::signing::goldcoin_vault::DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap();
    let boxed: Vec<Box<dyn VaultSigner>> = signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect();
    (vault, boxed)
}

/// A minimal `GoldcoinRpc` test double — recovery only ever calls
/// `send_raw_transaction`; every other method is unreachable from this
/// module's tests and left `unimplemented!()` so a test that accidentally
/// exercises one fails loudly rather than silently returning a fake
/// value.
struct TestGoldcoinRpc {
    behavior: std::sync::Mutex<BroadcastBehavior>,
    broadcasts: std::sync::Mutex<Vec<String>>,
}

enum BroadcastBehavior {
    Accept,
    RejectNonCanonical,
    MissingInputs,
}

impl TestGoldcoinRpc {
    fn new(behavior: BroadcastBehavior) -> Self {
        TestGoldcoinRpc {
            behavior: std::sync::Mutex::new(behavior),
            broadcasts: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn broadcasts(&self) -> Vec<String> {
        self.broadcasts.lock().unwrap().clone()
    }
}

impl GoldcoinRpc for TestGoldcoinRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        unimplemented!("not exercised by payout_recovery tests")
    }
    async fn get_block_hash(&self, _height: i64) -> Result<String, RpcError> {
        unimplemented!("not exercised by payout_recovery tests")
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("not exercised by payout_recovery tests")
    }
    async fn get_raw_transaction(&self, _txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        unimplemented!("not exercised by payout_recovery tests")
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<GoldcoinTxOut>, RpcError> {
        unimplemented!("not exercised by payout_recovery tests")
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        self.broadcasts.lock().unwrap().push(hex.to_string());
        match *self.behavior.lock().unwrap() {
            BroadcastBehavior::Accept => Ok(BroadcastOutcome::Accepted {
                txid: "test-txid".to_string(),
            }),
            BroadcastBehavior::RejectNonCanonical => Err(RpcError::Method {
                code: -26,
                message: "64: non-mandatory-script-verify-flag (Non-canonical signature: S value is unnecessarily high)".to_string(),
            }),
            BroadcastBehavior::MissingInputs => Ok(BroadcastOutcome::MissingInputs),
        }
    }
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<ListUnspentEntry>, RpcError> {
        unimplemented!("not exercised by payout_recovery tests")
    }
}

fn configure_reserves(ledger: &mut Ledger) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            100_000_000,
            0,
            50_000_000,
            20_000_000,
            10_000_000,
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
}

/// Drives a ledger's `SolToGlc` request through exactly the same
/// functions the real orchestrator uses
/// (`independently_sign_all_inputs`/`multisig::assemble`/the ledger's own
/// `record_goldcoin_payout_{built,signed}`) up through `Signed` and stops
/// there — never broadcasting — modeling exactly "the RPC call after
/// signing failed," the precondition request #8 was found stuck in.
async fn ledger_with_stuck_signed_payout(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    amount: u64,
    dest_addr: &str,
) -> (i64, String) {
    configure_reserves(ledger);

    let goldcoin_atomic =
        crate::amount_conversion::solana_to_goldcoin_atomic(amount, TEST_SOLANA_DECIMALS).unwrap();
    let utxo = VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic: goldcoin_atomic + 100_000,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    ledger
        .sync_vault_utxos(&[(utxo, 10, vault.script_pubkey_hex())], 1, 0)
        .unwrap();

    let gross_canonical = crate::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let fb = crate::amount_conversion::compute_fee(gross_canonical).unwrap();
    let amounts = crate::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    };
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts, [7u8; 32], dest_addr.as_bytes(), 0)
        .unwrap()
    else {
        panic!("expected the deposit to fold straight to SourceFinalized")
    };

    let source = DevLedgerPayoutSource { ledger };
    let (plan, mut tx, partials) = independently_sign_all_inputs(
        vault_signers,
        vault,
        &source,
        request_id,
        TEST_THRESHOLD,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();

    let commitment_hash: [u8; 32] = Sha256::digest(tx.serialize()).into();
    let unsigned_hex = crate::goldcoin::hex::encode(&tx.serialize());
    ledger
        .reserve_vault_utxos(request_id, &plan.inputs, 0)
        .unwrap();
    ledger
        .record_goldcoin_payout_built(request_id, &plan, commitment_hash, &unsigned_hex, 0)
        .unwrap();

    for (input_index, input_partials) in partials.iter().enumerate() {
        let input_vault = &plan.input_contexts[input_index].vault;
        let sighash = tx.sighash_all(input_index, &input_vault.redeem_script());
        tx.inputs[input_index].script_sig =
            multisig::assemble(input_vault, &sighash, input_partials).unwrap();
    }
    let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
    ledger
        .record_goldcoin_payout_signed(request_id, &signed_hex, 0)
        .unwrap();

    (request_id, signed_hex)
}

#[allow(clippy::too_many_arguments)]
async fn run_recovery(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    rpc: &TestGoldcoinRpc,
    request_id: i64,
    now: i64,
) -> Result<RecoveryOutcome, RecoveryError> {
    recover_stuck_goldcoin_payout(
        ledger,
        vault,
        vault_signers,
        rpc,
        request_id,
        TEST_THRESHOLD,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
        now,
    )
    .await
}

#[tokio::test]
async fn recovers_a_stuck_signed_payout_to_broadcast() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (request_id, original_signed_hex) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;

    // Sanity: this really is the "stuck" precondition before recovery runs.
    let before = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(before.state, "Signed");
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        crate::ledger::RequestState::SettlementAuthorized
    );

    let rpc = TestGoldcoinRpc::new(BroadcastBehavior::Accept);
    let outcome = run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 100)
        .await
        .unwrap();

    match outcome {
        RecoveryOutcome::Broadcast { .. } => {}
        other => panic!("expected Broadcast, got {other:?}"),
    }
    let after = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(after.state, "Broadcast");
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        crate::ledger::RequestState::DestinationSubmitted
    );
    assert_eq!(rpc.broadcasts().len(), 1);
    // Deterministic signing with the same plan and the same keys
    // reproduces the exact same bytes it started with — recovery never
    // invents a different transaction from the one already on record.
    assert_eq!(rpc.broadcasts()[0], original_signed_hex);
}

#[tokio::test]
async fn resigning_always_produces_canonical_low_s_signatures() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (request_id, _) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;

    // Exercise the recovery-specific plan source and signing path
    // directly, the same way `recover_stuck_goldcoin_payout` does
    // internally, and inspect the raw partial signatures it produces.
    let source = RecoveryPayoutSource { ledger: &ledger };
    let (_, _, partials) = independently_sign_all_inputs(
        &vault_signers,
        &vault,
        &source,
        request_id,
        TEST_THRESHOLD,
        &test_policy(),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();

    let mut checked = 0;
    for input_partials in &partials {
        for partial in input_partials {
            let sig = libsecp256k1::Signature::parse_der(&partial.der_signature).unwrap();
            let mut renormalized = sig;
            renormalized.normalize_s();
            assert_eq!(
                renormalized.serialize_der().as_ref(),
                partial.der_signature.as_slice(),
                "recovery-path signature must already be canonical (low-S)"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "expected at least one signature to check");
}

#[tokio::test]
async fn failed_broadcast_leaves_the_payout_in_signed_state() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (request_id, _) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;

    let rpc = TestGoldcoinRpc::new(BroadcastBehavior::RejectNonCanonical);
    let result = run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 100).await;
    assert!(
        result.is_err(),
        "expected the broadcast rejection to propagate as an error"
    );

    let after = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        after.state, "Signed",
        "must never be marked Broadcast before RPC acceptance"
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        crate::ledger::RequestState::SettlementAuthorized,
        "bridge_requests state must be untouched by a failed recovery attempt"
    );
}

#[tokio::test]
async fn missing_inputs_on_broadcast_leaves_the_payout_in_signed_state() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (request_id, _) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;

    let rpc = TestGoldcoinRpc::new(BroadcastBehavior::MissingInputs);
    let result = run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 100).await;
    assert!(matches!(
        result,
        Err(RecoveryError::BroadcastConflict(id)) if id == request_id
    ));
    let after = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(after.state, "Signed");
}

#[tokio::test]
async fn already_broadcast_is_a_safe_no_op() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (request_id, _) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;

    // Drive it to Broadcast first (a prior, successful recovery).
    let rpc = TestGoldcoinRpc::new(BroadcastBehavior::Accept);
    run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 100)
        .await
        .unwrap();
    let before = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();

    // Calling recovery again must be a pure no-op: no second broadcast, no
    // mutation.
    let outcome = run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 200)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        RecoveryOutcome::AlreadyDone {
            state: "Broadcast".to_string()
        }
    );
    assert_eq!(
        rpc.broadcasts().len(),
        1,
        "must not broadcast a second time"
    );
    let after = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        before, after,
        "already-broadcast payout record must be untouched"
    );
}

#[tokio::test]
async fn tampered_payout_amount_is_refused_not_silently_signed() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";

    // File-backed (not in-memory) so a second raw connection can tamper
    // the row directly, mirroring `ops::audit::tests`'s own
    // tamper-detection pattern — the exact same "corrupted/mutated after
    // the fact" scenario recovery must independently notice.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let mut ledger = Ledger::open(&db_path).unwrap();
    let (request_id, _) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;
    drop(ledger);

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.execute(
        "UPDATE goldcoin_payouts SET payout_atomic = payout_atomic + 1 WHERE request_id = ?1",
        [request_id],
    )
    .unwrap();
    drop(raw);

    let mut ledger = Ledger::open(&db_path).unwrap();
    let rpc = TestGoldcoinRpc::new(BroadcastBehavior::Accept);
    let result = run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 100).await;
    assert!(result.is_err());
    assert!(
        rpc.broadcasts().is_empty(),
        "must never reach broadcast on a tampered record"
    );
    let after = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(after.state, "Signed");
}

#[tokio::test]
async fn recovery_never_reserves_a_new_utxo_or_lets_the_normal_build_path_touch_the_request() {
    let (vault, vault_signers) = vault_and_signers();
    let dest = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
    let mut ledger = Ledger::open_in_memory().unwrap();
    let (request_id, _) =
        ledger_with_stuck_signed_payout(&mut ledger, &vault, &vault_signers, 500_000, dest).await;

    let inputs_before = ledger.get_goldcoin_payout_inputs(request_id).unwrap();
    let available_before = ledger.available_vault_utxos().unwrap();

    let rpc = TestGoldcoinRpc::new(BroadcastBehavior::Accept);
    run_recovery(&mut ledger, &vault, &vault_signers, &rpc, request_id, 100)
        .await
        .unwrap();

    let inputs_after = ledger.get_goldcoin_payout_inputs(request_id).unwrap();
    assert_eq!(
        inputs_before, inputs_after,
        "recovery must operate on the exact same reserved inputs"
    );
    let available_after = ledger.available_vault_utxos().unwrap();
    assert_eq!(
        available_before, available_after,
        "recovery must never select or reserve a new UTXO"
    );

    // The normal build path independently refuses to touch this request
    // at all now — it requires SourceFinalized, which this request has
    // already moved past — so recovery's dedicated path is structurally
    // the only way to act on it, and a second `goldcoin_payouts` row for
    // the same request is impossible regardless (PK on request_id).
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let rederive_err = source
        .rederive_plan(request_id, &vault, &test_policy(), Network::Testnet)
        .unwrap_err();
    assert!(matches!(
        rederive_err,
        SigningError::NotSourceFinalized(id, _) if id == request_id
    ));
}
