//! Diagnostic verification of the UTXO-liquidity fix
//! (`service/tests/utxo_liquidity_incident.rs`) against the LITERAL
//! production-config assumptions, requested explicitly rather than the
//! deliberately-retuned values `utxo_liquidity_incident.rs` uses:
//!
//! - `vault_min_confirmations = 6` (NOT the pilot-template's `20` —
//!   deliberately not used here per instruction).
//! - `utxo_pool_min_available_count = 8`, `utxo_pool_warning_count = 15`
//!   (`config.rs`'s literal shipped defaults).
//! - `change_fanout_target_atomic = 250_000_000_000` (2,500 GLC — same
//!   number `utxo_liquidity_incident.rs` already used, just spelled out in
//!   atomic units here for exactness).
//! - `change_fanout_max_outputs = 10`.
//! - `fee_rate_per_kb = 100_000` — the pilot template's real value
//!   (`service/config.pilot-template.toml`), not the `1000` used in
//!   `utxo_liquidity_incident.rs` for readability.
//!
//! `utxo_liquidity_incident.rs`'s tests A-D now use `utxo_pool_min_available_count
//! = 10` — the final recommended production tuning for the 20x4,770 GLC /
//! 20,000 GLC protected-minimum vault shape — but running that shape at
//! the literal shipped default of `8` reproduces the ORIGINAL incident
//! (the hard invariant breaches before count-based backpressure ever
//! engages), which is exactly what
//! `test_prod_defaults_floor_8_breaches_before_backpressure_engages`
//! below demonstrates and pins down precisely, as a documented, expected
//! (not silently ignored) finding. `test_prod_recommended_floor_10_survives_the_25_burst_with_margin`
//! validates the final `10` recommendation itself, under the real
//! production fee rate.

use std::collections::BTreeMap;

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::{self, VaultUtxo};
use glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, RequestState, ReserveDirection, SolFoldOutcome};
use glc_reserve_bridge_service::reconciliation::{self, Classification};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    DevLedgerPayoutSource, DevVaultSigner, IndependentPayoutSource,
};

const DEST_ADDR: &str = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
const GLC: u64 = 100_000_000; // 1 GLC in canonical atomic units (8 decimals)

// --- literal production assumptions, per instruction --------------------
const PROD_VAULT_MIN_CONFIRMATIONS: i64 = 6;
const PROD_UTXO_POOL_MIN_AVAILABLE_COUNT: u32 = 8;
const PROD_UTXO_POOL_WARNING_COUNT: u32 = 15;
const PROD_CHANGE_FANOUT_TARGET_ATOMIC: u64 = 250_000_000_000; // 2,500 GLC
const PROD_CHANGE_FANOUT_MAX_OUTPUTS: usize = 10;
const PROD_FEE_RATE_PER_KB: u64 = 100_000; // service/config.pilot-template.toml
const PROD_DUST_THRESHOLD: u64 = 1000; // service/config.pilot-template.toml
const PROD_MAX_INPUTS: usize = 10; // service/config.pilot-template.toml

fn prod_policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: PROD_FEE_RATE_PER_KB,
        dust_threshold: PROD_DUST_THRESHOLD,
        max_inputs: PROD_MAX_INPUTS,
        change_fanout_target_atomic: PROD_CHANGE_FANOUT_TARGET_ATOMIC,
        change_fanout_max_outputs: PROD_CHANGE_FANOUT_MAX_OUTPUTS,
    }
}

fn test_vault() -> MultisigVault {
    let signers = [
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap()
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

fn configure_prod_reserve(ledger: &mut Ledger, total_balance_glc: u64, min_available_count: u32) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            total_balance_glc * GLC,
            20_000 * GLC,
            100_000 * GLC,
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
            min_available_count,
            PROD_UTXO_POOL_WARNING_COUNT,
        )
        .unwrap();
}

/// See `utxo_liquidity_incident.rs`'s module docs for why a full running
/// snapshot (never an incremental update) must be passed to
/// `Ledger::sync_vault_utxos` every time.
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

    fn sync(&self, ledger: &mut Ledger, vault: &MultisigVault, now: i64) {
        let observed: Vec<_> = self
            .entries
            .values()
            .map(|(utxo, conf)| (utxo.clone(), *conf, vault.script_pubkey_hex()))
            .collect();
        ledger
            .sync_vault_utxos(&observed, PROD_VAULT_MIN_CONFIRMATIONS, now)
            .unwrap();
    }
}

fn seed_mature_utxos(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    count: u8,
    amount_glc: u64,
) -> u64 {
    for i in 0..count {
        let mut txid = [0u8; 32];
        txid[0] = 0xE0;
        txid[1] = i;
        view.observe(
            VaultUtxo {
                txid,
                vout: 0,
                amount_atomic: amount_glc * GLC,
                script_pubkey_hex: vault.script_pubkey_hex(),
            },
            20,
        );
    }
    view.sync(ledger, vault, 0);
    count as u64 * amount_glc
}

/// Reconciles WITHOUT panicking on breach (unlike
/// `utxo_liquidity_incident.rs`'s `refresh_reconciliation`) — this file's
/// whole point is to observe, name, and pin down exactly when/why a breach
/// happens under the literal production default, not to treat it as a
/// test-harness bug.
fn reconcile_now(ledger: &mut Ledger, now: i64) -> reconciliation::ReconciliationReport {
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

fn admit_and_broadcast_one(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    obligation_index: u64,
    gross_glc: u64,
    now: i64,
) -> (SolFoldOutcome, Option<[u8; 32]>) {
    let outcome = ledger
        .fold_sol_deposit(
            obligation_index,
            amounts_for_gross_glc(gross_glc),
            [7u8; 32],
            DEST_ADDR.as_bytes(),
            now,
        )
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        return (outcome, None);
    };

    let source = DevLedgerPayoutSource { ledger };
    let plan = source
        .rederive_plan(request_id, vault, &prod_policy(), Network::Testnet)
        .unwrap();
    ledger
        .reserve_vault_utxos(request_id, &plan.inputs, now)
        .unwrap();
    let commitment = [0x77u8; 32];
    ledger
        .record_goldcoin_payout_built(request_id, &plan, commitment, "00", now)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, "00", now)
        .unwrap();
    let mut txid = [0u8; 32];
    txid[0] = 0xF0;
    txid[24..32].copy_from_slice(&obligation_index.to_be_bytes());
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
            1, // immature: below PROD_VAULT_MIN_CONFIRMATIONS
        );
    }
    view.sync(ledger, vault, now);
    (outcome, Some(txid))
}

fn manual_review_reason(ledger: &Ledger, request_id: i64) -> Option<String> {
    ledger
        .get_request(request_id)
        .unwrap()
        .unwrap()
        .manual_review_note
}

/// Items 2/3 (mature-available-count-per-payout, exact backpressure
/// engagement point) AND the answer to "should 8/15 change": run the
/// EXACT incident vault shape (20 x 4,770 GLC = 95,400 GLC, 20,000 GLC
/// protected minimum, 2,000 GLC gross / 1,880 GLC net payouts) at the
/// LITERAL shipped default `utxo_pool_min_available_count = 8` — not the
/// `9` `utxo_liquidity_incident.rs` deliberately retunes to for this exact
/// shape.
///
/// Worked out precisely beforehand (docs/09-runbook.md's tuning
/// worked-example, restated here): each payout removes one whole 4,770
/// GLC chunk from the mature pool and adds 1,880 GLC to
/// `pending_obligations`, so the hard invariant's own slack (95,400 -
/// 20,000 = 75,400 GLC) is exhausted at `floor(75,400 / 6,650) = 11`
/// payouts fully admitted — the 12th admission (index 11, 0-based) is the
/// last one the hard invariant can survive; reconciling AFTER it (i.e.
/// before attempting index 12) observes a mature balance that already
/// breaches. Count-based backpressure at floor 8 only blocks starting at
/// index 12 (`available_utxo_count` drops to 8, which is not `> 8`) — one
/// index too late to ever fire on its own: the hard-invariant breach is
/// detected at the SAME index, and because reconciliation runs first each
/// tick, the direction is auto-paused before `fold_sol_deposit` for index
/// 12 ever evaluates the count-based check. This test proves that exact
/// sequence, not just asserts a generic "something went wrong".
#[tokio::test]
async fn test_prod_defaults_floor_8_breaches_before_backpressure_engages() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 20, 4_770);
    configure_prod_reserve(&mut ledger, total, PROD_UTXO_POOL_MIN_AVAILABLE_COUNT);

    println!(
        "\n=== production-default (floor={PROD_UTXO_POOL_MIN_AVAILABLE_COUNT}) 25-obligation burst diagnostics ==="
    );
    println!(
        "{:>3} {:>26} {:>26} {:>14} {:>12} {:>10} reason",
        "i",
        "mature_available_before",
        "mature_atomic_before",
        "classification",
        "auto_paused",
        "outcome"
    );

    let mut first_breach_index: Option<u64> = None;
    let mut first_utxo_liquidity_reason_index: Option<u64> = None;
    let mut first_paused_reason_index: Option<u64> = None;
    let mut finalized = 0u32;
    let mut manual_review = 0u32;

    for i in 0..25u64 {
        let now = 100 + i as i64;
        let pool_before = ledger.utxo_pool_health().unwrap();
        let report = reconcile_now(&mut ledger, now);
        if report.classification == Classification::Breach && first_breach_index.is_none() {
            first_breach_index = Some(i);
        }

        let (outcome, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, i, 2_000, now);
        let (outcome_str, reason) = match outcome {
            SolFoldOutcome::FoldedFinalized { .. } => {
                finalized += 1;
                ("Finalized".to_string(), None)
            }
            SolFoldOutcome::FoldedManualReview { request_id } => {
                manual_review += 1;
                let reason = manual_review_reason(&ledger, request_id);
                if reason.as_deref() == Some("utxo_liquidity_low_at_fold")
                    && first_utxo_liquidity_reason_index.is_none()
                {
                    first_utxo_liquidity_reason_index = Some(i);
                }
                if reason.as_deref() == Some("reserve_paused_at_fold")
                    && first_paused_reason_index.is_none()
                {
                    first_paused_reason_index = Some(i);
                }
                ("ManualReview".to_string(), reason)
            }
            SolFoldOutcome::AlreadyFolded { .. } => unreachable!(),
        };

        println!(
            "{i:>3} {:>26} {:>26} {:>14?} {:>12} {:>10} {}",
            pool_before.available_utxo_count,
            pool_before.mature_available_atomic / GLC,
            report.classification,
            report.auto_paused,
            outcome_str,
            reason.as_deref().unwrap_or("-"),
        );
    }

    println!(
        "=== summary: finalized={finalized} manual_review={manual_review} first_breach_index={first_breach_index:?} \
         first_utxo_liquidity_reason_index={first_utxo_liquidity_reason_index:?} \
         first_paused_reason_index={first_paused_reason_index:?} ==="
    );

    assert_eq!(finalized, 12, "12 chunks consumable before the hard invariant's own slack (75,400 GLC / 6,650 GLC-per-payout) runs out");
    assert_eq!(manual_review, 13);
    assert_eq!(
        first_breach_index,
        Some(12),
        "the hard invariant must breach at exactly obligation index 12 for this vault shape at floor=8"
    );
    assert_eq!(
        first_paused_reason_index,
        Some(12),
        "the FIRST parked obligation's reason must be reserve_paused_at_fold, not utxo_liquidity_low_at_fold"
    );
    assert_eq!(
        first_utxo_liquidity_reason_index, None,
        "at floor=8 for this vault shape, count-based backpressure never actually gets to fire as the \
         recorded reason — the hard-invariant auto-pause always wins the reason-priority race at the same index"
    );
    assert!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "the direction must end up paused — this is the hard invariant correctly failing safe, not data corruption"
    );

    // Even though a real breach occurred, verify it never corrupted
    // accounting or double-spent anything.
    let all_request_ids = 1..=25i64;
    let mut seen_outpoints = std::collections::HashSet::new();
    for id in all_request_ids {
        if let Ok(inputs) = ledger.get_goldcoin_payout_inputs(id) {
            for input in inputs {
                assert!(
                    seen_outpoints.insert((input.txid, input.vout)),
                    "outpoint reused across requests"
                );
            }
        }
    }
}

/// Item 4: exact auto-recovery point after change reaches 6 confirmations,
/// at the literal production floor (8) and warning (15) — using a
/// correctly-sized pool (9 UTXOs against a 20,000 GLC protected minimum,
/// same shape as `utxo_liquidity_incident.rs`'s own Test B) where floor=8
/// engages WITHOUT ever approaching the hard invariant, proving floor=8 is
/// not universally wrong — it is wrong specifically for the
/// higher-total-balance 20-UTXO incident shape in the test above, because
/// the accounting slack there is large enough to let 12 payouts through
/// before backpressure would matter.
#[tokio::test]
async fn test_prod_defaults_recovery_after_maturity_diagnostics() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 9, 4_770);
    configure_prod_reserve(&mut ledger, total, PROD_UTXO_POOL_MIN_AVAILABLE_COUNT);

    println!("\n=== production-default (floor=8) recovery-after-maturity diagnostics ===");

    let before = ledger.utxo_pool_health().unwrap();
    println!(
        "before any payout: available_utxo_count={}",
        before.available_utxo_count
    );
    let (outcome0, txid0) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 2_000, 100);
    assert!(matches!(outcome0, SolFoldOutcome::FoldedFinalized { .. }));
    let after0 = ledger.utxo_pool_health().unwrap();
    println!(
        "after obligation 0 (Finalized): available_utxo_count={} own_unconfirmed_change_atomic={} GLC",
        after0.available_utxo_count,
        after0.own_unconfirmed_change_atomic / GLC
    );
    assert_eq!(
        after0.available_utxo_count, 8,
        "9 - 1 consumed = 8, exactly at the floor"
    );
    reconcile_now(&mut ledger, 100);
    assert!(
        !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "this smaller pool must never approach the hard invariant"
    );

    let (outcome1, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 1, 2_000, 101);
    let reason1 = match outcome1 {
        SolFoldOutcome::FoldedManualReview { request_id } => {
            manual_review_reason(&ledger, request_id)
        }
        other => panic!("expected ManualReview at the floor, got {other:?}"),
    };
    println!("after obligation 1 (ManualReview, reason={reason1:?}): backpressure engaged AT the floor, exactly as designed");
    assert_eq!(reason1.as_deref(), Some("utxo_liquidity_low_at_fold"));

    let txid0 = txid0.unwrap();
    view.bump_confirmations(txid0, PROD_VAULT_MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 200);
    let after_maturity = ledger.utxo_pool_health().unwrap();
    println!(
        "after change reaches {PROD_VAULT_MIN_CONFIRMATIONS} confirmations: available_utxo_count={} \
         (was {})",
        after_maturity.available_utxo_count, after0.available_utxo_count
    );
    assert!(after_maturity.available_utxo_count > after0.available_utxo_count);

    let (outcome2, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 2, 2_000, 300);
    println!(
        "after maturity, obligation 2: {outcome2:?} — recovered automatically, no operator action"
    );
    assert!(matches!(outcome2, SolFoldOutcome::FoldedFinalized { .. }));
}

/// Final production recommendation: `utxo_pool_min_available_count = 10`
/// (raised from the `8` shown insufficient above, and from the `9`
/// initially considered — see docs/09-runbook.md's tuning section),
/// `utxo_pool_warning_count = 15`, `change_fanout_target_atomic =
/// 250_000_000_000`, `change_fanout_max_outputs = 10` — validated here
/// under the REAL production fee rate (`fee_rate_per_kb = 100_000`), not
/// just the toy `1000` used in `utxo_liquidity_incident.rs`'s own
/// already-updated Test A. Backpressure must engage with a full payout of
/// margin to spare before the hard invariant's own 11-payout survival
/// limit for this exact vault shape (see the breach test above), and the
/// hard invariant must never actually breach.
const PROD_RECOMMENDED_MIN_AVAILABLE_COUNT: u32 = 10;

#[tokio::test]
async fn test_prod_recommended_floor_10_survives_the_25_burst_with_margin() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 20, 4_770);
    configure_prod_reserve(&mut ledger, total, PROD_RECOMMENDED_MIN_AVAILABLE_COUNT);

    println!(
        "\n=== recommended production tuning (floor={PROD_RECOMMENDED_MIN_AVAILABLE_COUNT}, \
         real fee_rate_per_kb={PROD_FEE_RATE_PER_KB}) 25-obligation burst diagnostics ==="
    );

    let mut first_breach_index: Option<u64> = None;
    let mut finalized = 0u32;
    let mut manual_review = 0u32;
    for i in 0..25u64 {
        let now = 100 + i as i64;
        let pool_before = ledger.utxo_pool_health().unwrap();
        let report = reconcile_now(&mut ledger, now);
        if report.classification == Classification::Breach && first_breach_index.is_none() {
            first_breach_index = Some(i);
        }
        let (outcome, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, i, 2_000, now);
        let outcome_str = match outcome {
            SolFoldOutcome::FoldedFinalized { .. } => {
                finalized += 1;
                "Finalized"
            }
            SolFoldOutcome::FoldedManualReview { .. } => {
                manual_review += 1;
                "ManualReview"
            }
            SolFoldOutcome::AlreadyFolded { .. } => unreachable!(),
        };
        println!(
            "{i:>3} mature_available_before={:>2} classification={:?} -> {outcome_str}",
            pool_before.available_utxo_count, report.classification
        );
    }

    println!("=== summary: finalized={finalized} manual_review={manual_review} first_breach_index={first_breach_index:?} ===");

    assert_eq!(
        first_breach_index, None,
        "the hard invariant must never breach at the recommended floor of 10"
    );
    assert!(
        !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "the recommended production tuning must never trigger an auto-pause on this exact incident scenario"
    );
    // 20 mature - 10 floor = 10 consumable, one full payout of margin
    // inside the hard invariant's own 11-payout survival limit.
    assert_eq!(finalized, 10);
    assert_eq!(manual_review, 15);
}

/// Item 5: the exact change outputs `finalize_fanout` produces for a
/// 1,880 GLC net payout consuming a single 4,770.8999317 GLC input, at
/// production fee-rate/target/cap.
#[test]
fn test_change_outputs_for_a_typical_4770_glc_input() {
    let vault = test_vault();
    let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
    let input_amount_atomic: u64 = 477_089_993_170; // 4,770.8999317 GLC exactly
    let net_destination_atomic = amounts_for_gross_glc(2_000).net_destination_atomic; // 1,880 GLC

    let candidates = vec![VaultUtxo {
        txid: [1u8; 32],
        vout: 0,
        amount_atomic: input_amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    }];
    let result = coin::select(
        &candidates,
        net_destination_atomic,
        PROD_FEE_RATE_PER_KB,
        vault.threshold,
        vault.redeem_script().len(),
        PROD_MAX_INPUTS,
    )
    .unwrap();
    assert_eq!(
        result.selected.len(),
        1,
        "the single input must cover the payout on its own"
    );

    let (change_outputs, fee) = coin::finalize_fanout(
        &result,
        net_destination_atomic,
        result.selected.len(),
        input_bytes,
        PROD_FEE_RATE_PER_KB,
        PROD_DUST_THRESHOLD,
        PROD_CHANGE_FANOUT_TARGET_ATOMIC,
        PROD_CHANGE_FANOUT_MAX_OUTPUTS,
    );

    println!("\n=== 4,770.8999317 GLC input, 1,880 GLC net payout, production fee/target/cap ===");
    println!(
        "input_amount_atomic       = {input_amount_atomic} ({:.7} GLC)",
        input_amount_atomic as f64 / GLC as f64
    );
    println!(
        "net_destination_atomic    = {net_destination_atomic} ({:.7} GLC)",
        net_destination_atomic as f64 / GLC as f64
    );
    println!(
        "fee_atomic                = {fee} ({:.8} GLC)",
        fee as f64 / GLC as f64
    );
    println!("change_outputs (atomic)   = {change_outputs:?}");
    for (i, c) in change_outputs.iter().enumerate() {
        println!("  change[{i}] = {c} ({:.7} GLC)", *c as f64 / GLC as f64);
    }

    // Exact conservation.
    let total_out: u64 = net_destination_atomic + fee + change_outputs.iter().sum::<u64>();
    assert_eq!(total_out, input_amount_atomic);
    // Leftover (~2,890.9 GLC) is just above one 2,500 GLC target -> 2 change outputs.
    assert_eq!(change_outputs.len(), 2);
    for c in &change_outputs {
        assert!(*c >= PROD_DUST_THRESHOLD);
    }
}

/// Item 6: behavior for a very large ~97,000 GLC input/change — the
/// `change_fanout_max_outputs = 10` cap dominates over the 2,500 GLC
/// target once leftover value is large enough, producing 10 outputs each
/// well above the target size rather than an unbounded number of them.
#[test]
fn test_change_outputs_for_a_very_large_97000_glc_input() {
    let vault = test_vault();
    let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
    let input_amount_atomic: u64 = 97_000 * GLC;
    let net_destination_atomic = amounts_for_gross_glc(2_000).net_destination_atomic;

    let candidates = vec![VaultUtxo {
        txid: [2u8; 32],
        vout: 0,
        amount_atomic: input_amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    }];
    let result = coin::select(
        &candidates,
        net_destination_atomic,
        PROD_FEE_RATE_PER_KB,
        vault.threshold,
        vault.redeem_script().len(),
        PROD_MAX_INPUTS,
    )
    .unwrap();

    let (change_outputs, fee) = coin::finalize_fanout(
        &result,
        net_destination_atomic,
        result.selected.len(),
        input_bytes,
        PROD_FEE_RATE_PER_KB,
        PROD_DUST_THRESHOLD,
        PROD_CHANGE_FANOUT_TARGET_ATOMIC,
        PROD_CHANGE_FANOUT_MAX_OUTPUTS,
    );

    println!("\n=== ~97,000 GLC input, 1,880 GLC net payout, production fee/target/cap ===");
    println!(
        "input_amount_atomic     = {input_amount_atomic} ({} GLC)",
        input_amount_atomic / GLC
    );
    println!(
        "naive target-sized count would be leftover/target = {}",
        (input_amount_atomic - net_destination_atomic) / PROD_CHANGE_FANOUT_TARGET_ATOMIC
    );
    println!(
        "actual change_outputs.len() = {} (capped at change_fanout_max_outputs={})",
        change_outputs.len(),
        PROD_CHANGE_FANOUT_MAX_OUTPUTS
    );
    println!("fee_atomic = {fee}");
    for (i, c) in change_outputs.iter().enumerate() {
        println!("  change[{i}] = {c} ({:.4} GLC)", *c as f64 / GLC as f64);
    }
    let total_out: u64 = net_destination_atomic + fee + change_outputs.iter().sum::<u64>();
    assert_eq!(total_out, input_amount_atomic);

    assert_eq!(
        change_outputs.len(),
        PROD_CHANGE_FANOUT_MAX_OUTPUTS,
        "leftover (~95,120 GLC) / target (2,500 GLC) far exceeds max_outputs=10, so the cap must bind"
    );
    let smallest = *change_outputs.iter().min().unwrap();
    let largest = *change_outputs.iter().max().unwrap();
    println!(
        "per-output size range: {} - {} GLC (vs. 2,500 GLC target)",
        smallest / GLC,
        largest / GLC
    );
    assert!(
        smallest / GLC > 9_000,
        "capped fan-out on a large UTXO still produces outputs far above the 2,500 GLC target \
         (~9,512 GLC each here) — close to the original incident's own manually-split chunk size"
    );
}

/// Item 7: the real production fee, computed via the actual
/// `fee_for`/`multisig_input_bytes` production code at `fee_rate_per_kb =
/// 100_000` (service/config.pilot-template.toml), across a realistic
/// range of change-output counts, cross-checked against
/// `dust_threshold = 1000`.
#[test]
fn test_production_fee_calculation_at_the_real_fee_rate() {
    let vault = test_vault();
    let redeem_script_len = vault.redeem_script().len();
    let input_bytes = coin::multisig_input_bytes(vault.threshold, redeem_script_len);
    assert_eq!(
        redeem_script_len, 105,
        "2-of-3 compressed-pubkey redeem script is 105 bytes"
    );
    assert_eq!(
        input_bytes, 299,
        "matches the documented 2-of-3/105-byte worked example"
    );

    println!("\n=== production fee verification: fee_rate_per_kb={PROD_FEE_RATE_PER_KB}, input_bytes={input_bytes} ===");
    println!(
        "{:>14} {:>10} {:>14} {:>16}",
        "num_outputs", "fee_atomic", "fee_GLC", "fee_vs_dust_ratio"
    );
    for num_outputs in 2..=11usize {
        let fee = coin::fee_for(1, num_outputs, PROD_FEE_RATE_PER_KB, input_bytes);
        println!(
            "{num_outputs:>14} {fee:>10} {:>14.8} {:>16.2}",
            fee as f64 / GLC as f64,
            fee as f64 / PROD_DUST_THRESHOLD as f64
        );
        // The fee must always stay a small fraction of a GLC at production
        // scale, and must always exceed a handful of dust thresholds (so
        // it's a real, economically meaningful cost, not a rounding
        // artifact) without ever approaching the 2,500 GLC fan-out target.
        assert!(
            fee < GLC / 100,
            "fee for {num_outputs} outputs unexpectedly large: {fee}"
        );
        assert!(
            fee > PROD_DUST_THRESHOLD,
            "fee for {num_outputs} outputs unexpectedly tiny: {fee}"
        );
    }

    // A single-input, 2-change-output transaction (the typical case from
    // the 4,770 GLC worked example above).
    let typical_fee = coin::fee_for(1, 3, PROD_FEE_RATE_PER_KB, input_bytes);
    println!(
        "typical (1 in, 1 recipient + 2 change out) fee = {typical_fee} atomic ({:.8} GLC)",
        typical_fee as f64 / GLC as f64
    );
    assert_eq!(typical_fee, 41_100);
}

/// A final full-file sanity check that the file's diagnostics all agree on
/// the same request-state bookkeeping helper used throughout.
fn count_states(ledger: &Ledger, ids: std::ops::RangeInclusive<i64>) -> (u32, u32) {
    let mut finalized_or_further = 0u32;
    let mut manual_review = 0u32;
    for id in ids {
        match ledger.get_request(id).unwrap().unwrap().state {
            RequestState::SourceFinalized
            | RequestState::SettlementAuthorized
            | RequestState::DestinationSubmitted
            | RequestState::DestinationConfirmed
            | RequestState::Settled => finalized_or_further += 1,
            RequestState::ManualReview => manual_review += 1,
            other => panic!("request {id}: unexpected state {other:?}"),
        }
    }
    (finalized_or_further, manual_review)
}

#[tokio::test]
async fn test_state_bookkeeping_matches_the_diagnostic_counts() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 20, 4_770);
    configure_prod_reserve(&mut ledger, total, PROD_UTXO_POOL_MIN_AVAILABLE_COUNT);
    for i in 0..25u64 {
        reconcile_now(&mut ledger, 100 + i as i64);
        admit_and_broadcast_one(&mut ledger, &mut view, &vault, i, 2_000, 100 + i as i64);
    }
    let (finalized, manual_review) = count_states(&ledger, 1..=25);
    assert_eq!(finalized + manual_review, 25);
    assert_eq!(finalized, 12);
    assert_eq!(manual_review, 13);
}
