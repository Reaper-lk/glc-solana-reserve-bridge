use super::*;
use crate::ledger::ReserveDirection;

fn healthy_snapshot(direction: ReserveDirection) -> ReserveSnapshot {
    ReserveSnapshot {
        direction,
        total_reserve_balance: 10_000_000,
        protected_minimum: 1_000_000,
        reserved_liquidity: 2_000_000,
        pending_obligations: 500_000,
        accrued_fees: 12_345,
        paused: false,
        invariant_holds: true,
    }
}

fn indexer(halted: bool) -> IndexerSummary {
    IndexerSummary {
        halted,
        halted_depth: if halted { 12 } else { 0 },
        seconds_since_tick: 5,
        deepest_reorg: 2,
        max_reorg_depth: 6,
    }
}

#[test]
fn a_fully_healthy_report_is_healthy_and_returns_200() {
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        Some(healthy_snapshot(ReserveDirection::SolanaReserve)),
        0,
        Some(indexer(false)),
        Some(indexer(false)),
        &[],
    );
    assert!(report.healthy());
    assert_eq!(report.status(), StatusCode::OK);
    assert!(report.text().starts_with("OK"));
}

#[test]
fn a_reserve_invariant_breach_makes_the_report_unhealthy_and_returns_503() {
    let mut breached = healthy_snapshot(ReserveDirection::GoldcoinReserve);
    breached.invariant_holds = false;
    let report = build_report(Some(breached), None, 0, None, None, &[]);
    assert!(!report.healthy());
    assert_eq!(report.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(report.text().contains("BREACH goldcoin_reserve_invariant"));
}

#[test]
fn a_paused_reserve_is_reported_unhealthy_even_when_the_balance_invariant_holds() {
    let mut paused = healthy_snapshot(ReserveDirection::SolanaReserve);
    paused.paused = true;
    let report = build_report(None, Some(paused), 0, None, None, &[]);
    assert!(!report.healthy());
    assert!(report.text().contains("BREACH solana_reserve_active"));
    assert!(report.text().contains("solana_reserve_invariant")); // still reported, still OK
}

#[test]
fn a_manual_review_backlog_is_unhealthy() {
    let report = build_report(None, None, 3, None, None, &[]);
    assert!(!report.healthy());
    assert!(report
        .text()
        .contains("BREACH no_manual_review_backlog: 3 request(s) awaiting manual review"));
}

#[test]
fn a_halted_goldcoin_indexer_is_unhealthy_and_names_the_attempted_depth() {
    let report = build_report(None, None, 0, Some(indexer(true)), None, &[]);
    assert!(!report.healthy());
    assert!(report.text().contains("HALTED"));
    assert!(report.text().contains("attempted 12"));
}

#[test]
fn a_halted_solana_indexer_summary_never_produces_an_invariant() {
    // The Solana indexer has no halt concept — only Goldcoin's does.
    // Passing `halted: true` here should never happen in practice, but
    // even if it did, no invariant is generated for the Solana slot.
    let report = build_report(None, None, 0, None, Some(indexer(true)), &[]);
    // Only the (always-present) manual-review invariant exists, and it's
    // healthy, so the report as a whole is healthy.
    assert!(report.healthy());
    assert!(!report.text().contains("solana_indexer_not_halted"));
}

#[test]
fn metrics_are_rendered_for_both_reserve_directions() {
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        Some(healthy_snapshot(ReserveDirection::SolanaReserve)),
        0,
        None,
        None,
        &[],
    );
    assert!(report
        .metrics
        .contains("glc_goldcoin_reserve_balance_atomic 10000000"));
    assert!(report
        .metrics
        .contains("glc_solana_reserve_balance_atomic 10000000"));
}

#[test]
fn extra_gauges_are_included() {
    let report = build_report(
        None,
        None,
        0,
        None,
        None,
        &[("glc_custom", 7.0, "custom help")],
    );
    assert!(report.metrics.contains("glc_custom 7"));
}

#[test]
fn an_empty_report_is_never_healthy() {
    // Direct construction, bypassing build_report (which always pushes at
    // least the manual-review invariant) — the same defensive property the
    // old bridge's HealthReport::healthy() had.
    let report = HealthReport::default();
    assert!(!report.healthy());
    assert_eq!(report.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(report.text().contains("UNKNOWN"));
}

#[test]
fn text_lines_are_one_per_invariant_with_ok_or_breach_prefix() {
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        None,
        0,
        None,
        None,
        &[],
    );
    let text = report.text();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), report.invariants.len());
    for line in lines {
        assert!(line.starts_with("OK") || line.starts_with("BREACH"));
    }
}
