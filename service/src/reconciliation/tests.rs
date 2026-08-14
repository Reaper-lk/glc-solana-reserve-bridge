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
        .create_request(Direction::GlcToSol, 900_000, &[1u8; 32], None, 3600, 0)
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
