//! Tests for the GlcToSol ManualReview refund path.
//!
//! The theme throughout: the refund principal and destination must come
//! from independently verified chain evidence, and every ambiguity must be
//! a refusal. Several tests deliberately make the DATABASE say one thing
//! and the CHAIN say another, and assert the refund refuses rather than
//! silently preferring one.

use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::goldcoin::rpc::{
    BroadcastOutcome, DecodedScriptPubKey, DecodedTransaction, DecodedVin, DecodedVout,
};
use crate::ledger::{
    CreateRequestOutcome, Direction, RequestAmounts, RequestState, ReserveDirection,
};

// ---------------------------------------------------------------- fixtures --

const DEPOSIT_TXID: [u8; 32] = [0xAA; 32];
const DEPOSIT_VOUT: u32 = 1;
const PREV_TXID: [u8; 32] = [0xEA; 32];
const PREV_VOUT: u32 = 0;
/// The real incident's shape: expected 29100 GLC, observed 29050 GLC.
const EXPECTED_GROSS: u64 = 2_910_000_000_000;
const OBSERVED: u64 = 2_905_000_000_000;
/// The sender's P2PKH hash160 — what the refund must be derived to.
const SENDER_HASH: [u8; 20] = [0x5A; 20];

fn fake_pubkey(seed: u8) -> [u8; 33] {
    let mut k = [0u8; 33];
    k[0] = 0x02;
    k[1] = seed;
    k
}

fn test_vault() -> MultisigVault {
    MultisigVault::new(
        vec![fake_pubkey(1), fake_pubkey(2), fake_pubkey(3)],
        2,
        Network::Testnet,
    )
    .unwrap()
}

fn policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 10_000,
        dust_threshold: 10_000,
        max_inputs: 20,
        change_fanout_target_atomic: 1_250_000_000_000,
        zero_conf_change_max_depth: 0,
        zero_conf_change_mode: crate::goldcoin::payout::ZeroConfChangeMode::DepthLimited,
        zero_conf_change_recursive_chain_limit: 20,
        change_fanout_max_outputs: 4,
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

fn dvin(txid: [u8; 32], n: u32) -> DecodedVin {
    DecodedVin {
        txid: Some(crate::goldcoin::hex::encode(&txid)),
        vout: Some(n),
        coinbase: None,
    }
}

struct MockGoldcoin {
    txs: HashMap<String, DecodedTransaction>,
    fail_reads: bool,
    broadcast: Mutex<Vec<String>>,
    broadcast_outcome: Option<BroadcastOutcome>,
    broadcast_error: bool,
}

impl MockGoldcoin {
    /// The healthy world: a single-input deposit paying the vault, with a
    /// P2PKH sender that can be traced.
    fn healthy(vault: &MultisigVault, confirmations: i64) -> Self {
        let mut txs = HashMap::new();
        txs.insert(
            crate::goldcoin::hex::encode(&DEPOSIT_TXID),
            DecodedTransaction {
                txid: crate::goldcoin::hex::encode(&DEPOSIT_TXID),
                vin: vec![dvin(PREV_TXID, PREV_VOUT)],
                vout: vec![
                    dvout(0, 1, "6a0100"),
                    dvout(DEPOSIT_VOUT, OBSERVED, &vault.script_pubkey_hex()),
                ],
                confirmations: Some(confirmations),
            },
        );
        txs.insert(
            crate::goldcoin::hex::encode(&PREV_TXID),
            DecodedTransaction {
                txid: crate::goldcoin::hex::encode(&PREV_TXID),
                vin: vec![dvin([0x11; 32], 0)],
                vout: vec![dvout(
                    PREV_VOUT,
                    EXPECTED_GROSS,
                    &crate::goldcoin::address::p2pkh_script_hex(&SENDER_HASH),
                )],
                confirmations: Some(confirmations + 10),
            },
        );
        Self {
            txs,
            fail_reads: false,
            broadcast: Mutex::new(Vec::new()),
            broadcast_outcome: None,
            broadcast_error: false,
        }
    }

    fn deposit_mut(&mut self) -> &mut DecodedTransaction {
        self.txs
            .get_mut(&crate::goldcoin::hex::encode(&DEPOSIT_TXID))
            .unwrap()
    }
    fn prev_mut(&mut self) -> &mut DecodedTransaction {
        self.txs
            .get_mut(&crate::goldcoin::hex::encode(&PREV_TXID))
            .unwrap()
    }
    fn broadcast_count(&self) -> usize {
        self.broadcast.lock().unwrap().len()
    }
}

impl RefundRpc for MockGoldcoin {
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        if self.fail_reads {
            return Err(RpcError::Transport("connection refused".into()));
        }
        self.txs
            .get(txid_hex)
            .cloned()
            .ok_or_else(|| RpcError::Method {
                code: -5,
                message: "No such mempool or blockchain transaction".into(),
            })
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        if self.broadcast_error {
            return Err(RpcError::Transport("broadcast failed".into()));
        }
        self.broadcast.lock().unwrap().push(hex.to_string());
        Ok(self
            .broadcast_outcome
            .clone()
            .unwrap_or(BroadcastOutcome::Accepted {
                txid: "00".repeat(32),
            }))
    }
}

/// Solana witness: `exists` controls whether a DepositClaim is reported.
struct MockSolana {
    exists: bool,
    error: bool,
}
impl MockSolana {
    fn no_release() -> Self {
        Self {
            exists: false,
            error: false,
        }
    }
}
impl ReleaseWitnessRpc for MockSolana {
    async fn account_exists(&self, _p: &solana_sdk::pubkey::Pubkey) -> Result<bool, String> {
        if self.error {
            return Err("rpc unreachable".to_string());
        }
        Ok(self.exists)
    }
}

fn amounts(gross: u64) -> RequestAmounts {
    RequestAmounts {
        gross_atomic: gross,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: gross,
        net_destination_atomic: gross,
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

fn insert_utxo(ledger: &mut Ledger, txid: [u8; 32], vout: u32, amount: u64, script_hex: &str) {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO vault_utxos (txid, vout, amount_atomic, script_pubkey_hex,
                                      confirmations, first_seen_at, state)
             VALUES (?1, ?2, ?3, ?4, 50, 1000, 'Available')",
            rusqlite::params![txid.as_slice(), vout, amount as i64, script_hex],
        )
        .unwrap();
}

/// A ledger with a request parked exactly as the real incident parked
/// #2477, the deposit indexed as a vault UTXO, and vault funds available.
fn fixture(vault: &MultisigVault) -> (Ledger, i64) {
    let mut ledger = new_ledger();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(EXPECTED_GROSS),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected a reservation")
    };
    ledger
        .record_glc_deposit_observed(
            request_id,
            DEPOSIT_TXID,
            DEPOSIT_VOUT,
            OBSERVED,
            10,
            [0xBB; 32],
            1_100,
        )
        .unwrap();

    let script = vault.script_pubkey_hex();
    insert_utxo(&mut ledger, DEPOSIT_TXID, DEPOSIT_VOUT, OBSERVED, &script);
    insert_utxo(&mut ledger, [0xCC; 32], 0, 1_000_000_000_000, &script);
    (ledger, request_id)
}

async fn dry_run(
    gc: &MockGoldcoin,
    sol: &MockSolana,
    ledger: &Ledger,
    id: i64,
    vault: &MultisigVault,
) -> RefundDryRunReport {
    dry_run_refund(gc, sol, ledger, id, vault, &policy(), Network::Testnet, 6)
        .await
        .unwrap()
}

fn failed(report: &RefundDryRunReport, name: &str) -> bool {
    report.checks.iter().any(|c| c.name == name && !c.passed)
}

/// Signs by assembling a dummy script_sig into each input — structurally
/// what `multisig::assemble` produces, without needing real keys.
async fn fake_sign(_plan: PayoutPlan, mut tx: Transaction) -> Result<Transaction, String> {
    for input in tx.inputs.iter_mut() {
        input.script_sig = vec![0x00, 0x47, 0x30, 0x44];
    }
    Ok(tx)
}

async fn execute(
    gc: &MockGoldcoin,
    sol: &MockSolana,
    ledger: &mut Ledger,
    id: i64,
    vault: &MultisigVault,
) -> Result<RefundExecuteOutcome, RefundError> {
    execute_refund(
        gc,
        sol,
        ledger,
        id,
        "operator note",
        "operator",
        vault,
        &policy(),
        Network::Testnet,
        6,
        2_000,
        fake_sign,
    )
    .await
}

fn pause(ledger: &mut Ledger) {
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("refund"))
        .unwrap();
}

// ------------------------------------------------------- the core behaviour --

#[tokio::test]
async fn refunds_the_observed_amount_never_the_expected_gross() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;

    assert!(report.would_refund, "checks: {:?}", report.checks);
    // THE central assertion of this whole feature.
    assert_eq!(report.refund_amount_atomic(), Some(OBSERVED));
    assert_ne!(report.refund_amount_atomic(), Some(EXPECTED_GROSS));
    assert_eq!(report.expected_gross_atomic, EXPECTED_GROSS);

    let d = report.derived.as_ref().unwrap();
    assert_eq!(d.observed_amount_atomic, OBSERVED);
    assert_eq!(d.refund_dest_p2pkh_hash, SENDER_HASH);
    assert_eq!(d.source_input_txid, PREV_TXID);
    assert_eq!(d.source_input_vout, PREV_VOUT);
}

#[tokio::test]
async fn the_refund_output_is_the_full_principal_and_the_vault_pays_the_fee() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;

    let plan = report.plan.as_ref().unwrap();
    assert_eq!(plan.payout_atomic, OBSERVED);
    assert!(plan.fee_atomic > 0);
    assert_eq!(
        report.vault_outflow_atomic(),
        Some(OBSERVED + plan.fee_atomic)
    );

    let tx = crate::goldcoin::payout::build_unsigned_tx(plan);
    assert_eq!(tx.outputs[0].value_atomic, OBSERVED);
    assert_eq!(
        tx.outputs[0].script_pubkey,
        crate::goldcoin::hex::decode_vec(&crate::goldcoin::address::p2pkh_script_hex(&SENDER_HASH))
            .unwrap()
    );
}

#[tokio::test]
async fn a_malformed_note_cannot_control_the_refund_amount() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests
             SET manual_review_note = 'deposit_amount_mismatch: expected 1 observed 99999999999999'
             WHERE id = ?1",
            [id],
        )
        .unwrap();

    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(report.would_refund);
    assert_eq!(
        report.refund_amount_atomic(),
        Some(OBSERVED),
        "the note's numbers must never reach the refund amount"
    );
}

// --------------------------------------------------------- fail-closed cases --

#[tokio::test]
async fn multiple_inputs_are_refused_as_ambiguous() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.deposit_mut().vin.push(dvin([0x77; 32], 3));

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(failed(&report, "independent Goldcoin source trace"));
    assert!(report
        .checks
        .iter()
        .any(|c| c.detail.contains("exactly one is required")));
}

#[tokio::test]
async fn an_unsupported_sender_script_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.prev_mut().vout[0].script_pub_key.hex = "a914".to_string() + &"bb".repeat(20) + "87";

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(failed(&report, "independent Goldcoin source trace"));
}

#[tokio::test]
async fn a_coinbase_input_has_no_traceable_sender() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.deposit_mut().vin = vec![DecodedVin {
        txid: None,
        vout: None,
        coinbase: Some("abcd".to_string()),
    }];

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
}

#[tokio::test]
async fn a_sender_paying_the_vault_itself_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    // Not P2PKH at all (the vault is P2SH), so this refuses at the script
    // form; the explicit vault check is the belt behind that brace.
    gc.prev_mut().vout[0].script_pub_key.hex = vault.script_pubkey_hex();

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
}

#[tokio::test]
async fn insufficient_confirmations_are_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 2),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
}

#[tokio::test]
async fn a_mempool_only_deposit_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.deposit_mut().confirmations = None;

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
}

#[tokio::test]
async fn an_unreachable_goldcoin_rpc_fails_closed() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.fail_reads = true;

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(report.derived.is_none());
}

#[tokio::test]
async fn a_deposit_output_that_does_not_pay_the_vault_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.deposit_mut().vout[1].script_pub_key.hex =
        crate::goldcoin::address::p2pkh_script_hex(&[0x99; 20]);

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
}

#[tokio::test]
async fn chain_and_index_disagreement_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.deposit_mut().vout[1].value = (OBSERVED + 100_000_000) as f64 / 100_000_000.0;

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(failed(&report, "chain amount agrees with indexed amount"));
}

#[tokio::test]
async fn an_existing_solana_release_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana {
            exists: true,
            error: false,
        },
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
    assert!(failed(
        &report,
        "no Solana release exists (on-chain DepositClaim)"
    ));
    assert!(report.solana_check_detail.contains("DepositClaim"));
}

#[tokio::test]
async fn an_unreadable_solana_rpc_fails_closed() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana {
            exists: false,
            error: true,
        },
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(
        !report.would_refund,
        "an unread Solana chain proves nothing and must refuse"
    );
}

#[tokio::test]
async fn the_wrong_direction_is_refused() {
    let vault = test_vault();
    let mut ledger = new_ledger();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::SolToGlc,
            amounts(OBSERVED),
            b"GLCdest",
            Some([2u8; 32]),
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        request_id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
    assert!(failed(&report, "direction is GlcToSol"));
}

#[tokio::test]
async fn the_wrong_state_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'SourceFinalized' WHERE id = ?1",
            [id],
        )
        .unwrap();
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
    assert!(failed(&report, "state is ManualReview"));
}

#[tokio::test]
async fn a_non_whitelisted_reason_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'something_else: detail' WHERE id = ?1",
            [id],
        )
        .unwrap();
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
    assert!(failed(&report, "manual_review reason is refundable"));
}

#[tokio::test]
async fn a_started_solana_settlement_is_refused_from_the_database_too() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET destination_txid = ?1 WHERE id = ?2",
            rusqlite::params![[0x44u8; 32].as_slice(), id],
        )
        .unwrap();
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
    assert!(failed(&report, "no destination transaction"));
}

// ------------------------------------------------- lifecycle and idempotency --

#[tokio::test]
async fn execute_refuses_without_the_goldcoin_pause() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    let gc = MockGoldcoin::healthy(&vault, 12);

    let err = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not paused"), "got: {err}");
    // Nothing may have happened.
    assert!(ledger.get_goldcoin_refund(id).unwrap().is_none());
    assert_eq!(gc.broadcast_count(), 0);
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
}

#[tokio::test]
async fn a_full_execute_broadcasts_once_and_walks_the_lifecycle() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    let outcome = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert!(matches!(outcome, RefundExecuteOutcome::Broadcast { .. }));
    assert_eq!(gc.broadcast_count(), 1);

    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Broadcast);
    assert_eq!(row.refund_amount_atomic, OBSERVED);
    assert_eq!(row.observed_amount_atomic, OBSERVED);
    assert_eq!(row.refund_dest_p2pkh_hash, SENDER_HASH);
    assert_eq!(row.source_input_txid, PREV_TXID);
    assert!(row.txid.is_some());
    assert!(!row.reservation_released);
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::RefundBroadcast
    );
}

#[tokio::test]
async fn a_second_execute_never_builds_a_second_transaction() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    let first_txid = ledger.get_goldcoin_refund(id).unwrap().unwrap().txid;

    let again = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert!(matches!(
        again,
        RefundExecuteOutcome::AlreadyBroadcast { .. }
    ));
    assert_eq!(
        gc.broadcast_count(),
        1,
        "an already-broadcast refund must not be sent again"
    );
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().txid,
        first_txid
    );
}

#[tokio::test]
async fn crash_after_signing_rebroadcasts_the_same_bytes_and_never_rebuilds() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    // Drive to Signed, then simulate the crash by never broadcasting.
    let derived = IndependentRefundSource {
        rpc: &gc,
        vault_script_hex: vault.script_pubkey_hex(),
        network: Network::Testnet,
        required_confirmations: 6,
    }
    .derive(DEPOSIT_TXID, DEPOSIT_VOUT)
    .await
    .unwrap();
    let candidates = ledger.available_vault_utxos().unwrap();
    let plan = build_refund_plan(&derived, &candidates, &vault, &policy()).unwrap();
    let unsigned = crate::goldcoin::payout::build_unsigned_tx(&plan);
    ledger
        .begin_goldcoin_refund(
            id,
            derived.observed_amount_atomic,
            derived.source_input_txid,
            derived.source_input_vout,
            derived.refund_dest_p2pkh_hash,
            &derived.refund_dest_address,
            plan.fee_atomic,
            &plan.inputs,
            &crate::goldcoin::hex::encode(&unsigned.serialize()),
            "note",
            "operator",
            1_500,
        )
        .unwrap();
    let signed = fake_sign(plan.clone(), unsigned).await.unwrap();
    let signed_hex = crate::goldcoin::hex::encode(&signed.serialize());
    ledger
        .record_goldcoin_refund_signed(id, &signed_hex, 1_600)
        .unwrap();
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Signed
    );

    // Resume: must re-broadcast the SAME bytes.
    let outcome = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert!(matches!(outcome, RefundExecuteOutcome::Rebroadcast { .. }));
    assert_eq!(gc.broadcast.lock().unwrap()[0], signed_hex);
    assert_eq!(
        ledger
            .get_goldcoin_refund(id)
            .unwrap()
            .unwrap()
            .signed_tx_hex,
        Some(signed_hex)
    );
}

#[tokio::test]
async fn crash_after_broadcast_resumes_without_sending_again() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);
    execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();

    let outcome = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        RefundExecuteOutcome::AlreadyBroadcast { .. }
    ));
    assert_eq!(gc.broadcast_count(), 1);
}

#[tokio::test]
async fn already_in_mempool_and_already_in_chain_are_idempotent_successes() {
    for outcome in [
        BroadcastOutcome::AlreadyInMempool,
        BroadcastOutcome::AlreadyInChain,
    ] {
        let vault = test_vault();
        let (mut ledger, id) = fixture(&vault);
        pause(&mut ledger);
        let mut gc = MockGoldcoin::healthy(&vault, 12);
        gc.broadcast_outcome = Some(outcome.clone());

        let result = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
            .await
            .unwrap();
        assert!(
            matches!(result, RefundExecuteOutcome::Broadcast { .. }),
            "{outcome:?} must be treated as success"
        );
        assert_eq!(
            ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
            GoldcoinRefundState::Broadcast
        );
    }
}

#[tokio::test]
async fn a_broadcast_failure_leaves_the_refund_signed_and_resumable() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let mut gc = MockGoldcoin::healthy(&vault, 12);
    gc.broadcast_error = true;

    let err = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap_err();
    assert!(matches!(err, RefundError::Rpc(_)), "got: {err}");

    // Signed, with bytes recorded — recoverable, and no second build.
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Signed);
    assert!(row.signed_tx_hex.is_some());
    assert!(row.txid.is_none());

    gc.broadcast_error = false;
    let outcome = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert!(matches!(outcome, RefundExecuteOutcome::Rebroadcast { .. }));
}

#[tokio::test]
async fn a_signer_failure_leaves_the_refund_built_and_broadcasts_nothing() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    let err = execute_refund(
        &gc,
        &MockSolana::no_release(),
        &mut ledger,
        id,
        "note",
        "operator",
        &vault,
        &policy(),
        Network::Testnet,
        6,
        2_000,
        |_plan, _tx| async { Err("signer 2 timed out".to_string()) },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("signer 2 timed out"), "got: {err}");

    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Built);
    assert!(row.signed_tx_hex.is_none());
    assert_eq!(gc.broadcast_count(), 0);
}

#[tokio::test]
async fn a_signer_that_alters_the_destination_is_refused_before_broadcast() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    let err = execute_refund(
        &gc,
        &MockSolana::no_release(),
        &mut ledger,
        id,
        "note",
        "operator",
        &vault,
        &policy(),
        Network::Testnet,
        6,
        2_000,
        |_plan, mut tx: Transaction| async move {
            // Redirect the refund to an attacker address.
            tx.outputs[0].script_pubkey = crate::goldcoin::hex::decode_vec(
                &crate::goldcoin::address::p2pkh_script_hex(&[0xFF; 20]),
            )
            .unwrap();
            Ok(tx)
        },
    )
    .await
    .unwrap_err();
    // Caught by the reused payout verifier before this module's own
    // destination check even runs — defense in depth, in that order.
    assert!(matches!(err, RefundError::Refused(_)), "got: {err}");
    assert!(
        err.to_string().contains("destination") || err.to_string().contains("does not match plan"),
        "got: {err}"
    );
    assert_eq!(gc.broadcast_count(), 0, "nothing may be broadcast");
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Built
    );
}

#[tokio::test]
async fn a_signer_that_alters_the_amount_is_refused_before_broadcast() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    let err = execute_refund(
        &gc,
        &MockSolana::no_release(),
        &mut ledger,
        id,
        "note",
        "operator",
        &vault,
        &policy(),
        Network::Testnet,
        6,
        2_000,
        |_plan, mut tx: Transaction| async move {
            tx.outputs[0].value_atomic = EXPECTED_GROSS;
            Ok(tx)
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RefundError::Refused(_)), "got: {err}");
    assert_eq!(gc.broadcast_count(), 0);
}

// ------------------------------------------------------- reserve accounting --

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

#[tokio::test]
async fn the_solana_reservation_is_released_exactly_once_and_only_when_confirmed() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    // The mismatch park left the SolanaReserve reservation held.
    let held = reserved_liquidity(&ledger);
    assert_eq!(held, EXPECTED_GROSS as i64);

    execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert_eq!(
        reserved_liquidity(&ledger),
        held,
        "Built/Signed/Broadcast must NOT free capacity — the deposit is still outstanding"
    );

    ledger
        .record_goldcoin_refund_confirmed(id, 6, 3_000)
        .unwrap();
    assert_eq!(
        reserved_liquidity(&ledger),
        0,
        "the terminal transition releases the stranded reservation"
    );
    let row = ledger.get_goldcoin_refund(id).unwrap().unwrap();
    assert_eq!(row.state, GoldcoinRefundState::Refunded);
    assert!(row.reservation_released);
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::Refunded
    );

    // Re-entry (a retried confirmation tick) must not free it twice.
    ledger
        .record_goldcoin_refund_confirmed(id, 9, 3_100)
        .unwrap();
    ledger
        .record_goldcoin_refund_confirmed(id, 12, 3_200)
        .unwrap();
    assert_eq!(
        reserved_liquidity(&ledger),
        0,
        "capacity must never be released more than once"
    );
    assert_eq!(
        ledger
            .get_goldcoin_refund(id)
            .unwrap()
            .unwrap()
            .confirmations,
        12
    );
}

#[tokio::test]
async fn a_refunded_request_is_permanently_ineligible() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);
    execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    ledger
        .record_goldcoin_refund_confirmed(id, 6, 3_000)
        .unwrap();

    // Terminal: a further execute is a no-op, never a second refund.
    let outcome = execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert_eq!(outcome, RefundExecuteOutcome::AlreadyRefunded);
    assert_eq!(gc.broadcast_count(), 1);

    let state = ledger.get_request(id).unwrap().unwrap().state;
    assert!(state.is_refund_lifecycle());
    let checks = ledger.glc_refund_db_checks(id).unwrap();
    assert!(!checks.all_passed());
}

// ------------------------------------------------- database-level guarantees --

#[tokio::test]
async fn the_schema_forbids_a_second_refund_for_the_same_request() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    execute(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &mut ledger,
        id,
        &vault,
    )
    .await
    .unwrap();

    // Bypass every application check and insert directly: the PRIMARY KEY
    // must still refuse.
    let err = ledger.conn_for_tests().execute(
        "INSERT INTO goldcoin_refunds
            (request_id, source_txid, source_vout, observed_amount_atomic, source_input_txid,
             source_input_vout, refund_dest_p2pkh_hash, refund_dest_address,
             refund_amount_atomic, fee_atomic, state, manual_review_reason, note, created_by,
             built_at)
         VALUES (?1, ?2, 9, 1, ?3, 0, ?4, 'addr', 1, 0, 'Built', 'r', 'n', 'c', 1)",
        rusqlite::params![
            id,
            [0x01u8; 32].as_slice(),
            [0x02u8; 32].as_slice(),
            [0x03u8; 20].as_slice()
        ],
    );
    assert!(
        err.is_err(),
        "the PRIMARY KEY must prevent a second refund row"
    );
}

#[tokio::test]
async fn the_schema_forbids_refunding_one_deposit_through_two_requests() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    execute(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &mut ledger,
        id,
        &vault,
    )
    .await
    .unwrap();

    // A DIFFERENT request id, but the SAME source outpoint.
    let err = ledger.conn_for_tests().execute(
        "INSERT INTO goldcoin_refunds
            (request_id, source_txid, source_vout, observed_amount_atomic, source_input_txid,
             source_input_vout, refund_dest_p2pkh_hash, refund_dest_address,
             refund_amount_atomic, fee_atomic, state, manual_review_reason, note, created_by,
             built_at)
         VALUES (999, ?1, ?2, 1, ?3, 0, ?4, 'addr', 1, 0, 'Built', 'r', 'n', 'c', 1)",
        rusqlite::params![
            DEPOSIT_TXID.as_slice(),
            DEPOSIT_VOUT,
            [0x02u8; 32].as_slice(),
            [0x03u8; 20].as_slice()
        ],
    );
    assert!(
        err.is_err(),
        "UNIQUE(source_txid, source_vout) must prevent refunding one deposit twice"
    );
}

#[tokio::test]
async fn the_schema_forbids_a_refund_amount_that_is_not_the_observed_amount() {
    let ledger = new_ledger();
    let err = ledger.conn_for_tests().execute(
        "INSERT INTO goldcoin_refunds
            (request_id, source_txid, source_vout, observed_amount_atomic, source_input_txid,
             source_input_vout, refund_dest_p2pkh_hash, refund_dest_address,
             refund_amount_atomic, fee_atomic, state, manual_review_reason, note, created_by,
             built_at)
         VALUES (1, ?1, 0, 100, ?2, 0, ?3, 'addr', 99, 0, 'Built', 'r', 'n', 'c', 1)",
        rusqlite::params![
            [0x01u8; 32].as_slice(),
            [0x02u8; 32].as_slice(),
            [0x03u8; 20].as_slice()
        ],
    );
    assert!(
        err.is_err(),
        "the schema must forbid refunding anything other than the observed principal"
    );
}

#[tokio::test]
async fn the_schema_forbids_releasing_capacity_outside_the_terminal_state() {
    let ledger = new_ledger();
    let err = ledger.conn_for_tests().execute(
        "INSERT INTO goldcoin_refunds
            (request_id, source_txid, source_vout, observed_amount_atomic, source_input_txid,
             source_input_vout, refund_dest_p2pkh_hash, refund_dest_address,
             refund_amount_atomic, fee_atomic, state, manual_review_reason, note, created_by,
             built_at, reservation_released)
         VALUES (1, ?1, 0, 100, ?2, 0, ?3, 'addr', 100, 0, 'Built', 'r', 'n', 'c', 1, 1)",
        rusqlite::params![
            [0x01u8; 32].as_slice(),
            [0x02u8; 32].as_slice(),
            [0x03u8; 20].as_slice()
        ],
    );
    assert!(
        err.is_err(),
        "capacity may only be marked released in the Refunded state"
    );
}

#[tokio::test]
async fn refund_inputs_cannot_be_reused_by_another_refund() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    execute(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &mut ledger,
        id,
        &vault,
    )
    .await
    .unwrap();
    let inputs = ledger.get_goldcoin_refund_inputs(id).unwrap();
    assert!(!inputs.is_empty());

    let err = ledger.conn_for_tests().execute(
        "INSERT INTO goldcoin_refund_inputs (request_id, input_order, txid, vout, amount_atomic)
         VALUES (999, 0, ?1, ?2, 1)",
        rusqlite::params![inputs[0].0.as_slice(), inputs[0].1],
    );
    assert!(
        err.is_err(),
        "UNIQUE(txid, vout) must stop one vault UTXO funding two refunds"
    );
}

#[tokio::test]
async fn listing_shows_open_refunds_and_can_hide_terminal_ones() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    execute(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &mut ledger,
        id,
        &vault,
    )
    .await
    .unwrap();

    assert_eq!(ledger.list_goldcoin_refunds(false).unwrap().len(), 1);
    assert_eq!(ledger.list_goldcoin_refunds(true).unwrap().len(), 1);

    ledger
        .record_goldcoin_refund_confirmed(id, 6, 3_000)
        .unwrap();
    assert_eq!(ledger.list_goldcoin_refunds(false).unwrap().len(), 1);
    assert_eq!(
        ledger.list_goldcoin_refunds(true).unwrap().len(),
        0,
        "--open-only must hide a completed refund"
    );
}

/// Renders a full dry-run report so the operator-facing format is checked
/// by the suite rather than only described in the runbook. Run with
/// `--nocapture` to see it.
#[tokio::test]
async fn dry_run_report_renders_the_documented_format() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;

    // Every documented section must be present and populated.
    assert!(report.would_refund);
    assert_eq!(report.refund_amount_atomic(), Some(OBSERVED));
    assert!(report.fee_atomic().unwrap() > 0);
    assert_eq!(
        report.vault_outflow_atomic(),
        Some(OBSERVED + report.fee_atomic().unwrap())
    );
    assert!(report.checks.len() >= 14, "every check must be reported");
    // Every check passes EXCEPT the pause, which correctly reports FAIL in
    // a dry run: the runbook's procedure is dry-run, then pause, then
    // execute, so a not-yet-engaged pause must not read as PASS.
    for c in &report.checks {
        if c.name.starts_with("GoldcoinReserve paused") {
            assert!(
                !c.passed,
                "an unpaused dry run must report the pause as FAIL"
            );
        } else {
            assert!(c.passed, "unexpected failure: {} — {}", c.name, c.detail);
        }
    }

    println!("\n===== SAMPLE DRY-RUN OUTPUT =====");
    println!(
        "GlcToSol ManualReview refund — request {}",
        report.request_id
    );
    println!("\n  REQUEST");
    println!(
        "    expected gross            = {} atomic ({} GLC)",
        report.expected_gross_atomic,
        format_glc(report.expected_gross_atomic)
    );
    let d = report.derived.as_ref().unwrap();
    println!(
        "    source transaction        = {}",
        crate::goldcoin::hex::encode(&d.source_txid)
    );
    println!("    source output             = {}", d.source_vout);
    println!("\n  INDEPENDENTLY VERIFIED CHAIN FACTS");
    println!(
        "    observed deposit          = {} atomic ({} GLC)",
        d.observed_amount_atomic,
        format_glc(d.observed_amount_atomic)
    );
    println!(
        "    confirmations             = {} (required 6)",
        d.confirmations
    );
    println!(
        "    traced source input       = {}:{}",
        crate::goldcoin::hex::encode(&d.source_input_txid),
        d.source_input_vout
    );
    println!("    derived refund address    = {}", d.refund_dest_address);
    println!(
        "    refund amount             = {} atomic ({} GLC)  [the FULL observed deposit; the vault pays the miner fee]",
        d.observed_amount_atomic,
        format_glc(d.observed_amount_atomic)
    );
    println!("\n  SOLANA RELEASE WITNESS");
    println!("    {}", report.solana_check_detail);
    println!("\n  EXISTING REFUND\n    none");
    let plan = report.plan.as_ref().unwrap();
    println!("\n  VAULT / RESERVE EFFECT");
    println!("    inputs selected           = {}", plan.inputs.len());
    println!(
        "    estimated miner fee       = {} atomic ({} GLC)",
        plan.fee_atomic,
        format_glc(plan.fee_atomic)
    );
    println!(
        "    change outputs            = {} totalling {} atomic",
        plan.change_outputs.len(),
        plan.total_change_atomic()
    );
    println!(
        "    total vault outflow       = {} atomic ({} GLC)  [refund + fee]",
        report.vault_outflow_atomic().unwrap(),
        format_glc(report.vault_outflow_atomic().unwrap())
    );
    println!(
        "    GoldcoinReserve paused    = {}",
        report.goldcoin_reserve_paused
    );
    println!("\n  SAFETY CHECKS");
    for c in &report.checks {
        let mark = if c.passed { "PASS" } else { "FAIL" };
        if c.detail.is_empty() {
            println!("    [{mark}] {}", c.name);
        } else {
            println!("    [{mark}] {} — {}", c.name, c.detail);
        }
    }
    println!("\n  VERDICT: every check passes; an --execute would refund (after the GoldcoinReserve pause)");
    println!("===== END SAMPLE =====\n");
}
