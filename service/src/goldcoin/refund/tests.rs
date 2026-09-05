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
/// The fixture's request is the first row in a fresh in-memory ledger, so
/// its rowid is 1. Pinned as a constant because the derived deposit
/// script is a function of it; `fixture` asserts the coupling rather than
/// letting it drift silently.
const FIXTURE_REQUEST_ID: i64 = 1;

/// Real, valid secp256k1 points from fixed secrets. The fixture needs
/// genuine points now, not placeholder bytes: `derive_request_vault`
/// parses and EC-tweaks each root pubkey, so a fake one cannot be
/// derived from — which is precisely the production behaviour under
/// test.
fn root_pubkey(seed: u8) -> [u8; 33] {
    let mut sk = [0u8; 32];
    sk[31] = seed;
    let secret = libsecp256k1::SecretKey::parse(&sk).unwrap();
    libsecp256k1::PublicKey::from_secret_key(&secret).serialize_compressed()
}

/// The ROOT vault — the shared 2-of-3 that holds spendable inventory and
/// pays change. A per-request deposit address is NOT this script.
fn test_vault() -> MultisigVault {
    MultisigVault::new(
        vec![root_pubkey(1), root_pubkey(2), root_pubkey(3)],
        2,
        Network::Testnet,
    )
    .unwrap()
}

/// THE canonical derivation, exactly as production computes it — the same
/// `derivation::derive_request_vault` used by request creation, payout
/// recovery and the refund verifier. Tests never hand-roll a script.
fn request_deposit_script(vault: &MultisigVault, request_id: i64) -> String {
    crate::goldcoin::derivation::derive_request_vault(vault, request_id, Network::Testnet)
        .unwrap()
        .script_pubkey_hex()
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
    /// `deposit_script_hex` is the request-specific derived P2SH the
    /// deposit output pays — deliberately NOT the root vault script, so
    /// the fixture reproduces the real production shape.
    /// The common case: a deposit paying the fixture request's own
    /// derived deposit script.
    fn healthy(vault: &MultisigVault, confirmations: i64) -> Self {
        Self::healthy_with_deposit_script(
            &request_deposit_script(vault, FIXTURE_REQUEST_ID),
            confirmations,
        )
    }

    fn healthy_with_deposit_script(deposit_script_hex: &str, confirmations: i64) -> Self {
        let mut txs = HashMap::new();
        txs.insert(
            crate::goldcoin::hex::encode(&DEPOSIT_TXID),
            DecodedTransaction {
                txid: crate::goldcoin::hex::encode(&DEPOSIT_TXID),
                vin: vec![dvin(PREV_TXID, PREV_VOUT)],
                vout: vec![
                    dvout(0, 1, "6a0100"),
                    dvout(DEPOSIT_VOUT, OBSERVED, deposit_script_hex),
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
    assert_eq!(
        request_id, FIXTURE_REQUEST_ID,
        "the derived deposit script is a function of the request id; keep them coupled"
    );

    // Exactly what production does at request creation: derive the
    // per-request deposit vault and persist address + script + redeem
    // script (api.rs). The stored columns are a consistency witness, not
    // an authority.
    let derived =
        crate::goldcoin::derivation::derive_request_vault(vault, request_id, Network::Testnet)
            .unwrap();
    ledger
        .set_glc_to_sol_deposit_address(
            request_id,
            derived.address(),
            &derived.script_pubkey_hex(),
            &derived.redeem_script_hex(),
        )
        .unwrap();

    // Parks the request exactly as the indexer does on an amount
    // mismatch — which now also persists `observed_amount_atomic`
    // (schema v20) in the same transaction as the source outpoint.
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

    // NOTE: the deposit is deliberately NOT inserted into `vault_utxos`.
    // That table is listunspent-derived root-vault spendable inventory,
    // and nothing imports a per-request derived P2SH into the node, so a
    // real request-specific deposit never appears there. Only ROOT-vault
    // funds are seeded, to pay the refund and its fee.
    //
    // The refund is therefore funded from ROOT vault inventory, and must
    // cover the full principal plus the fee.
    let root_script = vault.script_pubkey_hex();
    for (i, seed) in [0xCCu8, 0xCD, 0xCE, 0xCF].iter().enumerate() {
        let mut txid = [0u8; 32];
        txid.fill(*seed);
        insert_utxo(&mut ledger, txid, i as u32, 1_000_000_000_000, &root_script);
    }
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
    assert!(failed(
        &report,
        "chain amount agrees with the durable ledger witness"
    ));
    // And the mode is never reported as if the check had succeeded.
    assert_eq!(report.amount_witness_mode, None);
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
        expected_deposit_script_hex: request_deposit_script(&vault, FIXTURE_REQUEST_ID),
        root_vault_script_hex: vault.script_pubkey_hex(),
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

// ------------------------------------------------- no automatic processing --

/// No tick may ever CREATE, SIGN or SEND a Goldcoin refund. That remains
/// an explicit operator request, and this asserts it structurally: none of
/// the constructing or fund-moving entry points appears in the
/// orchestrator's tick surface at all, so there is no code path that could
/// pick up a `Built` or `Signed` row and carry it toward a broadcast.
///
/// Narrowed on 2026-09-04, when confirmation reconciliation was added. The
/// orchestrator MAY now advance an ALREADY-BROADCAST refund to `Refunded`
/// once its recorded transaction is verified at the required depth — a
/// read-only observation of a transaction that already exists, which moves
/// nothing. The forbidden list keeps exactly the entry points that could
/// put new bytes on the network, and
/// `refund_reconciliation_cannot_reach_a_broadcast` below pins the
/// read-only half from the other side.
#[test]
fn no_orchestrator_tick_builds_signs_or_broadcasts_a_goldcoin_refund() {
    let orchestrator_src = include_str!("../../orchestrator.rs");
    for forbidden in [
        "execute_refund",
        "begin_goldcoin_refund",
        "record_goldcoin_refund_signed",
        "record_goldcoin_refund_broadcast",
    ] {
        assert!(
            !orchestrator_src.contains(forbidden),
            "orchestrator.rs references {forbidden:?} — a Goldcoin refund may only ever be \
             built, signed or sent through an explicit operator request, never a tick"
        );
    }
}

/// The other half of the same guarantee: the reconciliation module that
/// the tick DOES call cannot construct or send anything either. Its RPC
/// bound carries one read method, so a broadcast does not compile — this
/// test additionally pins that no builder, signer or evidence-writing
/// ledger call has crept into it by another route.
#[test]
fn refund_reconciliation_cannot_reach_a_broadcast() {
    let src = include_str!("../refund_reconcile.rs");
    for forbidden in [
        "send_raw_transaction",
        "execute_refund",
        "build_refund_plan",
        "begin_goldcoin_refund",
        "record_goldcoin_refund_signed",
        "record_goldcoin_refund_broadcast",
        "VaultSigner",
        "independently_sign",
        "available_vault_utxos",
    ] {
        // The module docs name `send_raw_transaction` when explaining what
        // the narrow trait deliberately omits; only code may not use it.
        let code_only: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code_only.contains(forbidden),
            "refund_reconcile.rs references {forbidden:?} — confirmation reconciliation must \
             never build, select inputs for, sign, or send anything"
        );
    }
}

/// A refund left in `Built` stays there across any amount of unrelated
/// activity: nothing sweeps it up.
#[tokio::test]
async fn a_built_refund_is_never_advanced_without_an_explicit_request() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);

    // Reach Built by failing the signer.
    let _ = execute_refund(
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
        |_p, _t| async { Err("signer down".to_string()) },
    )
    .await;
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Built
    );

    // Unrelated ledger activity must not touch it.
    ledger.expire_reservations(9_999).unwrap();
    assert_eq!(
        ledger.get_goldcoin_refund(id).unwrap().unwrap().state,
        GoldcoinRefundState::Built,
        "only an explicit execute may advance a refund"
    );
    assert_eq!(gc.broadcast_count(), 0);
}

// ------------------------------------------- real mainnet address/script forms --

/// The same trace against MAINNET-form P2PKH scripts and addresses,
/// proving the derivation is not testnet-specific and produces a real
/// mainnet-prefix address.
///
/// Deliberately NOT a request-id special case and not the incident's own
/// values: it exercises the address FORM, while every amount and address
/// still comes from the mocked chain exactly as production takes them
/// from RPC.
#[tokio::test]
async fn the_trace_derives_a_mainnet_form_address_from_a_mainnet_script() {
    // A realistic-looking hash160 rather than a repeated byte.
    let sender_hash: [u8; 20] = [
        0x62, 0xe9, 0x07, 0xb1, 0x5c, 0xbf, 0x27, 0xd5, 0x42, 0x53, 0x99, 0xeb, 0xf6, 0xf0, 0xfb,
        0x50, 0xeb, 0xb8, 0x8f, 0x18,
    ];
    let script = crate::goldcoin::address::p2pkh_script_hex(&sender_hash);
    // The canonical 25-byte P2PKH template, exactly as a node reports it.
    assert!(script.starts_with("76a914") && script.ends_with("88ac"));
    assert_eq!(script.len(), 50);

    let recovered = crate::goldcoin::address::p2pkh_hash_from_script_hex(&script).unwrap();
    assert_eq!(recovered, sender_hash);

    let mainnet = crate::goldcoin::address::encode_p2pkh(&recovered, Network::Mainnet);
    let testnet = crate::goldcoin::address::encode_p2pkh(&recovered, Network::Testnet);
    assert_ne!(
        mainnet, testnet,
        "the same hash must render differently per network — a testnet-form address in a \
         report is a fixture artifact, never a mainnet destination"
    );
    // Round-trips back to the same hash.
    assert_eq!(
        crate::goldcoin::address::decode_p2pkh(&mainnet, Network::Mainnet).unwrap(),
        sender_hash
    );
}

/// Non-P2PKH script forms that a real sender might use are all refused,
/// individually.
#[test]
fn every_unsupported_script_form_is_refused_individually() {
    let cases = [
        ("P2SH", "a914".to_string() + &"bb".repeat(20) + "87"),
        ("OP_RETURN", "6a0548656c6c6f".to_string()),
        ("P2PK", "21".to_string() + &"02".repeat(33) + "ac"),
        ("P2WPKH (segwit v0)", "0014".to_string() + &"cc".repeat(20)),
        ("empty", String::new()),
        (
            "P2PKH with a wrong trailer",
            "76a914".to_string() + &"dd".repeat(20) + "88ad",
        ),
        (
            "P2PKH with a wrong length prefix",
            "76a913".to_string() + &"ee".repeat(20) + "88ac",
        ),
    ];
    for (label, script) in cases {
        assert!(
            crate::goldcoin::address::p2pkh_hash_from_script_hex(&script).is_err(),
            "{label} must not yield a refund destination"
        );
    }
}

// =========================================================================
// Permanent regression coverage for the request-specific deposit binding
// and the durable amount witness (schema v20).
//
// The real incident's SHAPE is reproduced — a per-request P2SH deposit
// output distinct from the root vault script — with ZERO request-id
// special-casing anywhere in production code or here.
// =========================================================================

/// The production defect, pinned: a per-request deposit does NOT pay the
/// root vault script, and requiring it made every such deposit
/// unrefundable.
#[tokio::test]
async fn a_request_deposit_script_is_not_the_root_vault_script() {
    let vault = test_vault();
    let derived = request_deposit_script(&vault, FIXTURE_REQUEST_ID);
    let root = vault.script_pubkey_hex();

    assert_ne!(
        derived, root,
        "a per-request deposit address must differ from the root vault script; conflating them \
         is the defect this suite exists to prevent"
    );
    // Both are P2SH (OP_HASH160 <20> OP_EQUAL), 23 bytes — the real
    // production shape, and why a naive comparison looked plausible.
    for script in [&derived, &root] {
        assert_eq!(script.len(), 46, "23-byte P2SH");
        assert!(script.starts_with("a914") && script.ends_with("87"));
    }
}

/// A legitimate request-specific deposit refunds with no manual database
/// work of any kind, and absence from `vault_utxos` is NOT a failure.
#[tokio::test]
async fn a_request_specific_deposit_refunds_without_any_manual_db_work() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);

    // Proof the deposit really is absent from vault_utxos.
    let indexed: Option<i64> = ledger
        .conn_for_tests()
        .query_row(
            "SELECT amount_atomic FROM vault_utxos WHERE txid = ?1 AND vout = ?2",
            rusqlite::params![DEPOSIT_TXID.as_slice(), DEPOSIT_VOUT],
            |r| r.get(0),
        )
        .ok();
    assert_eq!(
        indexed, None,
        "a per-request deposit is never in vault_utxos; the refund must not require it"
    );

    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(
        report.would_refund,
        "checks: {:?}",
        report
            .checks
            .iter()
            .filter(|c| !c.passed)
            .map(|c| format!("{}: {}", c.name, c.detail))
            .collect::<Vec<_>>()
    );
    assert_eq!(report.refund_amount_atomic(), Some(OBSERVED));
}

/// The durable witness is written at park time, in the same transaction
/// as the outpoint.
#[tokio::test]
async fn a_parked_mismatch_stores_the_durable_amount_witness() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);

    let (durable, txid, vout): (Option<i64>, Option<Vec<u8>>, Option<u32>) = ledger
        .conn_for_tests()
        .query_row(
            "SELECT observed_amount_atomic, source_txid, source_vout
             FROM bridge_requests WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(durable, Some(OBSERVED as i64));
    assert_eq!(txid.as_deref(), Some(DEPOSIT_TXID.as_slice()));
    assert_eq!(vout, Some(DEPOSIT_VOUT));

    let checks = ledger.glc_refund_db_checks(id).unwrap();
    assert_eq!(checks.durable_observed_amount_atomic, Some(OBSERVED));
}

/// The witness and the outpoint are written atomically: a rolled-back
/// observation leaves NEITHER, never a half-written witness.
#[tokio::test]
async fn a_rolled_back_observation_leaves_no_partial_witness() {
    let vault = test_vault();
    let mut ledger = new_ledger();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(EXPECTED_GROSS),
            &[9u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!("expected a reservation")
    };
    let derived =
        crate::goldcoin::derivation::derive_request_vault(&vault, request_id, Network::Testnet)
            .unwrap();
    ledger
        .set_glc_to_sol_deposit_address(
            request_id,
            derived.address(),
            &derived.script_pubkey_hex(),
            &derived.redeem_script_hex(),
        )
        .unwrap();

    // An observation against a request in the WRONG state rolls back.
    ledger
        .cancel_request(request_id, 1_050, "test rollback")
        .unwrap();
    let _ = ledger.record_glc_deposit_observed(
        request_id,
        DEPOSIT_TXID,
        DEPOSIT_VOUT,
        OBSERVED,
        10,
        [0xBB; 32],
        1_100,
    );

    let (durable, txid): (Option<i64>, Option<Vec<u8>>) = ledger
        .conn_for_tests()
        .query_row(
            "SELECT observed_amount_atomic, source_txid FROM bridge_requests WHERE id = ?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (durable, txid),
        (None, None),
        "the witness and the outpoint must be written together or not at all"
    );
}

/// RPC amount equal to the durable witness passes; any difference fails.
#[tokio::test]
async fn rpc_amount_must_equal_the_durable_witness_exactly() {
    let vault = test_vault();

    // Equal -> PASS, in durable mode.
    let (ledger, id) = fixture(&vault);
    let ok = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(ok.would_refund);
    assert_eq!(
        ok.amount_witness_mode,
        Some(AmountWitnessMode::DurableChainVsLedger)
    );

    // Different in EITHER direction -> FAIL. No tolerance.
    for delta in [1i64, -1, 100_000_000, -100_000_000] {
        let (ledger, id) = fixture(&vault);
        let mut gc = MockGoldcoin::healthy(&vault, 12);
        gc.deposit_mut().vout[1].value = (OBSERVED as i64 + delta) as f64 / 100_000_000.0;
        let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
        assert!(
            !report.would_refund,
            "a {delta} atomic difference must refuse"
        );
        assert!(failed(
            &report,
            "chain amount agrees with the durable ledger witness"
        ));
    }
}

/// One request's derived deposit script can never authorize another's
/// refund — the script IS the request binding.
#[tokio::test]
async fn request_as_deposit_script_cannot_authorize_request_b() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);

    // A deposit paying a DIFFERENT request's derived script.
    let other_script = request_deposit_script(&vault, FIXTURE_REQUEST_ID + 1);
    assert_ne!(other_script, request_deposit_script(&vault, id));
    let gc = MockGoldcoin::healthy_with_deposit_script(&other_script, 12);

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(failed(&report, "independent Goldcoin source trace"));
}

/// An arbitrary P2SH output — the right SHAPE, the wrong script — fails.
#[tokio::test]
async fn an_arbitrary_p2sh_deposit_output_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);

    let arbitrary = "a914".to_string() + &"7f".repeat(20) + "87";
    assert_eq!(arbitrary.len(), 46, "same shape as a real deposit script");
    let gc = MockGoldcoin::healthy_with_deposit_script(&arbitrary, 12);

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(failed(&report, "independent Goldcoin source trace"));
}

/// The ROOT vault script is not accepted for a request-specific deposit
/// either: it is a real bridge script, but not THIS request's.
#[tokio::test]
async fn the_root_vault_script_does_not_authorize_a_request_specific_refund() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    let gc = MockGoldcoin::healthy_with_deposit_script(&vault.script_pubkey_hex(), 12);

    let report = dry_run(&gc, &MockSolana::no_release(), &ledger, id, &vault).await;
    assert!(!report.would_refund);
    assert!(failed(&report, "independent Goldcoin source trace"));
}

/// Tampering with the stored deposit script cannot redirect or authorize
/// a refund: the derivation is the authority, the column only a witness.
#[tokio::test]
async fn a_tampered_stored_deposit_script_is_refused() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);

    let attacker = "a914".to_string() + &"11".repeat(20) + "87";
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET deposit_script_pubkey_hex = ?1 WHERE id = ?2",
            rusqlite::params![attacker, id],
        )
        .unwrap();

    // The chain still pays the correctly derived script, so the trace
    // passes; the STORED column now disagrees with the derivation.
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund);
    assert!(failed(
        &report,
        "stored deposit script agrees with the derivation"
    ));
}

/// Tampering with `deposit_address` cannot authorize anything: it is
/// never consulted as an authority at all.
#[tokio::test]
async fn a_tampered_deposit_address_cannot_authorize_a_refund() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET deposit_address = 'MATTACKERaddressNotUsedAnywhere'
             WHERE id = ?1",
            [id],
        )
        .unwrap();

    // The refund still succeeds, and still pays the SENDER — proving the
    // address column had no influence on the outcome in either direction.
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
        report.derived.as_ref().unwrap().refund_dest_p2pkh_hash,
        SENDER_HASH,
        "the destination comes from the traced prevout, never from deposit_address"
    );
}

/// A wrong source vout, and a wrong source txid, each fail.
#[tokio::test]
async fn a_wrong_source_outpoint_is_refused() {
    let vault = test_vault();

    // Wrong vout: the ledger's durable binding points at an output that
    // is not the deposit (here, the OP_RETURN at index 0).
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET source_vout = 0 WHERE id = ?1",
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
    assert!(!report.would_refund, "a wrong vout must refuse");

    // Wrong txid: the node does not know it.
    let (ledger, id) = fixture(&vault);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET source_txid = ?1 WHERE id = ?2",
            rusqlite::params![[0x99u8; 32].as_slice(), id],
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
    assert!(!report.would_refund, "a wrong txid must refuse");
}

// ------------------------------------------------------------ legacy mode --

/// Simulates a row parked by the OLD software: the durable witness column
/// is NULL. Done by clearing the column, never by inserting a fake value.
fn make_legacy(ledger: &Ledger, id: i64) {
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET observed_amount_atomic = NULL WHERE id = ?1",
            [id],
        )
        .unwrap();
}

/// A legacy row still refunds — every other binding applies in full — but
/// the reduced assurance is reported explicitly and distinctly.
#[tokio::test]
async fn a_legacy_row_uses_the_explicit_legacy_mode() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    make_legacy(&ledger, id);

    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(report.would_refund, "a legacy row is still refundable");
    assert_eq!(
        report.amount_witness_mode,
        Some(AmountWitnessMode::LegacyRpcOnly)
    );
    // The two modes must never read as equivalent.
    assert!(AmountWitnessMode::LegacyRpcOnly.is_legacy());
    assert!(!AmountWitnessMode::DurableChainVsLedger.is_legacy());
    assert_ne!(
        AmountWitnessMode::LegacyRpcOnly.describe(),
        AmountWitnessMode::DurableChainVsLedger.describe()
    );
    assert!(
        AmountWitnessMode::LegacyRpcOnly
            .describe()
            .contains("legacy"),
        "the legacy banner must say so plainly"
    );
    // The principal is the RPC-verified amount.
    assert_eq!(report.refund_amount_atomic(), Some(OBSERVED));
}

/// Legacy mode still cannot take the amount from the note — the note is
/// never evidence, in either mode.
#[tokio::test]
async fn legacy_mode_still_never_parses_the_reason_note() {
    let vault = test_vault();
    let (ledger, id) = fixture(&vault);
    make_legacy(&ledger, id);
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
        "the note's number must have no effect whatsoever, legacy mode included"
    );
}

/// A legacy row still fails every non-amount binding.
#[tokio::test]
async fn legacy_mode_does_not_relax_any_other_binding() {
    let vault = test_vault();

    // Wrong deposit script.
    let (ledger, id) = fixture(&vault);
    make_legacy(&ledger, id);
    let arbitrary = "a914".to_string() + &"7f".repeat(20) + "87";
    let report = dry_run(
        &MockGoldcoin::healthy_with_deposit_script(&arbitrary, 12),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(
        !report.would_refund,
        "legacy must not relax the script bind"
    );

    // Insufficient confirmations.
    let (ledger, id) = fixture(&vault);
    make_legacy(&ledger, id);
    let report = dry_run(
        &MockGoldcoin::healthy(&vault, 1),
        &MockSolana::no_release(),
        &ledger,
        id,
        &vault,
    )
    .await;
    assert!(!report.would_refund, "legacy must not relax confirmations");

    // An existing Solana release.
    let (ledger, id) = fixture(&vault);
    make_legacy(&ledger, id);
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
    assert!(
        !report.would_refund,
        "legacy must not relax the release check"
    );
}

/// Duplicate refunds remain structurally impossible under the new model.
#[tokio::test]
async fn duplicate_refunds_remain_impossible_after_the_binding_change() {
    let vault = test_vault();
    let (mut ledger, id) = fixture(&vault);
    pause(&mut ledger);
    let gc = MockGoldcoin::healthy(&vault, 12);
    execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();

    // Same request again: no second transaction.
    execute(&gc, &MockSolana::no_release(), &mut ledger, id, &vault)
        .await
        .unwrap();
    assert_eq!(gc.broadcast_count(), 1);

    // And the schema still refuses a second row for the same outpoint.
    let err = ledger.conn_for_tests().execute(
        "INSERT INTO goldcoin_refunds
            (request_id, source_txid, source_vout, observed_amount_atomic, source_input_txid,
             source_input_vout, refund_dest_p2pkh_hash, refund_dest_address,
             refund_amount_atomic, fee_atomic, state, manual_review_reason, note, created_by,
             built_at)
         VALUES (4242, ?1, ?2, 1, ?3, 0, ?4, 'addr', 1, 0, 'Built', 'r', 'n', 'c', 1)",
        rusqlite::params![
            DEPOSIT_TXID.as_slice(),
            DEPOSIT_VOUT,
            [0x02u8; 32].as_slice(),
            [0x03u8; 20].as_slice()
        ],
    );
    assert!(err.is_err(), "one deposit can never be refunded twice");
}
