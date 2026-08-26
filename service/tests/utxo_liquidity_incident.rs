//! Regression/load tests reproducing the real production incident this
//! branch fixes permanently: normal Solana->Goldcoin traffic repeatedly
//! draining the vault's mature UTXO pool faster than 6-confirmation
//! maturity could replenish it — even after the vault had already been
//! manually pre-split into ~20 chunks once. See docs/09-runbook.md's
//! "UTXO liquidity" section for the full incident narrative and the fix's
//! design (deterministic change fan-out, UTXO pool health accounting,
//! admission backpressure).
//!
//! These tests drive the real production code paths (`Ledger::
//! fold_sol_deposit`, `signing::goldcoin_vault::rederive_plan`,
//! `goldcoin::coin::select`/`finalize_fanout`, `Ledger::
//! record_goldcoin_payout_built`/`record_goldcoin_payout_broadcast`,
//! `reconciliation::reconcile`) — never a hand-rolled simulation of them.
//! Real multisig signing is skipped (irrelevant to UTXO-pool/admission
//! mechanics, and already exhaustively covered elsewhere —
//! `signing::goldcoin_vault`'s own test suite, `goldcoin_payout_lifecycle
//! .rs`); a payout is driven directly from `rederive_plan`'s real output
//! through to `record_goldcoin_payout_broadcast` with a synthetic
//! signature/txid.
//!
//! `ChainView` stands in for a real Goldcoin node's `listunspent`: every
//! `sync_vault_utxos` call in this crate's production code passes the
//! COMPLETE current set of observed UTXOs each time (a full scan
//! snapshot) — anything previously synced but absent from a later call is
//! treated as spent. `ChainView` keeps that full running snapshot so this
//! file's tests replicate that contract exactly, rather than accidentally
//! marking everything else spent by passing only an incremental update.

use std::collections::BTreeMap;

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::payout::PayoutPolicy;
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{Ledger, RequestState, ReserveDirection, SolFoldOutcome};
use glc_reserve_bridge_service::reconciliation::{self, Classification};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    DevLedgerPayoutSource, DevVaultSigner, IndependentPayoutSource,
};

const DEST_ADDR: &str = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
const GLC: u64 = 100_000_000; // 1 GLC in canonical atomic units (8 decimals)
const MIN_CONFIRMATIONS: i64 = 6;

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

/// Production-shaped policy: real fan-out target/cap, real fee rate/dust
/// threshold — not a toy configuration.
fn policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * GLC,
        change_fanout_max_outputs: 10,
    }
}

/// `gross` GLC canonical -> the real gross/fee/net breakdown at the
/// production 6% fee rate (docs/20-bridge-fee.md) — `RequestAmounts` for
/// `fold_sol_deposit`, exactly as `solana::indexer::tick` would build it
/// for a real on-chain SolToGlc obligation of this size.
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

/// The production reserve shape from the incident: a configurable total
/// balance, 20,000 GLC protected minimum, and a configurable
/// UTXO-liquidity backpressure floor (production default: 8 mature UTXOs
/// must remain — see `default_utxo_pool_min_available_count` in
/// `service/src/config.rs`).
fn configure_incident_reserve(
    ledger: &mut Ledger,
    total_balance_glc: u64,
    utxo_pool_min_available_count: u32,
) {
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
            utxo_pool_min_available_count,
            utxo_pool_min_available_count + 5,
        )
        .unwrap();
}

/// A full running snapshot of every vault UTXO this test module knows
/// about, keyed by outpoint — see module docs for why a full snapshot
/// (not an incremental update) must be passed to `sync_vault_utxos` every
/// time, exactly like a real `listunspent`-backed `tick_vault_utxos`
/// would.
struct ChainView {
    entries: BTreeMap<([u8; 32], u32), (VaultUtxo, i64)>, // outpoint -> (utxo, confirmations)
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

    /// Full-snapshot sync — the only correct way to call
    /// `Ledger::sync_vault_utxos`.
    fn sync(&self, ledger: &mut Ledger, vault: &MultisigVault, now: i64) {
        let observed: Vec<_> = self
            .entries
            .values()
            .map(|(utxo, conf)| (utxo.clone(), *conf, vault.script_pubkey_hex()))
            .collect();
        ledger
            .sync_vault_utxos(&observed, MIN_CONFIRMATIONS, now)
            .unwrap();
    }
}

/// Seeds `count` mature UTXOs of `amount_glc` GLC each (matching the
/// incident's real "20 x ~4,770 GLC" shape), all paying the vault's own
/// script, and syncs them into the ledger as `Available`. Returns the
/// exact total seeded (`count * amount_glc` GLC) so callers can configure
/// `total_reserve_balance` to match physical reality exactly — this file
/// is about UTXO-pool/admission mechanics, not about a pre-existing,
/// unrelated accounting-vs-physical gap, so every test starts from a
/// perfectly consistent state.
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

/// Refreshes `total_reserve_balance` against the LIVE mature balance via
/// the real reconciliation pass, exactly like `Orchestrator::tick`'s own
/// EARLY reconciliation call (before `solana_indexer.tick()` admits
/// anything — the admission-freshness fix) does every real tick. Without
/// this, admission decisions would be made against a stale cached balance
/// that never reflects UTXOs actually being consumed — not a
/// reconciliation bug, just this test replicating the real tick order.
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

/// Folds SolToGlc obligation `i` (`gross_glc` GLC gross) and, if admitted,
/// drives it through the REAL `rederive_plan` -> coin-selection -> fan-out
/// -> ledger-persistence path all the way to `Broadcast`, then registers
/// its real change output(s) as freshly observed but still `Unconfirmed`
/// (1 confirmation, below the 6 required) — exactly what a live chain
/// scan would show the instant such a payout actually broadcasts. Real
/// multisig signing is skipped (see module docs); the persisted
/// `signed_tx_hex`/txid are synthetic placeholders. Returns the fold
/// outcome and, if a payout was built, its broadcast txid.
fn admit_and_broadcast_one(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    obligation_index: u64,
    gross_glc: u64,
    now: i64,
) -> (SolFoldOutcome, Option<[u8; 32]>) {
    let pre_admission = refresh_reconciliation(ledger, now);
    assert_ne!(
        pre_admission.classification,
        Classification::Breach,
        "obligation {obligation_index}: pre-admission reconciliation must never itself find a \
         breach in this file's scenarios: {pre_admission:?}"
    );
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
        .rederive_plan(request_id, vault, &policy(), Network::Testnet)
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

    // The payout's own change output(s), freshly observed on-chain but not
    // yet mature — `own_unconfirmed_change_atomic` recognizes these by
    // `vault_utxos.txid` matching this exact broadcast txid. The consumed
    // input(s) are no longer part of this view at all (a real node's
    // `listunspent` would no longer return a spent outpoint either).
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
            1, // immature: below MIN_CONFIRMATIONS
        );
    }
    view.sync(ledger, vault, now);
    (outcome, Some(txid))
}

/// A request that reached `DestinationSubmitted` (or further) was
/// genuinely admitted and successfully paid out — `admit_and_broadcast_one`
/// drives a real broadcast, not just admission, so this is the "finalized"
/// outcome throughout this file.
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

/// Test A: a burst of 25 consecutive 2,000-GLC-gross obligations, arriving
/// before any payout's own change reaches maturity, must not be
/// misclassified as unexplained missing reserve — the service either
/// keeps admitting from genuinely sufficient mature liquidity, or
/// intentionally parks (never drops) the obligations that would cross the
/// UTXO-liquidity floor, BEFORE the pool actually runs dry.
#[tokio::test]
async fn test_a_burst_of_25_obligations_never_misclassifies_reserve_as_unexplained_zero() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 20, 4_770);
    // Tuned to this vault's actual shape (4,770 GLC chunks, 1,880 GLC net
    // payouts, 20,000 GLC protected minimum): each payout leaves about
    // 2,890 GLC of its consumed chunk as immature change, so the hard
    // invariant's own slack (95,400 - 20,000 = 75,400 GLC) is exhausted
    // roughly every 6,650 GLC of throughput — surviving only 11 payouts on
    // its own (see the worked comment below). Floor 9 makes count-based
    // backpressure engage at 11 admissions, strictly before that hard
    // limit — exactly the operator tuning docs/09-runbook.md calls for:
    // the right floor depends on the vault's real chunk/payout sizes, not
    // a one-size-fits-all default.
    configure_incident_reserve(&mut ledger, total, 9);

    for i in 0..25u64 {
        admit_and_broadcast_one(&mut ledger, &mut view, &vault, i, 2_000, 100 + i as i64);
    }

    let (finalized, manual_review) = count_states(&ledger, 1..=25);
    assert_eq!(
        finalized + manual_review,
        25,
        "every obligation must land somewhere — never dropped"
    );
    assert!(
        finalized > 0,
        "genuinely available mature liquidity must still be used"
    );
    assert!(
        manual_review > 0,
        "backpressure must have engaged before the pool ran dry — otherwise this test's \
         starting shape (20 mature UTXOs, floor 9) isn't actually exercising it"
    );
    // Exactly 11 fit before the floor (20 mature - 9 floor = 11 consumable)
    // — pinned as a precise, worked-out expectation, not just "some". This
    // also stays strictly inside the hard invariant's own 11-payout
    // survival limit for this vault shape (see the setup comment above) —
    // backpressure catches it first, with a full payout of margin to
    // spare, never letting the hard invariant itself fire.
    assert_eq!(finalized, 11, "20 mature UTXOs, floor 9 => 11 consumable");
    assert_eq!(manual_review, 14);

    // The reserve must never have been auto-paused by this — backpressure
    // is a targeted, per-obligation park, not a direction-wide pause.
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());

    // Reconciliation against the real, live mature balance: the 12 spent
    // UTXOs and their still-immature change are known, internally-created
    // in-flight movement, not unexplained missing reserve.
    let observed_balance = ledger
        .available_vault_utxos()
        .unwrap()
        .iter()
        .map(|u| u.amount_atomic)
        .sum();
    let report = reconciliation::reconcile(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        observed_balance,
        0,
        200,
    )
    .unwrap();
    assert_ne!(
        report.classification,
        Classification::Breach,
        "known internal payout change must never be misclassified as an unexplained breach: {report:?}"
    );
    assert!(!report.auto_paused);
}

/// Test B: once a payout's change reaches 6 confirmations, it becomes a
/// normal `Available` candidate again, and admission automatically
/// recovers — no operator action anywhere.
#[tokio::test]
async fn test_b_matured_change_becomes_available_and_admission_recovers_automatically() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    // A tight pool: exactly floor+1 mature UTXOs, so the very next
    // admission after one payout is built immediately hits the floor.
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 9, 4_770);
    configure_incident_reserve(&mut ledger, total, 8);

    let (outcome0, txid0) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 2_000, 100);
    assert!(matches!(outcome0, SolFoldOutcome::FoldedFinalized { .. }));
    let available_count_after_one = ledger.available_vault_utxos().unwrap().len();
    assert_eq!(
        available_count_after_one, 8,
        "9 - 1 consumed = 8, at the floor"
    );

    let (outcome1, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 1, 2_000, 101);
    assert!(
        matches!(outcome1, SolFoldOutcome::FoldedManualReview { .. }),
        "at the floor: the next obligation must park, not consume the last buffer UTXO"
    );

    // The first payout's change now reaches 6 confirmations — a real
    // chain scan re-observes the exact same outpoint(s) with enough
    // confirmations, exactly like the next `tick_vault_utxos` would.
    let txid0 = txid0.expect("the first obligation must have built a real payout");
    view.bump_confirmations(txid0, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 200);

    let available_count_after_maturity = ledger.available_vault_utxos().unwrap().len();
    assert!(
        available_count_after_maturity > 8,
        "the matured change output(s) must count as available again: {available_count_after_maturity}"
    );

    // No operator action of any kind — the very next obligation is
    // admitted automatically now that liquidity has recovered.
    let (outcome2, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 2, 2_000, 300);
    assert!(
        matches!(outcome2, SolFoldOutcome::FoldedFinalized { .. }),
        "admission must recover automatically once real liquidity matures"
    );
}

/// Test C: repeated traffic through several maturity cycles never
/// produces a permanent pause, never an unexplained reserve drop, and
/// conserves value exactly throughout.
#[tokio::test]
async fn test_c_several_maturity_cycles_never_pause_never_unexplained_drop_exact_conservation() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 20, 4_770);
    // See Test A's setup comment for why floor 9 (not the bare production
    // default of 8) is the operator-appropriate tuning for this specific
    // vault shape.
    configure_incident_reserve(&mut ledger, total, 9);

    let mut obligation_index = 0u64;
    let mut now = 1000i64;
    for cycle in 0..4 {
        let mut this_cycle_txids = Vec::new();
        let mut this_cycle_request_ids = Vec::new();
        for _ in 0..5 {
            let (outcome, txid) = admit_and_broadcast_one(
                &mut ledger,
                &mut view,
                &vault,
                obligation_index,
                2_000,
                now,
            );
            this_cycle_txids.extend(txid);
            if let SolFoldOutcome::FoldedFinalized { request_id } = outcome {
                this_cycle_request_ids.push(request_id);
            }
            obligation_index += 1;
            now += 1;
        }

        // Reconcile mid-cycle: never a breach, never an auto-pause.
        let observed_balance = ledger
            .available_vault_utxos()
            .unwrap()
            .iter()
            .map(|u| u.amount_atomic)
            .sum();
        let report = reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::GoldcoinReserve,
            observed_balance,
            0,
            now,
        )
        .unwrap();
        assert_ne!(
            report.classification,
            Classification::Breach,
            "cycle {cycle}: {report:?}"
        );
        assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());

        // This cycle's own change matures fully before the next cycle
        // starts — real traffic resuming once liquidity is genuinely
        // back, exactly like Test B.
        for txid in this_cycle_txids {
            view.bump_confirmations(txid, MIN_CONFIRMATIONS);
        }
        view.sync(&mut ledger, &vault, now);
        now += 100;

        // Each cycle's payouts also reach real, full settlement before the
        // next cycle starts (a real bridge does not leave requests
        // permanently `Broadcast` — completion on Solana follows once the
        // Goldcoin payout itself confirms) — so `pending_obligations`
        // correctly releases, exactly like sustained real production
        // traffic, rather than growing without bound across cycles.
        for request_id in this_cycle_request_ids {
            ledger
                .update_goldcoin_payout_confirmations(request_id, 6, 6, 6, now)
                .unwrap();
            ledger
                .record_goldcoin_completion_submitted(request_id, [0x77u8; 64], now)
                .unwrap();
            ledger
                .mark_goldcoin_completion_confirmed(request_id, now)
                .unwrap();
        }
    }

    let (finalized_total, parked_total) = count_states(&ledger, 1..=obligation_index as i64);
    assert_eq!(finalized_total + parked_total, obligation_index as u32);
    assert!(
        !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "no permanent pause across repeated maturity cycles"
    );

    // Exact conservation: the invariant holds throughout.
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let final_available: u64 = ledger
        .available_vault_utxos()
        .unwrap()
        .iter()
        .map(|u| u.amount_atomic)
        .sum();
    assert!(
        final_available > 0,
        "liquidity must have recovered by the end of the run, not collapsed to zero"
    );
}

/// Test D: randomized payout sizes (100-2,000 GLC gross, a fixed seed for
/// reproducibility — determinism matters throughout this codebase),
/// staying within the 100,000 GLC daily gross limit, prove no duplicate
/// input use, exact fees/change, and the protected floor always held.
#[tokio::test]
async fn test_d_randomized_payout_sizes_no_double_spend_exact_accounting_floor_preserved() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 20, 4_770);
    // See Test A's setup comment for why floor 9 (not the bare production
    // default of 8) is the operator-appropriate tuning for this specific
    // vault shape.
    configure_incident_reserve(&mut ledger, total, 9);

    // Fixed xorshift PRNG — no external dependency, fully reproducible
    // across runs, matching this codebase's own determinism discipline
    // (`goldcoin::coin::select`'s own docs).
    let mut state: u64 = 0x2545F4914F6CDD1D;
    let mut next_u64 = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut daily_gross_glc: u64 = 0;
    let mut used_outpoints = std::collections::HashSet::new();
    let mut attempted = 0u64;
    // `now` is a simulated timestamp advancing alongside `i`, not a second
    // loop counter.
    #[allow(clippy::explicit_counter_loop)]
    for i in 0..60u64 {
        let now = 1000i64 + i as i64;
        let gross_glc = 100 + (next_u64() % 1901); // [100, 2000]
        if daily_gross_glc + gross_glc > 100_000 {
            break;
        }
        attempted += 1;
        let (outcome, _) =
            admit_and_broadcast_one(&mut ledger, &mut view, &vault, i, gross_glc, now);
        if let SolFoldOutcome::FoldedFinalized { request_id } = outcome {
            let plan_inputs = ledger.get_goldcoin_payout_inputs(request_id).unwrap();
            for input in &plan_inputs {
                let key = (input.txid, input.vout);
                assert!(
                    used_outpoints.insert(key),
                    "outpoint {key:?} was selected twice across the run"
                );
            }
            let payout = ledger
                .get_goldcoin_payout_full(request_id)
                .unwrap()
                .unwrap();
            let total_in: u64 = plan_inputs.iter().map(|u| u.amount_atomic).sum();
            assert_eq!(
                total_in,
                payout.payout_atomic + payout.change_atomic + payout.fee_atomic
            );
            daily_gross_glc += gross_glc;

            // The protected floor must hold after every single admission.
            let (balance, protected_minimum, reserved, _pending) = ledger
                .reserve_snapshot(ReserveDirection::GoldcoinReserve)
                .unwrap();
            assert!(
                balance as i64 - protected_minimum as i64 - reserved as i64 >= 0,
                "protected minimum breached after obligation {i}"
            );
        }
    }

    assert!(
        attempted > 0,
        "the run must have actually attempted obligations"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

/// Test E: two independent signers re-deriving the SAME multi-change-output
/// payout must build byte-identical transactions — determinism survives
/// fan-out exactly as it did for single-output change.
#[tokio::test]
async fn test_e_two_signers_independently_rederive_a_byte_identical_multi_change_transaction() {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    // A single large mature UTXO, forcing real fan-out change (leftover
    // well above the 2,500 GLC target -> multiple change outputs).
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 1, 40_000);
    // Backpressure disabled (0): this test is specifically about signing
    // determinism over a fan-out plan, not admission backpressure — a
    // small, deliberately single-UTXO pool is the point.
    configure_incident_reserve(&mut ledger, total, 0);

    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(
            0,
            amounts_for_gross_glc(2_000),
            [7u8; 32],
            DEST_ADDR.as_bytes(),
            100,
        )
        .unwrap()
    else {
        panic!("expected admission to succeed against a freshly seeded reserve")
    };

    let source_a = DevLedgerPayoutSource { ledger: &ledger };
    let plan_a = source_a
        .rederive_plan(request_id, &vault, &policy(), Network::Testnet)
        .unwrap();
    let source_b = DevLedgerPayoutSource { ledger: &ledger };
    let plan_b = source_b
        .rederive_plan(request_id, &vault, &policy(), Network::Testnet)
        .unwrap();

    assert!(
        plan_a.change_outputs.len() > 1,
        "this scenario must genuinely exercise fan-out, not just the single-output case: {:?}",
        plan_a.change_outputs
    );
    assert_eq!(
        plan_a, plan_b,
        "two independent re-derivations from the same ledger state must be byte-identical"
    );
    let tx_a = glc_reserve_bridge_service::goldcoin::payout::build_unsigned_tx(&plan_a);
    let tx_b = glc_reserve_bridge_service::goldcoin::payout::build_unsigned_tx(&plan_b);
    assert_eq!(tx_a.serialize(), tx_b.serialize());
}

/// Test F: restart the "daemon" (reopen the ledger from the same file)
/// while several fan-out change outputs are still `Unconfirmed`, and
/// confirm every relevant figure reconstructs correctly from the ledger
/// alone (a real daemon reconciles the rest against live Goldcoin RPC on
/// its next tick, which this test's `vault_utxos` rows already stand in
/// for).
#[tokio::test]
async fn test_f_restart_with_unconfirmed_fanout_change_reconstructs_state_correctly() {
    let vault = test_vault();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let mut view = ChainView::new();
    let request_id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        // Two equal-sized UTXOs: coin selection consumes exactly one
        // (forcing real fan-out change from the leftover), leaving the
        // other fully mature and untouched — enough that the hard
        // invariant genuinely holds afterward, since this test is about
        // restart recovery over a fan-out payout, not admission
        // backpressure (disabled: floor 0).
        let total = seed_mature_utxos(&mut ledger, &mut view, &vault, 2, 25_000);
        configure_incident_reserve(&mut ledger, total, 0);
        let (outcome, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 2_000, 100);
        let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
            panic!()
        };
        request_id
        // Crash here: payout broadcast, change observed but still
        // Unconfirmed, process exits.
    };

    let ledger = Ledger::open(&db_path).unwrap();
    let payout = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
    assert!(
        payout.change_outputs.len() > 1,
        "this scenario must genuinely exercise fan-out: {:?}",
        payout.change_outputs
    );
    assert_eq!(
        payout.change_atomic,
        payout.change_outputs.iter().sum::<u64>()
    );

    // The change outputs are correctly reconstructed as still-immature —
    // not silently lost, not silently counted as spendable. Exactly the
    // untouched twin UTXO remains; the consumed one's change is not here.
    let available = ledger.available_vault_utxos().unwrap();
    assert_eq!(
        available.len(),
        1,
        "the consumed UTXO's change hasn't matured yet; only the untouched twin remains: {available:?}"
    );
    let observed_balance: u64 = available.iter().map(|u| u.amount_atomic).sum();
    drop(ledger);

    // Reconciliation against this restarted ledger must still correctly
    // explain the drop as known internal change, not a breach.
    let mut ledger = Ledger::open(&db_path).unwrap();
    let report = reconciliation::reconcile(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        observed_balance,
        0,
        200,
    )
    .unwrap();
    assert_ne!(report.classification, Classification::Breach, "{report:?}");
}
