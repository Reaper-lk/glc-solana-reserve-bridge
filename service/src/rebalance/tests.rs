use super::*;
use crate::ledger::Ledger;

fn ledger_with(
    balance: u64,
    protected_minimum: u64,
    target: u64,
    warning: u64,
    critical: u64,
) -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            balance,
            protected_minimum,
            target,
            warning,
            critical,
            0,
        )
        .unwrap();
    ledger
}

#[test]
fn balance_at_or_above_warning_is_normal_with_no_suggestion() {
    let ledger = ledger_with(10_000_000, 0, 8_000_000, 5_000_000, 2_000_000);
    let a = assess(&ledger, ReserveDirection::SolanaReserve).unwrap();
    assert_eq!(a.severity, ImbalanceSeverity::Normal);
    assert_eq!(a.suggested_deposit_atomic, None);
}

#[test]
fn balance_between_critical_and_warning_is_warning_with_a_target_restoring_suggestion() {
    let ledger = ledger_with(4_000_000, 0, 8_000_000, 5_000_000, 2_000_000);
    let a = assess(&ledger, ReserveDirection::SolanaReserve).unwrap();
    assert_eq!(a.severity, ImbalanceSeverity::Warning);
    assert_eq!(a.suggested_deposit_atomic, Some(8_000_000 - 4_000_000));
}

#[test]
fn balance_below_critical_is_critical_with_a_target_restoring_suggestion() {
    let ledger = ledger_with(1_000_000, 0, 8_000_000, 5_000_000, 2_000_000);
    let a = assess(&ledger, ReserveDirection::SolanaReserve).unwrap();
    assert_eq!(a.severity, ImbalanceSeverity::Critical);
    assert_eq!(a.suggested_deposit_atomic, Some(8_000_000 - 1_000_000));
}

#[test]
fn exact_boundary_values_classify_on_the_documented_side() {
    // docs/09-runbook.md: Normal is balance >= warning_reserve, Warning is
    // critical_reserve <= balance < warning_reserve — exact equality with
    // a band's lower bound belongs to that (better) band, not the one
    // below it.
    let ledger = ledger_with(5_000_000, 0, 8_000_000, 5_000_000, 2_000_000);
    let a = assess(&ledger, ReserveDirection::SolanaReserve).unwrap();
    assert_eq!(
        a.severity,
        ImbalanceSeverity::Normal,
        "exactly at warning_reserve is Normal"
    );

    let ledger = ledger_with(2_000_000, 0, 8_000_000, 5_000_000, 2_000_000);
    let a = assess(&ledger, ReserveDirection::SolanaReserve).unwrap();
    assert_eq!(
        a.severity,
        ImbalanceSeverity::Warning,
        "exactly at critical_reserve is Warning"
    );
}

#[test]
fn never_invents_a_value_beyond_what_is_already_configured() {
    // The suggested deposit is always exactly target_reserve - balance —
    // a value entirely derived from operator-configured fields, asserted
    // here so a future change can't quietly start adding a margin/fudge
    // factor that wasn't explicitly configured.
    let ledger = ledger_with(3_333_333, 0, 9_999_999, 5_000_000, 2_000_000);
    let a = assess(&ledger, ReserveDirection::SolanaReserve).unwrap();
    assert_eq!(a.suggested_deposit_atomic, Some(9_999_999 - 3_333_333));
}
