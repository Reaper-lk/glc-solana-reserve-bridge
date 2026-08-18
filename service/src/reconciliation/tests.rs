use super::*;
use crate::ledger::{CreateRequestOutcome, Direction, Ledger};

fn setup() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
    ledger
}

#[test]
fn matching_balance_is_within_tolerance_and_never_pauses() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        0,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn small_drop_within_tolerance_is_accepted() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        999_950,
        100,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
}

#[test]
fn unexplained_drop_beyond_tolerance_breaches_and_auto_pauses() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        100,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn balance_increase_is_never_a_breach() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        2_000_000,
        100,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);
}

#[test]
fn hard_invariant_breach_pauses_even_within_the_delta_tolerance() {
    // Commit the maximum reservable amount (capped by available capacity:
    // balance 1_000_000 - protected_minimum 100_000 = 900_000) so
    // protected_minimum + pending_obligations == the cached balance
    // exactly. Then simulate the LIVE chain balance having dropped below
    // that (e.g. an unauthorized external movement) — this is precisely
    // what reconciliation exists to catch, and it must fire even with a
    // huge delta tolerance, because it is not a delta check at all.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 900_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 900_000,
                net_destination_atomic: 900_000,
            },
            &[1u8; 32],
            None,
            3600,
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 900_000, 1, [0xBB; 32], 1)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 2).unwrap();

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        950_000,
        1_000_000,
        10,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}

#[test]
fn reconciliation_never_auto_unpauses() {
    let mut ledger = setup();
    reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        100,
        10,
    )
    .unwrap();
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
    // Balance "recovers" to the original cached value.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        100,
        20,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(
        ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
        "reconciliation must never auto-unpause; only an operator may"
    );
}

#[test]
fn reconciliation_reports_accrued_fees_without_them_masking_a_real_breach() {
    // Credit a large accrued-fee balance directly, as real settlements
    // would over time (docs/20-bridge-fee.md). This must be surfaced for
    // audit visibility but must NEVER be treated as capacity that could
    // excuse an otherwise-real invariant breach — "do not let collected
    // fees falsely increase the amount considered available for customer
    // payouts."
    let mut ledger = setup();
    ledger
        .raw()
        .execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = 500000 WHERE direction = 'SolanaReserve'",
            [],
        )
        .unwrap();

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        0,
        100,
    )
    .unwrap();
    assert_eq!(report.accrued_fees, 500_000);
    assert_eq!(report.classification, Classification::WithinTolerance);

    // A real unexplained drop must still breach and auto-pause regardless
    // of how large the accrued-fee balance is.
    let breach_report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        100,
        200,
    )
    .unwrap();
    assert_eq!(
        breach_report.accrued_fees, 500_000,
        "accrued fees are still reported during a breach"
    );
    assert_eq!(breach_report.classification, Classification::Breach);
    assert!(breach_report.auto_paused);
}

#[test]
fn every_finding_is_recorded_including_skips() {
    let mut ledger = setup();
    reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        0,
        1,
    )
    .unwrap();
    record_skipped(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        "rpc_timeout",
        2,
    )
    .unwrap();
    let count: i64 = ledger
        .raw()
        .query_row("SELECT count(*) FROM reconciliation_findings", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 2);
    let skipped: String = ledger
        .raw()
        .query_row(
            "SELECT classification FROM reconciliation_findings ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(skipped.starts_with("SKIPPED"));
}

#[test]
fn confirmed_rebalance_is_never_misclassified_as_an_unexplained_breach() {
    // Mirrors the exact rationale behind mark_release_confirmed's own
    // immediate-balance-decrement fix (docs/14-phase6-checkpoint.md bug
    // 3): an operator-authorized, confirmed rebalance is an EXPLAINED
    // balance change, and Ledger::confirm_rebalance must keep the cached
    // total_reserve_balance self-consistent with it immediately — not
    // leave the next reconciliation tick to discover a "surprise" drop
    // and misclassify routine, authorized activity as a breach.
    let mut ledger = setup();
    // Baseline cached balance is 1_000_000 (see `setup`).

    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            crate::ledger::RebalanceKind::Withdraw,
            50_000,
            "sweep surplus to cold storage",
            "ops-alice",
            1,
            5,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 6).unwrap();
    ledger
        .record_rebalance_executed(id, "sig-reconciliation-interaction", "ops-alice", 7)
        .unwrap();

    // Before confirm_rebalance: the real (post-withdrawal) balance would
    // still look like an unexplained drop against the stale cache.
    let premature = reconcile(&mut ledger, ReserveDirection::SolanaReserve, 950_000, 0, 8).unwrap();
    assert_eq!(
        premature.classification,
        Classification::Breach,
        "before confirmation, reconciliation has no way to know this drop is explained"
    );
    ledger
        .set_paused(
            ReserveDirection::SolanaReserve,
            false,
            Some("test: undo premature auto-pause"),
        )
        .unwrap();

    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 9)
        .unwrap();

    // After confirm_rebalance: the same real balance now reconciles cleanly.
    let report = reconcile(&mut ledger, ReserveDirection::SolanaReserve, 950_000, 0, 10).unwrap();
    assert_eq!(
        report.classification,
        Classification::WithinTolerance,
        "a confirmed, operator-authorized rebalance must never be misclassified as a breach"
    );
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

/// Puts a GlcToSol request through to `DestinationSubmitted` for
/// `net_destination_atomic`, so its amount counts toward
/// `Ledger::pending_destination_settlement_amount(SolanaReserve)`.
fn glc_to_sol_request_destination_submitted(ledger: &mut Ledger, net_destination_atomic: u64) {
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: net_destination_atomic,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: net_destination_atomic,
                net_destination_atomic,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(
            request_id,
            [0xAA; 32],
            0,
            net_destination_atomic,
            10,
            [0xBB; 32],
            2,
        )
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 3).unwrap();
    ledger
        .record_release_submitted(request_id, [0xCC; 64], 4)
        .unwrap();
}

#[test]
fn a_drop_fully_matching_a_pending_destination_submission_is_in_flight_explained_not_a_breach() {
    let mut ledger = setup();
    // A release for exactly 50_000 has been broadcast to Solana but not
    // yet folded into Settled bookkeeping when the observed balance
    // already reflects the debit.
    glc_to_sol_request_destination_submitted(&mut ledger, 50_000);

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        950_000, // 1_000_000 cached - 50_000, exactly the pending release
        0,
        0,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::InFlightExplained);
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn a_drop_beyond_the_pending_destination_submission_still_breaches_on_the_residual() {
    let mut ledger = setup();
    // Only 50_000 is legitimately pending, but the real balance dropped by
    // 90_000 — the extra 40_000 is genuinely unexplained and must still
    // breach and auto-pause, exactly as before this fix, on the residual
    // amount only.
    glc_to_sol_request_destination_submitted(&mut ledger, 50_000);

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        910_000, // 1_000_000 cached - 90_000
        0,
        0,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn in_flight_explained_never_masks_a_hard_invariant_breach() {
    // Even when a drop is fully explained by a pending destination
    // submission, the hard solvency invariant (observed >=
    // protected_minimum + pending_obligations) is checked independently
    // and must still breach if it fails — in-flight explanation only ever
    // affects the delta/tolerance check, never the solvency floor.
    let mut ledger = setup();
    glc_to_sol_request_destination_submitted(&mut ledger, 50_000);
    // protected_minimum is 100_000 (setup()); observe a balance below that
    // floor even though it exactly matches the "explained" delta from
    // 1_000_000.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        50_000, // observed < protected_minimum (100_000)
        0,
        0,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}

#[test]
fn a_broadcast_goldcoin_payout_temporarily_consuming_its_whole_utxo_is_in_flight_explained() {
    // Regression: a UTXO-based-chain-specific effect distinct from the
    // generic DestinationSubmitted/DestinationConfirmed explanation above.
    // Spending the vault's UTXO to fund a payout makes that UTXO's ENTIRE
    // value (paid-out portion AND its own change) temporarily invisible to
    // a confirmed-only `listunspent` read until the payout transaction
    // itself confirms and the change output matures — even though none of
    // that value has actually left the vault's control (the change
    // returns to it). Observed once for real during release-candidate
    // validation: a 6,000 GLC vault funded via one UTXO, spent by a small
    // (~100 GLC) payout, made the ENTIRE 6,000 GLC balance disappear from
    // reconciliation's view for one tick and breach/auto-pause even though
    // ~5,900 GLC of legitimate change was still fully vault-controlled,
    // just unconfirmed.
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            600_000_000_000, // 6,000 GLC cached baseline
            0,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000,
            0,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 10_000_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 10_000_000_000,
                net_destination_atomic: 10_000_000_000,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };

    // A payout consuming the whole vault UTXO: pays out 10_000_000_000
    // (100 GLC), returns 589_900_000_000 (5,899 GLC) as change, pays
    // 100_000_000 (1 GLC) fee — total input value 600_000_000_000 (6,000
    // GLC), matching the vault's entire cached balance.
    ledger
        .raw()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, broadcast_at)
             VALUES (?1, X'00', 10000000000, 589900000000, 100000000, X'00', 'Broadcast', 1, 1)",
            [request_id],
        )
        .unwrap();

    // Both the paid-out portion and the not-yet-matured change are
    // temporarily invisible: observed balance drops to ~0.
    let report = reconcile(&mut ledger, ReserveDirection::GoldcoinReserve, 0, 0, 10).unwrap();
    assert_eq!(
        report.classification,
        Classification::InFlightExplained,
        "the entire drop must be explained by the broadcast-but-unconfirmed payout's full \
         input value, not just its net payout amount: {report:?}"
    );
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
}

#[test]
fn goldcoin_in_flight_explanation_does_not_leak_into_solana_reconciliation() {
    // The broadcast-payout UTXO-value term is GoldcoinReserve-specific
    // (Goldcoin is UTXO-based; Solana is account-based and has no
    // "change" concept) — must never be added when reconciling
    // SolanaReserve.
    let mut ledger = setup();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            600_000_000_000,
            0,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 10_000_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 10_000_000_000,
                net_destination_atomic: 10_000_000_000,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .raw()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, broadcast_at)
             VALUES (?1, X'00', 10000000000, 589900000000, 100000000, X'00', 'Broadcast', 1, 1)",
            [request_id],
        )
        .unwrap();

    // A real, unexplained drop on SolanaReserve must still breach — the
    // huge unrelated Goldcoin broadcast payout above must not leak in and
    // explain it away.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        0,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}
