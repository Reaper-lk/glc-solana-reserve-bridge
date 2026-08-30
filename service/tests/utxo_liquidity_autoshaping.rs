//! Regression/integration tests for automatic UTXO liquidity shaping
//! (`goldcoin::liquidity`, docs/09-runbook.md's "Automatic UTXO liquidity
//! shaping" section) — the permanent fix for the 2026-08-30 production
//! incident: SolToGlc payouts repeatedly failing with `coin selection
//! failed: selection would require more than 10 inputs` while a large
//! amount of reserve liquidity sat as temporarily immature internal payout
//! change, with manual `glc-admin split-vault-utxo` runs as the only
//! remedy.
//!
//! Like `utxo_liquidity_incident.rs`, these tests drive the REAL
//! production code paths (`Ledger::fold_sol_deposit`,
//! `signing::goldcoin_vault::rederive_plan`, `goldcoin::coin::select`/
//! `finalize_fanout`, `goldcoin::liquidity::run_shaping_tick` — which
//! itself runs the real `signing::goldcoin_split` independent 2-of-3
//! signing, the full split lifecycle (`Built -> Signed -> Broadcast ->
//! Confirmed | Abandoned`), and the ledger's atomic split-broadcast
//! bookkeeping), never a hand-rolled simulation. The Goldcoin node is a
//! minimal accept-all broadcast double; `ChainView` keeps the full
//! `listunspent`-style snapshot contract `Ledger::sync_vault_utxos`
//! requires (see that file's module docs).
//!
//! # Zero-conf policy note (requirements D/E/F)
//!
//! Production DOES have a zero-conf spend path — the provenance-gated
//! 0-conf payout-change policy (`zero_conf_change_max_depth = 1`,
//! docs/09-runbook.md "Zero-conf payout change"), owned and pinned by
//! `tests/zero_conf_change_policy.rs`. This suite runs with that policy
//! DISABLED (`zero_conf_change_max_depth = 0`) so it pins the shaping/
//! maturity mechanics in isolation: with the policy off, unconfirmed
//! internal change of any depth is never selected while unconfirmed and
//! becomes selectable exactly at maturity, while the pool-health
//! machinery (fan-out, backpressure, automatic shaping) absorbs the
//! maturity window. The one place the two features compose —
//! split CHUNK outputs must never be 0-conf eligible at ANY depth
//! setting, because they carry no payout-change provenance row — is
//! pinned at depth 1 by `test_k_split_chunks_are_never_zero_conf_eligible`.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::indexer::GoldcoinRpc;
use glc_reserve_bridge_service::goldcoin::liquidity::{run_shaping_tick, ShapingPolicy};
use glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy;
use glc_reserve_bridge_service::goldcoin::rpc::{
    BlockHeader, BroadcastOutcome, DecodedTransaction, ListUnspentEntry, RpcError,
    TxOut as GoldcoinTxOut,
};
use glc_reserve_bridge_service::goldcoin::split;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, LedgerError, ReserveDirection, SolFoldOutcome};
use glc_reserve_bridge_service::reconciliation::{self, Classification};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    DevLedgerPayoutSource, DevVaultSigner, IndependentPayoutSource, SigningError,
};
use glc_reserve_bridge_service::signing::signers::VaultSigner;

const GLC: u64 = 100_000_000; // 1 GLC = 100,000,000 atomic units (8 decimals)
const MIN_CONFIRMATIONS: i64 = 6;
const SIGNER_TIMEOUT: Duration = Duration::from_secs(5);

fn vault_and_signers() -> (MultisigVault, Vec<Box<dyn VaultSigner>>) {
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
    let boxed = signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect();
    (vault, boxed)
}

/// The 2026-08-30 production-shaped payout policy: real production fee
/// rate, the retuned 5,000 GLC fan-out chunk target, and the explicit
/// max_inputs = 25 decision (service/config.pilot-template.toml).
fn policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 100_000,
        dust_threshold: 1_000,
        max_inputs: 25,
        change_fanout_target_atomic: 5_000 * GLC,
        change_fanout_max_outputs: 10,
        // 0 here, deliberately: this suite pins the shaping/maturity
        // mechanics in isolation from the (orthogonal, separately tested)
        // zero-conf payout-change policy — see
        // `tests/zero_conf_change_policy.rs` for that policy's own
        // coverage and `test_k_split_chunks_are_never_zero_conf_eligible`
        // below for the one place the two compose.
        zero_conf_change_max_depth: 0,
    }
}

/// The shipped shaping defaults (`service/src/config.rs`).
fn shaping_policy() -> ShapingPolicy {
    ShapingPolicy {
        chunk_target_atomic: 5_000 * GLC,
        fee_rate_per_kb: 100_000,
        target_available_count: 15,
        min_source_atomic: 20_000 * GLC,
        max_outputs_per_split: 25,
        zero_conf_change_max_depth: 0,
    }
}

fn distinct_recipient(obligation_index: u64) -> String {
    let mut hash = [0u8; 20];
    hash[..8].copy_from_slice(&obligation_index.to_be_bytes());
    glc_reserve_bridge_service::goldcoin::address::encode_p2pkh(&hash, Network::Testnet)
}

fn distinct_wallet(obligation_index: u64) -> [u8; 32] {
    let mut wallet = [9u8; 32];
    wallet[24..32].copy_from_slice(&obligation_index.to_be_bytes());
    wallet
}

fn amounts_for_gross_glc(gross_glc: u64) -> glc_reserve_bridge_service::ledger::RequestAmounts {
    let fb = compute_fee(CanonicalAtomic(gross_glc * GLC)).unwrap();
    glc_reserve_bridge_service::ledger::RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

fn configure_reserve(ledger: &mut Ledger, total_balance_atomic: u64, utxo_floor: u32) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            total_balance_atomic,
            20_000 * GLC, // production protected minimum
            1_500_000 * GLC,
            50_000 * GLC,
            30_000 * GLC,
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
    ledger
        .set_utxo_pool_thresholds(
            ReserveDirection::GoldcoinReserve,
            utxo_floor,
            utxo_floor + 5,
        )
        .unwrap();
}

/// Accept-all broadcast double; every other RPC surface is unreachable
/// from these tests and fails loudly if touched (same discipline as
/// `split_recovery`'s own `TestGoldcoinRpc`).
struct AcceptAllRpc {
    submitted: Mutex<Vec<String>>,
    /// Txids (hex) the simulated node has "forgotten" (mempool eviction):
    /// `get_raw_transaction` answers a -5 method error for these, exactly
    /// like a Bitcoin-family node that no longer knows the transaction.
    evicted: Mutex<std::collections::HashSet<String>>,
    /// When set, every subsequent broadcast is refused with
    /// `MissingInputs` — the node-side symptom of a source spent by a
    /// conflicting transaction.
    refuse_missing_inputs: Mutex<bool>,
}

impl AcceptAllRpc {
    fn new() -> Self {
        AcceptAllRpc {
            submitted: Mutex::new(Vec::new()),
            evicted: Mutex::new(std::collections::HashSet::new()),
            refuse_missing_inputs: Mutex::new(false),
        }
    }
    fn broadcast_count(&self) -> usize {
        self.submitted.lock().unwrap().len()
    }
    fn evict(&self, txid: [u8; 32]) {
        self.evicted
            .lock()
            .unwrap()
            .insert(glc_reserve_bridge_service::goldcoin::hex::encode(&txid));
    }
    fn restore(&self, txid: [u8; 32]) {
        self.evicted
            .lock()
            .unwrap()
            .remove(&glc_reserve_bridge_service::goldcoin::hex::encode(&txid));
    }
    fn set_refuse_missing_inputs(&self, refuse: bool) {
        *self.refuse_missing_inputs.lock().unwrap() = refuse;
    }
}

impl GoldcoinRpc for AcceptAllRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        unimplemented!("not exercised by shaping tests")
    }
    async fn get_block_hash(&self, _height: i64) -> Result<String, RpcError> {
        unimplemented!("not exercised by shaping tests")
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("not exercised by shaping tests")
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        // The simulated node knows exactly the transactions that were
        // actually submitted to it (their txids computed from the real
        // submitted bytes), minus any the test has evicted — the same
        // membership semantics a real node's getrawtransaction has.
        let known = !self.evicted.lock().unwrap().contains(txid_hex)
            && self.submitted.lock().unwrap().iter().any(|hex| {
                let bytes = glc_reserve_bridge_service::goldcoin::hex::decode_vec(hex).unwrap();
                let txid = glc_reserve_bridge_service::goldcoin::tx::txid_of_serialized(&bytes);
                glc_reserve_bridge_service::goldcoin::hex::encode(&txid) == txid_hex
            });
        if !known {
            return Err(RpcError::Method {
                code: -5,
                message: "No such mempool or blockchain transaction".to_string(),
            });
        }
        Ok(DecodedTransaction {
            txid: txid_hex.to_string(),
            vout: vec![],
            confirmations: None,
        })
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<GoldcoinTxOut>, RpcError> {
        unimplemented!("not exercised by shaping tests")
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        if *self.refuse_missing_inputs.lock().unwrap() {
            return Ok(BroadcastOutcome::MissingInputs);
        }
        self.submitted.lock().unwrap().push(hex.to_string());
        Ok(BroadcastOutcome::Accepted {
            txid: "00".repeat(32),
        })
    }
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<ListUnspentEntry>, RpcError> {
        unimplemented!("not exercised by shaping tests")
    }
}

/// Full running `listunspent`-style snapshot — see
/// `utxo_liquidity_incident.rs`'s module docs for the full-snapshot
/// contract.
struct ChainView {
    entries: BTreeMap<([u8; 32], u32), (VaultUtxo, i64)>,
}

impl ChainView {
    fn new() -> Self {
        ChainView {
            entries: BTreeMap::new(),
        }
    }
    fn observe(&mut self, utxo: VaultUtxo, confirmations: i64) {
        self.entries
            .insert((utxo.txid, utxo.vout), (utxo, confirmations));
    }
    fn bump_confirmations(&mut self, txid: [u8; 32], confirmations: i64) {
        for ((t, _), (_, conf)) in self.entries.iter_mut() {
            if *t == txid {
                *conf = confirmations;
            }
        }
    }
    fn mature_everything(&mut self) {
        for (_, conf) in self.entries.values_mut() {
            *conf = MIN_CONFIRMATIONS;
        }
    }
    fn sync(&self, ledger: &mut Ledger, now: i64) {
        let observed: Vec<_> = self
            .entries
            .values()
            .map(|(utxo, conf)| (utxo.clone(), *conf, utxo.script_pubkey_hex.clone()))
            .collect();
        ledger
            .sync_vault_utxos(&observed, MIN_CONFIRMATIONS, now)
            .unwrap();
    }

    /// Mirrors on-chain reality after a broadcast split: the source
    /// outpoint is spent (removed from the snapshot) and the chunk
    /// outputs appear at 0 confirmations. Reads the split's own persisted
    /// figures back from the ledger, never trusting test-local state.
    fn absorb_split(&mut self, ledger: &Ledger, split_txid: [u8; 32], vault: &MultisigVault) {
        let broadcast = ledger
            .get_broadcast_vault_utxo_split(split_txid)
            .unwrap()
            .expect("split must be Broadcast");
        // Remove every view entry the ledger now knows is Spent.
        let spent: Vec<_> = self
            .entries
            .keys()
            .filter(|(txid, vout)| {
                ledger
                    .get_vault_utxo(*txid, *vout)
                    .unwrap()
                    .map(|row| row.state == "Spent")
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for key in spent {
            self.entries.remove(&key);
        }
        let amounts = split::distribute_evenly(
            broadcast
                .source_amount_atomic
                .saturating_sub(broadcast.fee_atomic),
            broadcast.chunk_count as u64,
        );
        for (i, amount) in amounts.into_iter().enumerate() {
            self.observe(
                VaultUtxo {
                    txid: split_txid,
                    vout: i as u32,
                    amount_atomic: amount,
                    script_pubkey_hex: vault.script_pubkey_hex(),
                },
                0,
            );
        }
    }
}

fn seed_mature_root_utxo(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    tag: u8,
    amount_atomic: u64,
    confirmations: i64,
) -> VaultUtxo {
    let mut txid = [0u8; 32];
    txid[0] = 0xD0;
    txid[1] = tag;
    let utxo = VaultUtxo {
        txid,
        vout: 0,
        amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    view.observe(utxo.clone(), confirmations);
    view.sync(ledger, 0);
    utxo
}

fn refresh_reconciliation(ledger: &mut Ledger, now: i64) -> reconciliation::ReconciliationReport {
    let observed_balance = ledger
        .available_vault_utxos()
        .unwrap()
        .iter()
        .map(|u| u.amount_atomic)
        .sum();
    reconciliation::reconcile(
        ledger,
        ReserveDirection::GoldcoinReserve,
        observed_balance,
        0,
        now,
    )
    .unwrap()
}

/// Folds one obligation and, if admitted, drives it through the REAL
/// rederive -> select -> fan-out -> persist path to `Broadcast`,
/// registering the change output(s) as freshly observed 0-conf outputs —
/// exactly `utxo_liquidity_incident.rs`'s production-path helper.
fn admit_and_broadcast_one(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    obligation_index: u64,
    gross_glc: u64,
    now: i64,
) -> (SolFoldOutcome, Option<[u8; 32]>) {
    let pre = refresh_reconciliation(ledger, now);
    assert_ne!(
        pre.classification,
        Classification::Breach,
        "obligation {obligation_index}: pre-admission reconciliation must never find a breach \
         in these scenarios: {pre:?}"
    );
    let outcome = ledger
        .fold_sol_deposit(
            obligation_index,
            amounts_for_gross_glc(gross_glc),
            distinct_wallet(obligation_index),
            distinct_recipient(obligation_index).as_bytes(),
            now,
        )
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        return (outcome, None);
    };
    let txid = build_and_broadcast_payout(ledger, view, vault, request_id, now);
    (outcome, Some(txid))
}

fn build_and_broadcast_payout(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    request_id: i64,
    now: i64,
) -> [u8; 32] {
    let source = DevLedgerPayoutSource { ledger };
    let plan = source
        .rederive_plan(request_id, vault, &policy(), Network::Testnet)
        .unwrap();
    assert!(
        plan.inputs.len() <= policy().max_inputs,
        "request {request_id}: selection exceeded max_inputs"
    );
    ledger
        .reserve_vault_utxos(
            request_id,
            &plan.inputs,
            policy().zero_conf_change_max_depth,
            now,
        )
        .unwrap();
    ledger
        .record_goldcoin_payout_built(request_id, &plan, [0x77u8; 32], "00", now)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, "00", now)
        .unwrap();
    let mut txid = [0u8; 32];
    txid[0] = 0xF0;
    txid[16..24].copy_from_slice(&request_id.to_be_bytes());
    ledger
        .record_goldcoin_payout_broadcast(request_id, txid, now)
        .unwrap();
    for input in &plan.inputs {
        view.entries.remove(&(input.txid, input.vout));
    }
    for (i, &amount_atomic) in plan.change_outputs.iter().enumerate() {
        view.observe(
            VaultUtxo {
                txid,
                vout: (i + 1) as u32,
                amount_atomic,
                script_pubkey_hex: vault.script_pubkey_hex(),
            },
            0,
        );
    }
    view.sync(ledger, now);
    txid
}

fn settle_payout(ledger: &mut Ledger, request_id: i64, now: i64) {
    ledger
        .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, now)
        .unwrap();
    ledger
        .record_goldcoin_completion_submitted(request_id, [0x66u8; 64], now)
        .unwrap();
    ledger
        .mark_goldcoin_completion_confirmed(request_id, now)
        .unwrap();
}

/// One shaping pass through the real `run_shaping_tick`, absorbing any
/// broadcast split into the chain view exactly as the next `listunspent`
/// scan would show it.
async fn shaping_tick(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    signers: &[Box<dyn VaultSigner>],
    rpc: &AcceptAllRpc,
    now: i64,
) -> glc_reserve_bridge_service::goldcoin::liquidity::ShapingOutcome {
    let outcome = run_shaping_tick(
        ledger,
        rpc,
        vault,
        signers,
        2,
        &shaping_policy(),
        SIGNER_TIMEOUT,
        now,
    )
    .await
    .unwrap();
    if let Some(txid) = outcome.new_split_txid.or(outcome.resumed_split_txid) {
        view.absorb_split(ledger, txid, vault);
        view.sync(ledger, now);
    }
    outcome
}

// =======================================================================
// Requirement A + J: one confirmed 1,000,000 GLC vault deposit, then
// repeated payouts up to 20,000 GLC gross each, with NO manual UTXO
// split anywhere — the bridge shapes its own liquidity.
// =======================================================================
#[tokio::test]
async fn test_a_one_million_glc_deposit_funds_repeated_max_payouts_with_no_manual_split() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();

    // The operator's single 1,000,000 GLC deposit, already mature.
    seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 1_000_000 * GLC, 20);
    configure_reserve(&mut ledger, 1_000_000 * GLC, 0);

    let mut obligation_index = 0u64;
    let mut now = 1_000i64;
    let mut splits_seen = 0u32;
    let mut finalized = 0u32;
    let mut used_outpoints = std::collections::HashSet::new();
    let mut parked: Vec<i64> = Vec::new();

    for _cycle in 0..6 {
        let mut cycle_requests = Vec::new();
        // First, retry anything parked earlier now that liquidity may
        // have matured — the same resume path the orchestrator's
        // auto-resume tick reuses verbatim.
        parked.retain(|&request_id| {
            match ledger.resume_manual_review_sol_to_glc(request_id, "auto-retry", "test", now) {
                Ok(_) => {
                    let txid =
                        build_and_broadcast_payout(&mut ledger, &mut view, &vault, request_id, now);
                    assert_ne!(txid, [0u8; 32]);
                    cycle_requests.push(request_id);
                    false
                }
                Err(_) => true, // still not resumable this cycle
            }
        });
        for _ in 0..4 {
            let (outcome, _) = admit_and_broadcast_one(
                &mut ledger,
                &mut view,
                &vault,
                obligation_index,
                20_000,
                now,
            );
            match outcome {
                SolFoldOutcome::FoldedFinalized { request_id } => cycle_requests.push(request_id),
                SolFoldOutcome::FoldedManualReview { request_id } => parked.push(request_id),
                SolFoldOutcome::AlreadyFolded { .. } => unreachable!(),
            }
            obligation_index += 1;
            now += 1;
        }

        // The daemon's shaping pass (post-payouts position, as in the
        // real tick).
        let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, now).await;
        if outcome.new_split_txid.is_some() || outcome.resumed_split_txid.is_some() {
            splits_seen += 1;
        }
        let report = refresh_reconciliation(&mut ledger, now);
        assert_ne!(
            report.classification,
            Classification::Breach,
            "cycle reconciliation must explain all in-flight internal value: {report:?}"
        );
        assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());

        // No double-selected outpoint anywhere in the run.
        for request_id in &cycle_requests {
            for input in ledger.get_goldcoin_payout_inputs(*request_id).unwrap() {
                assert!(
                    used_outpoints.insert((input.txid, input.vout)),
                    "outpoint reused across payouts"
                );
            }
            settle_payout(&mut ledger, *request_id, now);
            finalized += 1;
        }

        // ~6 blocks pass: everything matures before the next burst.
        view.mature_everything();
        view.sync(&mut ledger, now);
        now += 100;
        // A post-maturity shaping pass too — this is where a thin pool
        // with a freshly matured oversized UTXO actually restructures.
        let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, now).await;
        if outcome.new_split_txid.is_some() || outcome.resumed_split_txid.is_some() {
            splits_seen += 1;
        }
        view.mature_everything();
        view.sync(&mut ledger, now);
        now += 100;
    }

    assert!(
        finalized >= 16,
        "sustained 20,000 GLC payouts must keep flowing (finalized {finalized})"
    );
    assert!(
        splits_seen >= 1,
        "automatic shaping must have restructured the oversized deposit at least once"
    );
    assert!(
        rpc.broadcast_count() as u32 == splits_seen,
        "every shaping action must have gone through a real broadcast"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

// =======================================================================
// The stated operational bootstrap: floor = 10 (production), the 1M GLC
// deposit is the vault's ONLY liquidity. Admission parks on the count
// floor, shaping restructures the deposit anyway (its value never leaves
// the vault), and after 6 confirmations everything resumes with no
// operator action.
// =======================================================================
#[tokio::test]
async fn test_j_bootstrap_single_giant_deposit_with_production_floor_self_recovers() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();

    // Deposit observed below maturity first: requirement E/J — never
    // selectable, never counted available.
    let deposit = seed_mature_root_utxo(&mut ledger, &mut view, &vault, 2, 1_000_000 * GLC, 5);
    configure_reserve(&mut ledger, 0, 10);
    assert!(
        ledger.available_vault_utxos().unwrap().is_empty(),
        "an external deposit below vault_min_confirmations must not be selectable"
    );

    // Maturity: 6 confirmations.
    view.bump_confirmations(deposit.txid, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, 10);
    refresh_reconciliation(&mut ledger, 10);
    assert_eq!(ledger.available_vault_utxos().unwrap().len(), 1);

    // Admission parks on the count floor (1 <= 10) — by design.
    let outcome = ledger
        .fold_sol_deposit(
            0,
            amounts_for_gross_glc(20_000),
            distinct_wallet(0),
            distinct_recipient(0).as_bytes(),
            11,
        )
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("expected the count-floor to park the obligation, got {outcome:?}")
    };

    // Shaping restructures the deposit even though it IS the reserve —
    // the solvency-aligned safety check (balance - fee >= floor) knows
    // the chunks stay vault-owned, ledger-tracked value.
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 12).await;
    let split_txid = outcome
        .new_split_txid
        .expect("bootstrap shaping split must happen");
    assert_eq!(
        ledger
            .get_vault_utxo(deposit.txid, deposit.vout)
            .unwrap()
            .unwrap()
            .state,
        "Spent",
        "the split source must be unselectable the instant the split broadcasts"
    );
    let health = ledger.utxo_pool_health().unwrap();
    assert_eq!(health.available_utxo_count, 0);
    assert_eq!(
        health.unconfirmed_change_utxo_count, 25,
        "all 25 chunks tracked as known immature internal value"
    );
    // Reconciliation across the immaturity window: fully explained.
    let report = refresh_reconciliation(&mut ledger, 13);
    assert_ne!(report.classification, Classification::Breach, "{report:?}");
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());

    // Six confirmations later, the chunks mature…
    view.bump_confirmations(split_txid, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, 20);
    refresh_reconciliation(&mut ledger, 20);
    assert_eq!(
        ledger.available_vault_utxos().unwrap().len(),
        25,
        "matured chunks are ordinary available liquidity"
    );

    // …and the parked obligation resumes and pays out, no operator UTXO
    // maintenance anywhere.
    ledger
        .resume_manual_review_sol_to_glc(request_id, "auto-retry", "test", 21)
        .unwrap();
    let txid = build_and_broadcast_payout(&mut ledger, &mut view, &vault, request_id, 21);
    assert_ne!(txid, [0u8; 32]);
    let inputs = ledger.get_goldcoin_payout_inputs(request_id).unwrap();
    assert!(
        inputs.len() <= 25 && !inputs.is_empty(),
        "payout built from the shaped chunks: {} inputs",
        inputs.len()
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

// =======================================================================
// Requirement I: a healthy pool never produces self-transactions.
// =======================================================================
#[tokio::test]
async fn test_i_healthy_pool_produces_no_self_transactions() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();

    // 20 payout-ready chunks (>= target_available_count = 15) plus one
    // oversized UTXO that WOULD be a candidate if the pool were thin.
    for i in 0..20u8 {
        seed_mature_root_utxo(&mut ledger, &mut view, &vault, 10 + i, 5_000 * GLC, 20);
    }
    seed_mature_root_utxo(&mut ledger, &mut view, &vault, 99, 1_000_000 * GLC, 20);
    configure_reserve(&mut ledger, 1_100_000 * GLC, 10);
    refresh_reconciliation(&mut ledger, 5);

    for tick in 0..10 {
        let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 10 + tick).await;
        assert!(outcome.new_split_txid.is_none() && outcome.resumed_split_txid.is_none());
        assert!(
            outcome.skipped.as_deref().unwrap_or("").contains("healthy"),
            "expected a pool-healthy skip, got {outcome:?}"
        );
    }
    assert_eq!(
        rpc.broadcast_count(),
        0,
        "no transaction of any kind while the pool is healthy"
    );
}

// =======================================================================
// Shaping waits while a previous split's chunks are still maturing —
// never stacks self-transactions.
// =======================================================================
#[tokio::test]
async fn test_i2_shaping_does_not_stack_splits_while_chunks_mature() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();

    // Two oversized UTXOs, thin pool: only ONE may split per maturity
    // window.
    seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 200_000 * GLC, 20);
    seed_mature_root_utxo(&mut ledger, &mut view, &vault, 2, 150_000 * GLC, 20);
    configure_reserve(&mut ledger, 350_000 * GLC, 0);
    refresh_reconciliation(&mut ledger, 5);

    let first = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 10).await;
    assert!(first.new_split_txid.is_some());
    for tick in 0..5 {
        let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 20 + tick).await;
        assert!(
            outcome.new_split_txid.is_none(),
            "no second split while the first one's chunks are immature: {outcome:?}"
        );
        assert!(
            outcome
                .skipped
                .as_deref()
                .unwrap_or("")
                .contains("maturing"),
            "expected a chunks-still-maturing skip, got {outcome:?}"
        );
    }
    assert_eq!(rpc.broadcast_count(), 1);
}

// =======================================================================
// Requirements D + F: confirmed liquidity is what selection uses;
// unconfirmed internal change (any depth) is never selected while
// unconfirmed and becomes selectable exactly at maturity.
// =======================================================================
#[tokio::test]
async fn test_d_f_unconfirmed_internal_change_is_never_selected_until_maturity() {
    let (vault, _signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    for i in 0..12u8 {
        seed_mature_root_utxo(&mut ledger, &mut view, &vault, i, 25_000 * GLC, 20);
    }
    configure_reserve(&mut ledger, 12 * 25_000 * GLC, 0);

    // Payout 1 produces unconfirmed depth-1 change.
    let (outcome, txid1) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 20_000, 100);
    assert!(matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }));
    let txid1 = txid1.unwrap();

    // Requirement D: the next payout's selection must draw only from
    // confirmed liquidity — never from payout 1's unconfirmed change.
    let (outcome2, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 1, 20_000, 101);
    let SolFoldOutcome::FoldedFinalized { request_id: r2 } = outcome2 else {
        panic!("expected admission")
    };
    for input in ledger.get_goldcoin_payout_inputs(r2).unwrap() {
        assert_ne!(
            input.txid, txid1,
            "selection must never touch unconfirmed internal change"
        );
    }

    // Requirement F: with ONLY unconfirmed change left and the 0-conf
    // policy disabled (depth 0), selection fails closed (waits) rather
    // than spending zero-conf. (At the production depth-1 setting,
    // provenance-gated payout change WOULD be eligible here — that path
    // is zero_conf_change_policy.rs's to pin, not this suite's.)
    let available_now = ledger.available_vault_utxos().unwrap();
    for u in available_now {
        let mut fake_spender = [0xEEu8; 32];
        fake_spender[31] = u.txid[1];
        // Simulate the rest of the pool being consumed externally: remove
        // from the view so the next sync marks them Spent.
        view.entries.remove(&(u.txid, u.vout));
        let _ = fake_spender;
    }
    view.sync(&mut ledger, 102);
    assert!(
        ledger.available_vault_utxos().unwrap().is_empty(),
        "only unconfirmed change remains"
    );
    let outcome3 = ledger
        .fold_sol_deposit(
            2,
            amounts_for_gross_glc(1_000),
            distinct_wallet(2),
            distinct_recipient(2).as_bytes(),
            103,
        )
        .unwrap();
    if let SolFoldOutcome::FoldedFinalized { request_id: r3 } = outcome3 {
        // If value-accounting admitted it, coin selection itself must
        // still fail closed with Insufficient — never select zero-conf.
        let source = DevLedgerPayoutSource { ledger: &ledger };
        let err = source
            .rederive_plan(r3, &vault, &policy(), Network::Testnet)
            .unwrap_err();
        assert!(
            matches!(err, SigningError::CoinSelection(_)),
            "selection over an unconfirmed-only pool must fail closed at depth 0, got {err:?}"
        );
        // Depth-1 change matures -> becomes selectable per current policy.
        view.bump_confirmations(txid1, MIN_CONFIRMATIONS);
        view.sync(&mut ledger, 104);
        let source = DevLedgerPayoutSource { ledger: &ledger };
        let plan = source
            .rederive_plan(r3, &vault, &policy(), Network::Testnet)
            .unwrap();
        assert!(plan.inputs.iter().all(|i| i.txid == txid1));
    } else {
        panic!("value accounting should have admitted the small request: {outcome3:?}");
    }
}

// =======================================================================
// Requirement E: an external deposit below 6 confirmations is never
// selectable, and matures into selectability at exactly the threshold.
// =======================================================================
#[tokio::test]
async fn test_e_external_deposit_below_six_confirmations_is_never_selectable() {
    let (vault, _signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let deposit = seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 50_000 * GLC, 1);
    configure_reserve(&mut ledger, 0, 0);

    for conf in 1..MIN_CONFIRMATIONS {
        view.bump_confirmations(deposit.txid, conf);
        view.sync(&mut ledger, conf);
        assert!(
            ledger.available_vault_utxos().unwrap().is_empty(),
            "at {conf} confirmations the deposit must not be selectable"
        );
    }
    view.bump_confirmations(deposit.txid, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, 10);
    assert_eq!(ledger.available_vault_utxos().unwrap().len(), 1);
}

// =======================================================================
// Requirement G: two payouts can never reserve the same UTXO — the
// guarded UPDATE is the concurrency boundary, exercised through two
// independent connections onto the same database file.
// =======================================================================
#[tokio::test]
async fn test_g_concurrent_reservation_cannot_double_select_an_input() {
    let (vault, _signers) = vault_and_signers();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let mut ledger_a = Ledger::open(&db_path).unwrap();
    let mut view = ChainView::new();
    for i in 0..3u8 {
        seed_mature_root_utxo(&mut ledger_a, &mut view, &vault, i, 25_000 * GLC, 20);
    }
    configure_reserve(&mut ledger_a, 75_000 * GLC, 0);

    let SolFoldOutcome::FoldedFinalized { request_id: r1 } = ledger_a
        .fold_sol_deposit(
            0,
            amounts_for_gross_glc(2_000),
            distinct_wallet(0),
            distinct_recipient(0).as_bytes(),
            100,
        )
        .unwrap()
    else {
        panic!()
    };
    let SolFoldOutcome::FoldedFinalized { request_id: r2 } = ledger_a
        .fold_sol_deposit(
            1,
            amounts_for_gross_glc(2_000),
            distinct_wallet(1),
            distinct_recipient(1).as_bytes(),
            101,
        )
        .unwrap()
    else {
        panic!()
    };

    // Both "operators" re-derive against the same snapshot and race the
    // same selection.
    let plan_a = DevLedgerPayoutSource { ledger: &ledger_a }
        .rederive_plan(r1, &vault, &policy(), Network::Testnet)
        .unwrap();
    let mut ledger_b = Ledger::open(&db_path).unwrap();
    let plan_b = DevLedgerPayoutSource { ledger: &ledger_b }
        .rederive_plan(r2, &vault, &policy(), Network::Testnet)
        .unwrap();
    assert_eq!(
        plan_a.inputs, plan_b.inputs,
        "deterministic selection: both raced plans want the same inputs"
    );

    ledger_a
        .reserve_vault_utxos(r1, &plan_a.inputs, 0, 102)
        .unwrap();
    let err = ledger_b
        .reserve_vault_utxos(r2, &plan_b.inputs, 0, 103)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::VaultUtxoUnavailable { .. }),
        "the losing reservation must fail whole, got {err:?}"
    );
    // And nothing was partially reserved for the loser.
    for input in &plan_b.inputs {
        let row = ledger_b
            .get_vault_utxo(input.txid, input.vout)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "Reserved");
    }
    let winner_inputs = ledger_a.get_goldcoin_payout_inputs(r1);
    // r1 has no payout row yet — the reservation itself is what we
    // checked; get_goldcoin_payout_inputs requires a payout row, so an
    // error here is expected and irrelevant to the double-select proof.
    let _ = winner_inputs;
}

// =======================================================================
// Requirement H: restart/recovery. Reservations and payout-change
// provenance survive a reopen; a split stuck in Signed or Built resumes
// automatically on the next shaping tick.
// =======================================================================
#[tokio::test]
async fn test_h_restart_preserves_reservations_and_resumes_stuck_splits() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let mut view = ChainView::new();

    let (request_id, payout_txid, split_source) = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for i in 0..14u8 {
            seed_mature_root_utxo(&mut ledger, &mut view, &vault, i, 25_000 * GLC, 20);
        }
        // One oversized UTXO whose split we will strand in `Signed`.
        let split_source =
            seed_mature_root_utxo(&mut ledger, &mut view, &vault, 99, 100_000 * GLC, 20);
        configure_reserve(&mut ledger, (14 * 25_000 + 100_000) * GLC, 0);

        let (outcome, payout_txid) =
            admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 20_000, 100);
        let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
            panic!()
        };

        // Strand a split exactly as a crash after signing would: Built +
        // Signed recorded, broadcast never confirmed to us.
        let plan = split::plan_split(&split_source, &vault, 5_000 * GLC, 100_000).unwrap();
        let unsigned_tx = split::build_unsigned_split_tx(&plan);
        let unsigned_hex =
            glc_reserve_bridge_service::goldcoin::hex::encode(&unsigned_tx.serialize());
        let split_id = ledger
            .record_vault_utxo_split_built(&plan, 5_000 * GLC, &unsigned_hex, "test", 101)
            .unwrap();
        ledger
            .record_vault_utxo_split_signed(split_id, &unsigned_hex, 102)
            .unwrap();
        (request_id, payout_txid.unwrap(), split_source)
        // process "crashes" here
    };

    // ---- restart ----
    let mut ledger = Ledger::open(&db_path).unwrap();

    // Reservations survived exactly.
    let inputs = ledger.get_goldcoin_payout_inputs(request_id).unwrap();
    assert!(!inputs.is_empty());
    for input in &inputs {
        let row = ledger
            .get_vault_utxo(input.txid, input.vout)
            .unwrap()
            .unwrap();
        assert_eq!(row.state, "Reserved");
    }
    // Payout-change provenance survived: the broadcast payout's change
    // rows still match its txid and are still immature.
    let payout = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    assert_eq!(
        payout.change_atomic,
        payout.change_outputs.iter().sum::<u64>()
    );
    assert!(ledger.own_unconfirmed_change_atomic().unwrap() >= payout.change_atomic);
    let _ = payout_txid;

    // The stranded Signed split resumes on the very next shaping tick —
    // re-broadcasting the EXACT stored bytes, then applying the broadcast
    // effects.
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 200).await;
    let resumed = outcome
        .resumed_split_txid
        .expect("the Signed split must resume before any new split is considered");
    assert_eq!(rpc.broadcast_count(), 1);
    assert_eq!(
        ledger
            .get_vault_utxo(split_source.txid, split_source.vout)
            .unwrap()
            .unwrap()
            .state,
        "Spent"
    );
    assert!(ledger
        .get_broadcast_vault_utxo_split(resumed)
        .unwrap()
        .is_some());
    let report = refresh_reconciliation(&mut ledger, 201);
    assert_ne!(report.classification, Classification::Breach, "{report:?}");
}

#[tokio::test]
async fn test_h2_split_stranded_in_built_is_resigned_and_broadcast_after_restart() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let mut view = ChainView::new();

    let split_source = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let split_source =
            seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 100_000 * GLC, 20);
        configure_reserve(&mut ledger, 100_000 * GLC, 0);
        let plan = split::plan_split(&split_source, &vault, 5_000 * GLC, 100_000).unwrap();
        let unsigned_tx = split::build_unsigned_split_tx(&plan);
        let unsigned_hex =
            glc_reserve_bridge_service::goldcoin::hex::encode(&unsigned_tx.serialize());
        ledger
            .record_vault_utxo_split_built(&plan, 5_000 * GLC, &unsigned_hex, "test", 101)
            .unwrap();
        split_source
        // crash between record_built and record_signed
    };

    let mut ledger = Ledger::open(&db_path).unwrap();
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 200).await;
    let resumed = outcome
        .resumed_split_txid
        .expect("the Built split must re-sign and broadcast");
    assert_eq!(rpc.broadcast_count(), 1);
    let snapshot = ledger
        .get_vault_utxo_split(split_source.txid, split_source.vout)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.state, "Broadcast");
    assert_eq!(snapshot.txid, Some(resumed));
    assert_eq!(
        ledger
            .get_vault_utxo(split_source.txid, split_source.vout)
            .unwrap()
            .unwrap()
            .state,
        "Spent"
    );
    // And the resume is terminal: the next tick finds nothing pending.
    let outcome2 = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 201).await;
    assert!(outcome2.resumed_split_txid.is_none());
    assert_eq!(rpc.broadcast_count(), 1, "no re-broadcast of a done split");
}

// =======================================================================
// Requirement B at the integration level: the 2026-08-30 incident pool
// shape (fragmented mature change) must select a valid combination
// within max_inputs = 25 instead of failing TooManyInputs.
// =======================================================================
#[tokio::test]
async fn test_b_incident_pool_shape_selects_within_max_inputs() {
    let (vault, _signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    // The incident's degraded pool: many ~2,425 GLC fragments.
    for i in 0..30u8 {
        seed_mature_root_utxo(&mut ledger, &mut view, &vault, i, 2_425 * GLC, 20);
    }
    configure_reserve(&mut ledger, 30 * 2_425 * GLC, 0);

    let (outcome, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 20_000, 100);
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        panic!("expected admission")
    };
    let inputs = ledger.get_goldcoin_payout_inputs(request_id).unwrap();
    assert!(
        inputs.len() >= 9 && inputs.len() <= 25,
        "the fragmented pool needs a multi-input combination within max_inputs, got {}",
        inputs.len()
    );
}

// =======================================================================
// Lifecycle requirement L: a Broadcast split evicted from the mempool is
// re-broadcast automatically from its exact stored bytes — mempool
// eviction can never permanently lose vault liquidity.
// =======================================================================
#[tokio::test]
async fn test_l_evicted_broadcast_split_is_rebroadcast_automatically() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 100_000 * GLC, 20);
    configure_reserve(&mut ledger, 100_000 * GLC, 0);

    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 10).await;
    let split_txid = outcome.new_split_txid.expect("shaping split must happen");
    assert_eq!(rpc.broadcast_count(), 1);
    let before = ledger.own_unconfirmed_change_atomic().unwrap();
    assert!(before > 0, "chunks tracked as known internal value");

    // Node restart wipes the mempool: the tx is gone AND the chunk
    // outputs stop being reported by listunspent.
    rpc.evict(split_txid);
    for i in 0..25u32 {
        view.entries.remove(&(split_txid, i));
    }
    view.sync(&mut ledger, 11);

    // The chunk rows must survive the missed snapshot (exempt from the
    // absence flip while their split is Broadcast) — the accounting term
    // keeps explaining the dip throughout the eviction window.
    assert_eq!(
        ledger.own_unconfirmed_change_atomic().unwrap(),
        before,
        "one missed snapshot must not erase known internal value"
    );
    let report = refresh_reconciliation(&mut ledger, 12);
    assert_ne!(report.classification, Classification::Breach, "{report:?}");

    // The next tick re-broadcasts the exact stored bytes.
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 13).await;
    assert_eq!(outcome.rebroadcast_split_txid, Some(split_txid));
    assert_eq!(rpc.broadcast_count(), 2);
    let first = rpc.submitted.lock().unwrap()[0].clone();
    let second = rpc.submitted.lock().unwrap()[1].clone();
    assert_eq!(first, second, "re-broadcast must be byte-identical");

    // Back in the mempool: chunks reappear at 0 conf, then mature, and
    // the split reaches Confirmed.
    rpc.restore(split_txid);
    view.absorb_split(&ledger, split_txid, &vault);
    view.mature_everything();
    view.sync(&mut ledger, 14);
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 15).await;
    assert!(outcome.confirmed_split_ids.len() == 1, "{outcome:?}");
    let report = refresh_reconciliation(&mut ledger, 16);
    assert_ne!(report.classification, Classification::Breach, "{report:?}");
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

// =======================================================================
// Lifecycle requirement M: a Broadcast split whose inputs are genuinely
// gone (conflicting spend won) is abandoned — loudly, with its phantom
// chunk rows cleared — and shaping continues on later ticks instead of
// wedging forever.
// =======================================================================
#[tokio::test]
async fn test_m_conflicted_broadcast_split_is_abandoned_and_shaping_continues() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 100_000 * GLC, 20);
    configure_reserve(&mut ledger, 100_000 * GLC, 0);

    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 10).await;
    let split_txid = outcome.new_split_txid.expect("shaping split must happen");

    // Eviction, and every re-broadcast now reports missing inputs: a
    // conflicting transaction spent the source.
    rpc.evict(split_txid);
    rpc.set_refuse_missing_inputs(true);
    for i in 0..25u32 {
        view.entries.remove(&(split_txid, i));
    }
    view.sync(&mut ledger, 11);

    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 12).await;
    let (abandoned_id, reason) = outcome.abandoned_split.expect("split must be abandoned");
    assert!(reason.contains("missing inputs"), "{reason}");

    // The phantom chunks no longer explain anything (they are Spent), and
    // nothing is pending or broadcast any more — the wedge is gone.
    assert_eq!(ledger.own_unconfirmed_change_atomic().unwrap(), 0);
    assert_eq!(ledger.unconfirmed_split_chunk_count().unwrap(), 0);
    assert!(ledger.pending_vault_utxo_splits().unwrap().is_empty());
    assert!(ledger.broadcast_vault_utxo_splits().unwrap().is_empty());
    let _ = abandoned_id;

    // Later ticks run normally (nothing left to split here — the source
    // is genuinely gone — but shaping is not stuck in an error loop).
    rpc.set_refuse_missing_inputs(false);
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 13).await;
    assert!(outcome.abandoned_split.is_none());
    assert!(outcome.skipped.is_some(), "{outcome:?}");
}

// =======================================================================
// Lifecycle requirement N: a claimed (Built) split whose source vanishes
// before signing is abandoned — and if the source legitimately returns
// (reorg restored it), the resurrection rule plus the partial unique
// index let it be split afresh. No permanent wedge, no manual SQLite.
// =======================================================================
#[tokio::test]
async fn test_n_built_split_with_vanished_source_is_abandoned_then_resplittable() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let source = seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 100_000 * GLC, 20);
    configure_reserve(&mut ledger, 100_000 * GLC, 0);

    // Claim exactly as a crash-before-signing would leave it.
    let plan = split::plan_split(&source, &vault, 5_000 * GLC, 100_000).unwrap();
    let unsigned_hex = glc_reserve_bridge_service::goldcoin::hex::encode(
        &split::build_unsigned_split_tx(&plan).serialize(),
    );
    ledger
        .record_vault_utxo_split_built(&plan, 5_000 * GLC, &unsigned_hex, "test", 1)
        .unwrap();

    // The source vanishes (spent externally / reorged away).
    view.entries.remove(&(source.txid, source.vout));
    view.sync(&mut ledger, 2);
    assert_eq!(
        ledger
            .get_vault_utxo(source.txid, source.vout)
            .unwrap()
            .unwrap()
            .state,
        "Spent"
    );

    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 3).await;
    let (_, reason) = outcome.abandoned_split.expect("claim must be abandoned");
    assert!(reason.contains("no longer Available"), "{reason}");
    assert_eq!(rpc.broadcast_count(), 0, "nothing was ever broadcast");

    // The source returns (reorg restored it): resurrection revives the
    // row — it was sync-inferred Spent, never spent by our own signed
    // transaction — and the released outpoint is claimed and split
    // afresh on the next tick.
    view.observe(source.clone(), 20);
    view.sync(&mut ledger, 4);
    assert_eq!(
        ledger
            .get_vault_utxo(source.txid, source.vout)
            .unwrap()
            .unwrap()
            .state,
        "Available",
        "a sync-inferred Spent row must resurrect when the chain reports it unspent again"
    );
    let outcome = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 5).await;
    assert!(outcome.new_split_txid.is_some(), "{outcome:?}");
}

// =======================================================================
// Lifecycle requirement O: the claim IS the payout/shaping exclusion
// boundary — from the instant a split row is Built, the source is
// invisible to payout selection and unreservable, so the two flows can
// never commit to the same UTXO.
// =======================================================================
#[tokio::test]
async fn test_o_claimed_split_source_is_invisible_and_unreservable_to_payouts() {
    let (vault, _signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let source = seed_mature_root_utxo(&mut ledger, &mut view, &vault, 1, 100_000 * GLC, 20);
    configure_reserve(&mut ledger, 100_000 * GLC, 0);
    assert_eq!(ledger.available_vault_utxos().unwrap().len(), 1);
    // A real admitted request, so the reservation attempts below exercise
    // the genuine FK-checked path.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            amounts_for_gross_glc(2_000),
            distinct_wallet(0),
            distinct_recipient(0).as_bytes(),
            0,
        )
        .unwrap()
    else {
        panic!("expected admission")
    };

    let plan = split::plan_split(&source, &vault, 5_000 * GLC, 100_000).unwrap();
    let unsigned_hex = glc_reserve_bridge_service::goldcoin::hex::encode(
        &split::build_unsigned_split_tx(&plan).serialize(),
    );
    ledger
        .record_vault_utxo_split_built(&plan, 5_000 * GLC, &unsigned_hex, "test", 1)
        .unwrap();

    // Invisible to selection...
    assert!(
        ledger.available_vault_utxos().unwrap().is_empty(),
        "a claimed source must not be offered to payout selection"
    );
    // ...and unreservable even for a payout that selected it against a
    // pre-claim snapshot (the reservation guard re-checks inside its own
    // write transaction).
    let err = ledger
        .reserve_vault_utxos(request_id, std::slice::from_ref(&source), 0, 2)
        .unwrap_err();
    assert!(matches!(err, LedgerError::VaultUtxoUnavailable { .. }));

    // Abandoning the claim releases the source back to both.
    let snapshot = ledger
        .get_vault_utxo_split(source.txid, source.vout)
        .unwrap()
        .unwrap();
    ledger
        .abandon_vault_utxo_split(snapshot.id, "test release", 3)
        .unwrap();
    assert_eq!(ledger.available_vault_utxos().unwrap().len(), 1);
    ledger
        .reserve_vault_utxos(request_id, std::slice::from_ref(&source), 0, 4)
        .unwrap();
}

// =======================================================================
// Requirement K (composition with the zero-conf payout-change policy, at
// the PRODUCTION depth-1 setting): a split's chunk outputs carry no
// payout-change provenance row, so they are never 0-conf eligible and
// never reservable while unconfirmed — shaping does not widen the 0-conf
// surface by a single outpoint. Payout change itself keeps its documented
// depth-1 eligibility (sanity-checked here so this test would catch the
// policy being accidentally disabled instead of vacuously passing).
// =======================================================================
#[tokio::test]
async fn test_k_split_chunks_are_never_zero_conf_eligible() {
    let (vault, signers) = vault_and_signers();
    let rpc = AcceptAllRpc::new();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    for i in 0..12u8 {
        seed_mature_root_utxo(&mut ledger, &mut view, &vault, i, 25_000 * GLC, 20);
    }
    let big = seed_mature_root_utxo(&mut ledger, &mut view, &vault, 99, 100_000 * GLC, 20);
    configure_reserve(&mut ledger, (12 * 25_000 + 100_000) * GLC, 0);
    let _ = big;

    // A real payout at the production depth-1 policy: its change IS
    // recorded with provenance and IS 0-conf eligible.
    let depth_one_policy = PayoutPolicy {
        zero_conf_change_max_depth: 1,
        ..policy()
    };
    let (outcome, _) = {
        let pre = refresh_reconciliation(&mut ledger, 90);
        assert_ne!(pre.classification, Classification::Breach);
        let outcome = ledger
            .fold_sol_deposit(
                0,
                amounts_for_gross_glc(20_000),
                distinct_wallet(0),
                distinct_recipient(0).as_bytes(),
                100,
            )
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
            panic!("expected admission")
        };
        let source = DevLedgerPayoutSource { ledger: &ledger };
        let plan = source
            .rederive_plan(request_id, &vault, &depth_one_policy, Network::Testnet)
            .unwrap();
        ledger
            .reserve_vault_utxos(request_id, &plan.inputs, 1, 100)
            .unwrap();
        ledger
            .record_goldcoin_payout_built(request_id, &plan, [0x77u8; 32], "00", 100)
            .unwrap();
        ledger
            .record_goldcoin_payout_signed(request_id, "00", 100)
            .unwrap();
        let mut txid = [0u8; 32];
        txid[0] = 0xF0;
        ledger
            .record_goldcoin_payout_broadcast(request_id, txid, 100)
            .unwrap();
        for input in &plan.inputs {
            view.entries.remove(&(input.txid, input.vout));
        }
        for (i, &amount_atomic) in plan.change_outputs.iter().enumerate() {
            view.observe(
                VaultUtxo {
                    txid,
                    vout: (i + 1) as u32,
                    amount_atomic,
                    script_pubkey_hex: vault.script_pubkey_hex(),
                },
                0,
            );
        }
        view.sync(&mut ledger, 100);
        (outcome, txid)
    };
    let _ = outcome;
    let eligible_change = ledger.zero_conf_change_vault_utxos(1).unwrap();
    assert!(
        !eligible_change.is_empty(),
        "sanity: real payout change must be 0-conf eligible at depth 1"
    );

    // A shaping split broadcasts; its chunks are Unconfirmed, carry no
    // provenance row, and must never appear in the 0-conf pool.
    let tick = shaping_tick(&mut ledger, &mut view, &vault, &signers, &rpc, 101).await;
    let split_txid = tick.new_split_txid.expect("shaping split must happen");
    let eligible_after = ledger.zero_conf_change_vault_utxos(1).unwrap();
    assert!(
        eligible_after.iter().all(|u| u.txid != split_txid),
        "split chunks must never be 0-conf eligible at any depth"
    );
    // Nor reservable while unconfirmed, even at a deliberately huge cap.
    let chunk = glc_reserve_bridge_service::goldcoin::coin::VaultUtxo {
        txid: split_txid,
        vout: 0,
        amount_atomic: ledger
            .get_vault_utxo(split_txid, 0)
            .unwrap()
            .unwrap()
            .amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    let err = ledger
        .reserve_vault_utxos(999, std::slice::from_ref(&chunk), 10, 102)
        .unwrap_err();
    assert!(matches!(err, LedgerError::VaultUtxoUnavailable { .. }));
}
