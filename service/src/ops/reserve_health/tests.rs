use super::*;
use crate::ledger::Ledger;

fn ledger_with_direction(paused: bool) -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    if paused {
        ledger
            .set_paused(ReserveDirection::GoldcoinReserve, true, Some("test"))
            .unwrap();
    }
    ledger
}

#[test]
fn a_healthy_reserve_reports_the_invariant_holding_and_unpaused() {
    let ledger = ledger_with_direction(false);
    let snapshot = check(&ledger, ReserveDirection::GoldcoinReserve).unwrap();
    assert_eq!(snapshot.direction, ReserveDirection::GoldcoinReserve);
    assert_eq!(snapshot.total_reserve_balance, 10_000_000);
    assert_eq!(snapshot.protected_minimum, 0);
    assert!(snapshot.invariant_holds);
    assert!(!snapshot.paused);
}

#[test]
fn a_paused_reserve_is_reported_as_paused() {
    let ledger = ledger_with_direction(true);
    let snapshot = check(&ledger, ReserveDirection::GoldcoinReserve).unwrap();
    assert!(snapshot.paused);
    // Pause alone does not violate the balance invariant.
    assert!(snapshot.invariant_holds);
}

#[test]
fn an_unconfigured_reserve_errors_rather_than_reporting_a_fake_healthy_snapshot() {
    let ledger = Ledger::open_in_memory().unwrap();
    let result = check(&ledger, ReserveDirection::GoldcoinReserve);
    assert!(result.is_err());
}
