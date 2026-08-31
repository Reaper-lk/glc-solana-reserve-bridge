//! Regression tests for PR #35's maintainer-review finding 2: "manual
//! review resume must respect UTXO liquidity."
//!
//! `Ledger::resume_manual_review_sol_to_glc` used to check only the
//! VALUE-based reserve invariant before moving a parked `SolToGlc`
//! request back to `SourceFinalized` — never the count-based
//! `utxo_pool_min_available_count` admission gate `fold_sol_deposit`
//! applies to a brand-new obligation. An operator batch-resuming several
//! `utxo_liquidity_low_at_fold` requests (or simply retrying one) the
//! moment VALUE accounting looked sufficient — while the mature UTXO
//! COUNT was still at or below the floor — could re-admit exactly the
//! demand the backpressure mechanism exists to hold back, walking the
//! reserve right back toward the hard invariant it was designed to avoid
//! tripping.
//!
//! Fixed: `resume_manual_review_sol_to_glc` now re-runs the identical
//! count-based check before resuming anything, refusing (leaving the
//! request untouched, reserving nothing) with a dedicated
//! `LedgerError::UtxoLiquidityLow` while liquidity is still low, and
//! succeeding normally — no special-casing needed — the moment it
//! recovers.
//!
//! Tests A-E below correspond exactly to the five regression requirements
//! from the fix request:
//! - A: resume attempted while the pool is at the configured floor
//!   refuses safely.
//! - B: no duplicate obligation/payout is ever created across repeated
//!   refused resume attempts.
//! - C: the triggering payout's change matures to 6 confirmations and the
//!   pool recovers.
//! - D: resume succeeds normally after recovery, with no special-casing.
//! - E: the protected reserve invariant never breaches throughout.

use std::collections::BTreeMap;

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::goldcoin::payout::{PayoutPolicy, ZeroConfChangeMode};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{
    Ledger, LedgerError, RequestState, ReserveDirection, ResumeManualReviewOutcome, SolFoldOutcome,
};
use glc_reserve_bridge_service::reconciliation::{self, Classification};
use glc_reserve_bridge_service::signing::goldcoin_vault::{
    DevLedgerPayoutSource, DevVaultSigner, IndependentPayoutSource,
};

const DEST_ADDR: &str = "mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef";
/// A second, distinct, VALID Goldcoin testnet P2PKH address — the
/// liquidity-parked obligation in `setup_at_the_floor` must go to a
/// DIFFERENT recipient than obligation 0, or it would additionally (and
/// unintentionally) collide with the SolToGlc recipient rate limit's
/// rolling 24h window, which is not what this suite is about.
fn second_dest_addr() -> String {
    glc_reserve_bridge_service::goldcoin::address::encode_p2pkh(&[0x42u8; 20], Network::Testnet)
}
const GLC: u64 = 100_000_000;
const MIN_CONFIRMATIONS: i64 = 6;
const FLOOR: u32 = 10; // the final recommended production tuning

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

fn policy() -> PayoutPolicy {
    PayoutPolicy {
        fee_rate_per_kb: 1000,
        dust_threshold: 1000,
        max_inputs: 10,
        change_fanout_target_atomic: 2_500 * GLC,
        change_fanout_max_outputs: 10,
        zero_conf_change_max_depth: 0,
        zero_conf_change_mode: ZeroConfChangeMode::DepthLimited,
        zero_conf_change_recursive_chain_limit: 20,
    }
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

fn configure_reserve(ledger: &mut Ledger, total_balance_glc: u64, min_available_count: u32) {
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
            min_available_count + 5,
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
            .sync_vault_utxos(&observed, MIN_CONFIRMATIONS, now)
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

/// Admits and drives obligation `obligation_index` all the way to
/// `Broadcast`, real coin selection and fan-out included. Real multisig
/// signing is skipped (irrelevant here; exhaustively covered elsewhere) —
/// a synthetic signature/txid stands in.
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
            wallet_for(obligation_index),
            DEST_ADDR.as_bytes(),
            now,
        )
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        return (outcome, None);
    };
    broadcast_built_request(ledger, view, vault, request_id, obligation_index, now);
    (outcome, Some(txid_for(obligation_index)))
}

fn txid_for(obligation_index: u64) -> [u8; 32] {
    let mut txid = [0u8; 32];
    txid[0] = 0xF0;
    txid[24..32].copy_from_slice(&obligation_index.to_be_bytes());
    txid
}

/// A distinct source wallet per obligation index — this suite's tests
/// target the UTXO-liquidity mechanic specifically, never the (unrelated)
/// SolToGlc source-wallet rate limit, so obligations that must NOT collide
/// on that limit use this instead of a single fixed wallet.
fn wallet_for(obligation_index: u64) -> [u8; 32] {
    let mut wallet = [7u8; 32];
    wallet[24..32].copy_from_slice(&obligation_index.to_be_bytes());
    wallet
}

/// Drives an already-`SourceFinalized` request (whether freshly folded or
/// just resumed from `ManualReview`) through the real build/sign/broadcast
/// path exactly once.
fn broadcast_built_request(
    ledger: &mut Ledger,
    view: &mut ChainView,
    vault: &MultisigVault,
    request_id: i64,
    obligation_index: u64,
    now: i64,
) {
    let source = DevLedgerPayoutSource { ledger };
    let plan = source
        .rederive_plan(request_id, vault, &policy(), Network::Testnet)
        .unwrap();
    ledger
        .reserve_vault_utxos(request_id, &plan.inputs, 0, now)
        .unwrap();
    let commitment = [0x77u8; 32];
    ledger
        .record_goldcoin_payout_built(request_id, &plan, commitment, "00", now)
        .unwrap();
    ledger
        .record_goldcoin_payout_signed(request_id, "00", now)
        .unwrap();
    let txid = txid_for(obligation_index);
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
            1, // immature: below MIN_CONFIRMATIONS
        );
    }
    view.sync(ledger, vault, now);
}

/// Common setup shared by every test below: a pool of exactly `FLOOR + 1`
/// mature UTXOs, so ONE admission brings `available_utxo_count` exactly to
/// the floor — the very next obligation parks with
/// `utxo_liquidity_low_at_fold`, ready to exercise resume against.
fn setup_at_the_floor() -> (MultisigVault, Ledger, ChainView, i64) {
    let vault = test_vault();
    let mut ledger = Ledger::open_in_memory().unwrap();
    let mut view = ChainView::new();
    let total = seed_mature_utxos(&mut ledger, &mut view, &vault, FLOOR as u8 + 1, 4_770);
    configure_reserve(&mut ledger, total, FLOOR);

    let (outcome0, _) = admit_and_broadcast_one(&mut ledger, &mut view, &vault, 0, 2_000, 100);
    assert!(matches!(outcome0, SolFoldOutcome::FoldedFinalized { .. }));
    assert_eq!(
        ledger.available_vault_utxos().unwrap().len(),
        FLOOR as usize,
        "one admission must bring the pool exactly to the floor"
    );

    let outcome1 = ledger
        .fold_sol_deposit(
            1,
            amounts_for_gross_glc(2_000),
            wallet_for(1),
            second_dest_addr().as_bytes(),
            101,
        )
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome1 else {
        panic!("expected the second obligation to park at the floor, got {outcome1:?}")
    };
    let note = ledger
        .get_request(request_id)
        .unwrap()
        .unwrap()
        .manual_review_note;
    assert_eq!(
        note.as_deref(),
        Some("utxo_liquidity_low_at_fold"),
        "must park specifically for liquidity, not some other reason, or this test proves nothing"
    );

    (vault, ledger, view, request_id)
}

/// Test A: resuming while the pool is still at the floor refuses safely —
/// no override, no mutation.
#[tokio::test]
async fn test_a_resume_attempted_while_pool_is_at_the_floor_refuses_safely() {
    let (_vault, mut ledger, _view, request_id) = setup_at_the_floor();

    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "operator retry", "operator", 102)
        .unwrap_err();
    match err {
        LedgerError::UtxoLiquidityLow {
            request_id: rid,
            available_utxo_count,
            min_available_count,
        } => {
            assert_eq!(rid, request_id);
            assert_eq!(available_utxo_count, FLOOR as i64);
            assert_eq!(min_available_count, FLOOR as i64);
        }
        other => panic!("expected UtxoLiquidityLow, got {other:?}"),
    }

    // The request must remain exactly as it was — untouched.
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("utxo_liquidity_low_at_fold")
    );

    // Nothing was reserved: only obligation 0's reservation is counted.
    let (_, _, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap();
    let one_obligation = amounts_for_gross_glc(2_000).net_destination_atomic;
    assert_eq!(reserved, one_obligation);
    assert_eq!(pending, one_obligation);
}

/// Test B: no duplicate obligation or payout is ever created across
/// repeated refused resume attempts — fully idempotent.
#[tokio::test]
async fn test_b_no_duplicate_obligation_or_payout_is_created() {
    let (vault, mut ledger, mut view, request_id) = setup_at_the_floor();

    // Retry resume several times while still at the floor: every single
    // attempt must refuse identically, never partially mutating anything.
    for attempt in 0..5 {
        let err = ledger
            .resume_manual_review_sol_to_glc(
                request_id,
                "operator retry",
                "operator",
                102 + attempt,
            )
            .unwrap_err();
        assert!(matches!(err, LedgerError::UtxoLiquidityLow { .. }));
        assert!(
            ledger
                .get_goldcoin_payout_full(request_id)
                .unwrap()
                .is_none(),
            "attempt {attempt}: no payout row may exist while the request is still parked"
        );
    }

    // Re-folding the SAME obligation index must still be recognized as
    // already-folded — never a second request for the same obligation.
    let refold = ledger
        .fold_sol_deposit(
            1,
            amounts_for_gross_glc(2_000),
            [7u8; 32],
            DEST_ADDR.as_bytes(),
            200,
        )
        .unwrap();
    assert_eq!(
        refold,
        SolFoldOutcome::AlreadyFolded { request_id },
        "the exact same obligation index must never create a second request"
    );

    // Now let liquidity recover and actually resume + broadcast — even
    // after that, there must be exactly one payout for this request, ever.
    let txid0 = txid_for(0);
    view.bump_confirmations(txid0, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 300);
    ledger
        .resume_manual_review_sol_to_glc(request_id, "recovered", "operator", 301)
        .unwrap();
    broadcast_built_request(&mut ledger, &mut view, &vault, request_id, 1, 302);

    let payout = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .expect("exactly one payout must now exist");
    assert_eq!(payout.state, "Broadcast");

    // A second attempt to build a fresh payout for the same request must
    // be refused — the request has already moved past `SourceFinalized`
    // (real broadcast side effects advance it to `DestinationSubmitted`),
    // so `rederive_plan`'s own precondition refuses before a second
    // payout could ever even be constructed, proving the resume path
    // never opens a second door to the same request.
    let source = DevLedgerPayoutSource { ledger: &ledger };
    let duplicate = source.rederive_plan(request_id, &vault, &policy(), Network::Testnet);
    assert!(
        duplicate.is_err(),
        "a second payout attempt for the same, already-broadcast request must be refused"
    );
}

/// Test C: the triggering payout's change matures to 6 confirmations and
/// the mature pool recovers past the floor.
#[tokio::test]
async fn test_c_change_matures_and_pool_recovers() {
    let (vault, mut ledger, mut view, _request_id) = setup_at_the_floor();

    let before = ledger.available_vault_utxos().unwrap().len();
    assert_eq!(before, FLOOR as usize);

    let txid0 = txid_for(0);
    view.bump_confirmations(txid0, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 300);

    let after = ledger.available_vault_utxos().unwrap().len();
    assert!(
        after > FLOOR as usize,
        "matured change must count as available again: before={before} after={after}"
    );
}

/// Test D: resume succeeds normally after recovery — no special-casing —
/// and the resumed request completes its normal lifecycle.
#[tokio::test]
async fn test_d_resume_succeeds_after_recovery() {
    let (vault, mut ledger, mut view, request_id) = setup_at_the_floor();

    let txid0 = txid_for(0);
    view.bump_confirmations(txid0, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 300);

    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "recovered", "operator", 301)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert!(request.manual_review_note.is_none());

    // A second resume call on the now-already-resumed request is a safe,
    // reported no-op — never an error, never a double reservation.
    let again = ledger
        .resume_manual_review_sol_to_glc(request_id, "recovered again", "operator", 302)
        .unwrap();
    assert_eq!(
        again,
        ResumeManualReviewOutcome::AlreadyResumed {
            state: RequestState::SourceFinalized
        }
    );

    // The resumed request completes its normal lifecycle exactly like a
    // freshly-folded one.
    broadcast_built_request(&mut ledger, &mut view, &vault, request_id, 1, 303);
    let payout = ledger
        .get_goldcoin_payout_full(request_id)
        .unwrap()
        .unwrap();
    assert_eq!(payout.state, "Broadcast");
}

/// Test E: the protected reserve invariant never breaches throughout the
/// whole sequence — initial admission, the parked obligation, repeated
/// refused resume attempts, maturity, and the eventual successful resume.
#[tokio::test]
async fn test_e_protected_reserve_invariant_never_breaches() {
    let (vault, mut ledger, mut view, request_id) = setup_at_the_floor();

    let check = |ledger: &mut Ledger, now: i64| {
        let observed_balance: u64 = ledger
            .available_vault_utxos()
            .unwrap()
            .iter()
            .map(|u| u.amount_atomic)
            .sum();
        let report = reconciliation::reconcile(
            ledger,
            ReserveDirection::GoldcoinReserve,
            observed_balance,
            0,
            now,
        )
        .unwrap();
        assert_ne!(report.classification, Classification::Breach, "{report:?}");
        assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    };

    check(&mut ledger, 150);
    for attempt in 0..3 {
        let _ =
            ledger.resume_manual_review_sol_to_glc(request_id, "retry", "operator", 160 + attempt);
        check(&mut ledger, 160 + attempt);
    }

    let txid0 = txid_for(0);
    view.bump_confirmations(txid0, MIN_CONFIRMATIONS);
    view.sync(&mut ledger, &vault, 300);
    check(&mut ledger, 300);

    ledger
        .resume_manual_review_sol_to_glc(request_id, "recovered", "operator", 301)
        .unwrap();
    check(&mut ledger, 301);

    broadcast_built_request(&mut ledger, &mut view, &vault, request_id, 1, 302);
    check(&mut ledger, 302);

    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}
