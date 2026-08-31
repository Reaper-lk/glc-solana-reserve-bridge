//! Regression suite for the 0-conf-spendability policy for BRIDGE-CREATED
//! payout change (docs/09-runbook.md "Zero-conf payout change"):
//!
//! - only outputs with an AUTHORITATIVE provenance row in
//!   `goldcoin_payout_change_outpoints` (written by
//!   `Ledger::record_goldcoin_payout_broadcast`, in the same transaction
//!   as the broadcast fact) may be spent below `vault_min_confirmations`;
//! - external deposits, arbitrary vault-paying outputs, and vault-split
//!   outputs always wait the full threshold, at every confirmation count
//!   below it;
//! - confirmed UTXOs are always preferred; 0-conf change is additional
//!   liquidity only (`signing::goldcoin_vault`'s two-phase selection);
//! - unconfirmed chaining is capped (`zero_conf_change_max_depth`), the
//!   reservation guard re-checks eligibility inside its own write
//!   transaction, provenance survives restart, and a missing/conflicted
//!   parent (hold or disappearance) removes dependent change from
//!   selection immediately without ever enabling a second payout for the
//!   same request.
//!
//! Everything runs against the REAL ledger + REAL independent-signer
//! plan derivation, mirroring `goldcoin_payout_lifecycle.rs`'s harness.

use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::indexer::GoldcoinRpc;
use glc_reserve_bridge_service::goldcoin::multisig;
use glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy;
use glc_reserve_bridge_service::goldcoin::payout_recovery::{
    recover_stuck_goldcoin_payout, RecoveryError,
};
use glc_reserve_bridge_service::goldcoin::rpc::{
    BlockHeader, BroadcastOutcome, DecodedTransaction, ListUnspentEntry, RpcError,
};
use glc_reserve_bridge_service::goldcoin::split;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, ReserveDirection, SolFoldOutcome};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    independently_sign, DevLedgerPayoutSource, DevVaultSigner, SigningError,
};

/// Distinct per-request destination (and the folds below use distinct
/// source wallets too) so the SolToGlc recipient/source-wallet rate
/// limits — deliberately untouched by this policy — never park a fixture
/// request.
fn dest_addr(index: u64) -> String {
    glc_reserve_bridge_service::goldcoin::address::encode_p2pkh(
        &[0x50 + index as u8; 20],
        Network::Testnet,
    )
}
const TEST_SOLANA_DECIMALS: u8 = 6;
const TEST_SIGNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// The production external-deposit threshold this whole suite runs at —
/// deliberately the real value, never 1, so "below the threshold" cases
/// are genuine.
const VAULT_MIN_CONFIRMATIONS: i64 = 6;

fn test_policy(zero_conf_change_max_depth: u32) -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * 100_000_000,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth,
    }
}

fn setup_vault() -> (MultisigVault, [DevVaultSigner; 3]) {
    let signers = [
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap();
    (vault, signers)
}

fn sol_to_glc_amounts(amount: u64) -> glc_reserve_bridge_service::ledger::RequestAmounts {
    let gross_canonical = glc_reserve_bridge_service::amount_conversion::SolanaAtomic(amount)
        .to_canonical(TEST_SOLANA_DECIMALS)
        .unwrap();
    let fb = glc_reserve_bridge_service::amount_conversion::compute_fee(gross_canonical).unwrap();
    glc_reserve_bridge_service::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

fn net_payout(amount_solana: u64) -> u64 {
    sol_to_glc_amounts(amount_solana).net_destination_atomic
}

fn configure_reserves(ledger: &mut Ledger) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            500_000_000,
            0,
            250_000_000,
            100_000_000,
            50_000_000,
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

/// The wallet's live view, re-synced in full every time (matching
/// `sync_vault_utxos`'s contract that absent tracked outputs are spent).
#[derive(Default)]
struct ChainView {
    utxos: Vec<(VaultUtxo, i64, String)>,
}

impl ChainView {
    fn add(&mut self, utxo: VaultUtxo, confirmations: i64) {
        let script = utxo.script_pubkey_hex.clone();
        self.utxos.push((utxo, confirmations, script));
    }
    fn remove(&mut self, txid: [u8; 32], vout: u32) {
        self.utxos
            .retain(|(u, _, _)| !(u.txid == txid && u.vout == vout));
    }
    fn set_confirmations(&mut self, txid: [u8; 32], confirmations: i64) {
        for (u, c, _) in &mut self.utxos {
            if u.txid == txid {
                *c = confirmations;
            }
        }
    }
    fn sync(&self, ledger: &mut Ledger, now: i64) {
        ledger
            .sync_vault_utxos(&self.utxos, VAULT_MIN_CONFIRMATIONS, now)
            .unwrap();
    }
}

fn external_utxo(vault: &MultisigVault, tag: u8, amount_atomic: u64) -> VaultUtxo {
    VaultUtxo {
        txid: [tag; 32],
        vout: 0,
        amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    }
}

/// Builds, signs, reserves, and (optionally) broadcasts a real payout for
/// `request_id` under `policy`, exactly like the orchestrator does.
/// Returns the txid and the plan's inputs and change outputs.
async fn build_payout(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    signers: &[DevVaultSigner; 3],
    request_id: i64,
    policy: &PayoutPolicy,
    broadcast: bool,
    now: i64,
) -> ([u8; 32], Vec<VaultUtxo>, Vec<u64>) {
    let source = DevLedgerPayoutSource { ledger };
    let (p0, plan, unsigned_tx) = independently_sign(
        &signers[0],
        vault,
        &source,
        request_id,
        0,
        policy,
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let (p1, plan1, _) = independently_sign(
        &signers[1],
        vault,
        &source,
        request_id,
        0,
        policy,
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(plan, plan1, "independent re-derivation must agree");

    // Every fixture in this file funds its payout from exactly one input,
    // so single-input assembly (the same shape
    // goldcoin_payout_lifecycle.rs uses) is sufficient here; multi-input
    // assembly is exercised by that file and the incident suites.
    assert_eq!(
        plan.inputs.len(),
        1,
        "fixtures in this file are single-input by design"
    );
    let input_vault = &plan.input_contexts[0].vault;
    let sighash = unsigned_tx.sighash_all(0, &input_vault.redeem_script());
    let script_sig = multisig::assemble(input_vault, &sighash, &[p0, p1]).unwrap();
    let mut signed_tx = unsigned_tx.clone();
    signed_tx.inputs[0].script_sig = script_sig;

    let unsigned_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&unsigned_tx.serialize());
    ledger
        .record_goldcoin_payout_built(request_id, &plan, [0x99u8; 32], &unsigned_hex, now)
        .unwrap();
    ledger
        .reserve_vault_utxos(
            request_id,
            &plan.inputs,
            policy.zero_conf_change_max_depth,
            now,
        )
        .unwrap();
    let signed_hex = glc_reserve_bridge_service::goldcoin::hex::encode(&signed_tx.serialize());
    ledger
        .record_goldcoin_payout_signed(request_id, &signed_hex, now)
        .unwrap();
    let txid = signed_tx.txid();
    if broadcast {
        ledger
            .record_goldcoin_payout_broadcast(request_id, txid, now)
            .unwrap();
    }
    (txid, plan.inputs.clone(), plan.change_outputs.clone())
}

fn fold(ledger: &mut Ledger, index: u64, amount_solana: u64) -> i64 {
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            index,
            sol_to_glc_amounts(amount_solana),
            [index as u8 + 1; 32],
            dest_addr(index).as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!("expected FoldedFinalized")
    };
    request_id
}

fn outpoint_set(utxos: &[VaultUtxo]) -> Vec<([u8; 32], u32)> {
    utxos.iter().map(|u| (u.txid, u.vout)).collect()
}

// ---------------------------------------------------------------- external --

#[tokio::test]
async fn external_deposits_below_the_threshold_are_never_selectable_at_any_depth() {
    let (vault, _signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();

    // External deposits at 0 and at every 1..5 confirmation count: none
    // may enter the confirmed pool NOR the 0-conf policy pool — they have
    // no authoritative change-provenance row, and paying the vault script
    // is explicitly not provenance.
    for (i, conf) in [0i64, 1, 2, 3, 4, 5].iter().enumerate() {
        view.add(external_utxo(&vault, 0x10 + i as u8, 10_000_000), *conf);
    }
    view.sync(&mut ledger, 0);

    assert!(ledger.available_vault_utxos().unwrap().is_empty());
    for depth in [1u32, 2, 10] {
        assert!(
            ledger
                .zero_conf_change_vault_utxos(depth)
                .unwrap()
                .is_empty(),
            "no external deposit may ever qualify for the 0-conf policy (depth {depth})"
        );
    }
    // The reservation guard fails closed on them too, at any depth.
    let victim = external_utxo(&vault, 0x10, 10_000_000);
    let err = ledger
        .reserve_vault_utxos(1, std::slice::from_ref(&victim), 10, 0)
        .unwrap_err();
    assert!(matches!(
        err,
        glc_reserve_bridge_service::ledger::LedgerError::VaultUtxoUnavailable { .. }
    ));
}

#[tokio::test]
async fn external_deposit_at_the_threshold_is_selectable_normally() {
    let (vault, _signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    view.add(
        external_utxo(&vault, 0x20, 10_000_000),
        VAULT_MIN_CONFIRMATIONS,
    );
    view.sync(&mut ledger, 0);

    let available = ledger.available_vault_utxos().unwrap();
    assert_eq!(available.len(), 1);
    assert_eq!(available[0].txid, [0x20u8; 32]);
    // ...and it is confirmed liquidity, not a 0-conf policy candidate.
    assert!(ledger.zero_conf_change_vault_utxos(1).unwrap().is_empty());
}

#[tokio::test]
async fn arbitrary_vault_paying_output_at_zero_conf_never_qualifies() {
    let (vault, _signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();

    // An output that pays the vault's own script, from a transaction this
    // bridge never built — provenance unknown, therefore external policy,
    // full stop.
    view.add(external_utxo(&vault, 0x30, 50_000_000), 0);
    view.sync(&mut ledger, 0);
    assert!(ledger.zero_conf_change_vault_utxos(10).unwrap().is_empty());
}

// ------------------------------------------------------- authoritative change --

/// Drives request A end-to-end so its change output sits at 0
/// confirmations with an authoritative provenance row, and the confirmed
/// pool is empty. Returns (change utxo, A's request id, A's txid).
async fn setup_zero_conf_change(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    signers: &[DevVaultSigner; 3],
) -> (VaultUtxo, i64, [u8; 32]) {
    view.add(
        external_utxo(vault, 0xAA, 100_000_000),
        VAULT_MIN_CONFIRMATIONS,
    );
    view.sync(ledger, 0);

    let request_a = fold(ledger, 0, 500_000);
    let policy = test_policy(1);
    let (txid_a, inputs_a, change_a) =
        build_payout(ledger, vault, signers, request_a, &policy, true, 10).await;
    assert_eq!(inputs_a.len(), 1);
    assert_eq!(
        change_a.len(),
        1,
        "this fixture expects exactly one change output"
    );

    // The wallet now sees: funding input spent, change at vout 1, 0 conf.
    view.remove([0xAAu8; 32], 0);
    let change = VaultUtxo {
        txid: txid_a,
        vout: 1,
        amount_atomic: change_a[0],
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    view.add(change.clone(), 0);
    view.sync(ledger, 20);
    assert!(
        ledger.available_vault_utxos().unwrap().is_empty(),
        "no confirmed liquidity must remain in this fixture"
    );
    (change, request_a, txid_a)
}

#[tokio::test]
async fn authoritative_payout_change_at_zero_conf_is_selectable_and_funds_the_next_payout() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (change, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    let candidates = ledger.zero_conf_change_vault_utxos(1).unwrap();
    assert_eq!(outpoint_set(&candidates), vec![(txid_a, 1)]);
    assert_eq!(candidates[0].amount_atomic, change.amount_atomic);

    // A second request funds itself from that 0-conf change alone.
    let request_b = fold(&mut ledger, 1, 200_000);
    let policy = test_policy(1);
    let (txid_b, inputs_b, _change_b) =
        build_payout(&mut ledger, &vault, &signers, request_b, &policy, true, 30).await;
    assert_eq!(
        outpoint_set(&inputs_b),
        vec![(txid_a, 1)],
        "the payout must be funded by the authoritative 0-conf change"
    );
    assert_ne!(txid_b, txid_a);
}

#[tokio::test]
async fn confirmed_utxos_are_preferred_before_zero_conf_change() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (change, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    // Add a confirmed UTXO able to fund the next payout on its own — even
    // though the 0-conf change is LARGER (and would sort first in a naive
    // merged pool), the confirmed pool must win outright.
    let confirmed_amount = net_payout(200_000) + 1_000_000;
    assert!(confirmed_amount < change.amount_atomic);
    view.add(
        external_utxo(&vault, 0xBB, confirmed_amount),
        VAULT_MIN_CONFIRMATIONS,
    );
    view.sync(&mut ledger, 25);

    let request_b = fold(&mut ledger, 1, 200_000);
    let policy = test_policy(1);
    let (_txid_b, inputs_b, _c) =
        build_payout(&mut ledger, &vault, &signers, request_b, &policy, false, 30).await;
    assert_eq!(
        outpoint_set(&inputs_b),
        vec![([0xBBu8; 32], 0)],
        "confirmed liquidity must be preferred; 0-conf change is additional only"
    );
    assert!(!outpoint_set(&inputs_b).contains(&(txid_a, 1)));
}

#[tokio::test]
async fn kill_switch_depth_zero_disables_zero_conf_selection_entirely() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (_change, _a, _txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    assert!(ledger.zero_conf_change_vault_utxos(0).unwrap().is_empty());
    let request_b = fold(&mut ledger, 1, 200_000);
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let err = independently_sign(
        &signers[0],
        &vault,
        &source,
        request_b,
        0,
        &test_policy(0),
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, SigningError::CoinSelection(_)),
        "with the policy disabled, an empty confirmed pool must fail selection: {err:?}"
    );
}

// ----------------------------------------------------- parent validation/holds --

#[tokio::test]
async fn a_held_parent_removes_its_change_from_selection_and_reservation() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (change, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    assert_eq!(ledger.zero_conf_parent_txids().unwrap(), vec![txid_a]);
    ledger
        .set_zero_conf_hold(txid_a, Some("parent payout not accepted by node: test"))
        .unwrap();
    assert!(ledger.zero_conf_change_vault_utxos(10).unwrap().is_empty());
    // The reservation guard independently refuses the held row — a
    // dependent reservation FAILS, safely, instead of proceeding.
    let err = ledger
        .reserve_vault_utxos(99, std::slice::from_ref(&change), 10, 40)
        .unwrap_err();
    assert!(matches!(
        err,
        glc_reserve_bridge_service::ledger::LedgerError::VaultUtxoUnavailable { .. }
    ));

    // Re-acceptance clears the hold and restores eligibility.
    ledger.set_zero_conf_hold(txid_a, None).unwrap();
    assert_eq!(
        outpoint_set(&ledger.zero_conf_change_vault_utxos(1).unwrap()),
        vec![(txid_a, 1)]
    );
}

#[tokio::test]
async fn a_disappeared_parent_removes_its_change_immediately() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (change, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;
    assert!(!ledger.zero_conf_change_vault_utxos(1).unwrap().is_empty());

    // Reorg/eviction/conflict: the parent (and with it, the change
    // output) stops being reported by the wallet at all. The very next
    // sync takes the change out of every selectable pool.
    view.remove(txid_a, 1);
    view.sync(&mut ledger, 50);
    assert!(ledger.zero_conf_change_vault_utxos(10).unwrap().is_empty());
    let err = ledger
        .reserve_vault_utxos(99, std::slice::from_ref(&change), 10, 60)
        .unwrap_err();
    assert!(matches!(
        err,
        glc_reserve_bridge_service::ledger::LedgerError::VaultUtxoUnavailable { .. }
    ));
}

// ----------------------------------------------------------- ancestry limit --

#[tokio::test]
async fn unconfirmed_ancestry_limit_is_recorded_and_enforced() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (_change_a, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    // B spends A's 0-conf change (depth-1 input) — B's own change must be
    // recorded at depth 2.
    let request_b = fold(&mut ledger, 1, 200_000);
    let policy = test_policy(1);
    let (txid_b, _inputs_b, change_b) =
        build_payout(&mut ledger, &vault, &signers, request_b, &policy, true, 30).await;
    assert_eq!(change_b.len(), 1);
    let change_b_utxo = VaultUtxo {
        txid: txid_b,
        vout: 1,
        amount_atomic: change_b[0],
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    view.remove(txid_a, 1);
    view.add(change_b_utxo.clone(), 0);
    view.sync(&mut ledger, 40);

    // At the default cap (1), depth-2 change is NOT selectable while its
    // whole ancestor chain is unconfirmed; a deliberately raised cap (2)
    // admits it — proving the recorded depth, not a heuristic, drives the
    // decision. The reservation guard agrees in both directions.
    assert!(ledger.zero_conf_change_vault_utxos(1).unwrap().is_empty());
    assert_eq!(
        outpoint_set(&ledger.zero_conf_change_vault_utxos(2).unwrap()),
        vec![(txid_b, 1)]
    );
    assert!(ledger
        .reserve_vault_utxos(99, std::slice::from_ref(&change_b_utxo), 1, 50)
        .is_err());

    // Once B lands its first confirmation, every own-chain ancestor is
    // buried beneath it: the depth cap no longer applies, and the change
    // is selectable under the policy (still below the external
    // threshold).
    view.set_confirmations(txid_b, 1);
    view.sync(&mut ledger, 60);
    assert_eq!(
        outpoint_set(&ledger.zero_conf_change_vault_utxos(1).unwrap()),
        vec![(txid_b, 1)]
    );
}

// ------------------------------------------------------------------ restart --

#[tokio::test]
async fn provenance_and_ancestry_survive_a_daemon_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let (vault, signers) = setup_vault();
    let mut view = ChainView::default();
    let (change, txid_a) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        configure_reserves(&mut ledger);
        let (change, _a, txid_a) =
            setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;
        (change, txid_a)
    }; // ledger dropped: simulated daemon stop

    let reopened = Ledger::open(&db_path).unwrap();
    let candidates = reopened.zero_conf_change_vault_utxos(1).unwrap();
    assert_eq!(outpoint_set(&candidates), vec![(txid_a, 1)]);
    assert_eq!(candidates[0].amount_atomic, change.amount_atomic);
    assert_eq!(reopened.zero_conf_parent_txids().unwrap(), vec![txid_a]);
}

// ------------------------------------------------------- recovery/idempotency --

struct MissingInputsRpc;

impl GoldcoinRpc for MissingInputsRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        unimplemented!("not exercised")
    }
    async fn get_block_hash(&self, _height: i64) -> Result<String, RpcError> {
        unimplemented!("not exercised")
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("not exercised")
    }
    async fn get_raw_transaction(&self, _txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        Err(RpcError::Method {
            code: -5,
            message: "No such mempool or blockchain transaction".into(),
        })
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<glc_reserve_bridge_service::goldcoin::rpc::TxOut>, RpcError> {
        Ok(None)
    }
    async fn send_raw_transaction(&self, _hex: &str) -> Result<BroadcastOutcome, RpcError> {
        Ok(BroadcastOutcome::MissingInputs)
    }
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<ListUnspentEntry>, RpcError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn recovery_cannot_double_pay_after_an_unconfirmed_dependency_failure() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (_change, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    // B is built and signed on A's 0-conf change, but its broadcast never
    // succeeded (its dependency failed underneath it): B is stuck Signed.
    let request_b = fold(&mut ledger, 1, 200_000);
    let policy = test_policy(1);
    let (_txid_b, inputs_b, _c) =
        build_payout(&mut ledger, &vault, &signers, request_b, &policy, false, 30).await;
    assert_eq!(outpoint_set(&inputs_b), vec![(txid_a, 1)]);

    // Recovery re-derives the SAME transaction and the node rejects it
    // (missing inputs). The payout must stay exactly Signed — no second
    // payout row, no re-selection, no new transaction shape — preserving
    // the one-payout-per-request boundary.
    let boxed: Vec<Box<dyn glc_reserve_bridge_service::signing::signers::VaultSigner>> = signers
        .iter()
        .map(|s| {
            Box::new(DevVaultSigner {
                secret_key: s.secret_key,
                pubkey: s.pubkey,
            }) as Box<dyn glc_reserve_bridge_service::signing::signers::VaultSigner>
        })
        .collect();
    let rpc = MissingInputsRpc;
    let err = recover_stuck_goldcoin_payout(
        &mut ledger,
        &vault,
        &boxed,
        &rpc,
        request_b,
        2,
        &policy,
        Network::Testnet,
        TEST_SIGNER_TIMEOUT,
        100,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RecoveryError::BroadcastConflict(id) if id == request_b));
    let payout = ledger.get_goldcoin_payout_full(request_b).unwrap().unwrap();
    assert_eq!(
        payout.state, "Signed",
        "a failed dependency must never advance the payout"
    );
    assert_eq!(
        ledger.get_request(request_b).unwrap().unwrap().state,
        glc_reserve_bridge_service::ledger::RequestState::SettlementAuthorized
    );
}

// -------------------------------------------------- confirmed change / splits --

#[tokio::test]
async fn confirmed_change_still_flows_through_the_normal_confirmed_path() {
    let (vault, signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();
    let (change, _a, txid_a) =
        setup_zero_conf_change(&mut ledger, &mut view, &vault, &signers).await;

    // The change matures to the external threshold: it becomes ordinary
    // confirmed liquidity and leaves the 0-conf policy pool entirely.
    view.set_confirmations(txid_a, VAULT_MIN_CONFIRMATIONS);
    view.sync(&mut ledger, 100);
    let available = ledger.available_vault_utxos().unwrap();
    assert_eq!(outpoint_set(&available), vec![(txid_a, 1)]);
    assert_eq!(available[0].amount_atomic, change.amount_atomic);
    assert!(ledger.zero_conf_change_vault_utxos(10).unwrap().is_empty());
}

#[tokio::test]
async fn vault_split_outputs_never_receive_the_zero_conf_policy() {
    let (vault, _signers) = setup_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserves(&mut ledger);
    let mut view = ChainView::default();

    // A real recorded vault split: source is a mature vault UTXO, the
    // split is built, signed, and broadcast through the real ledger flow.
    let source = external_utxo(&vault, 0xC0, 10_000 * 100_000_000);
    view.add(source.clone(), VAULT_MIN_CONFIRMATIONS);
    view.sync(&mut ledger, 0);
    let plan = split::plan_split(&source, &vault, 2_500 * 100_000_000, 1000).unwrap();
    let split_id = ledger
        .record_vault_utxo_split_built(&plan, 2_500 * 100_000_000, "deadbeef", "test", 10)
        .unwrap();
    ledger
        .record_vault_utxo_split_signed(split_id, "deadbeef", 20)
        .unwrap();
    let split_txid = [0xC1u8; 32];
    ledger
        .record_vault_utxo_split_broadcast(
            split_id,
            split_txid,
            &plan.output_amounts,
            &vault.script_pubkey_hex(),
            30,
        )
        .unwrap();

    // Its outputs appear at 0 confirmations, paying the vault's own
    // script, from a transaction the bridge itself created — and STILL
    // must not qualify: split outputs are deliberately outside the 0-conf
    // policy (no `goldcoin_payout_change_outpoints` row exists for them).
    view.remove(source.txid, source.vout);
    for (i, amount) in plan.output_amounts.iter().enumerate() {
        view.add(
            VaultUtxo {
                txid: split_txid,
                vout: i as u32,
                amount_atomic: *amount,
                script_pubkey_hex: vault.script_pubkey_hex(),
            },
            0,
        );
    }
    view.sync(&mut ledger, 40);
    assert!(
        ledger.zero_conf_change_vault_utxos(10).unwrap().is_empty(),
        "split outputs must wait vault_min_confirmations like any external deposit"
    );
    assert!(ledger.available_vault_utxos().unwrap().is_empty());
}
