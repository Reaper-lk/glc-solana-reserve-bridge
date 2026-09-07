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
        immature_vault_utxo_total: 0,
        utxo_pool: crate::ledger::UtxoPoolHealth::default(),
        utxo_pool_warning: false,
        paused: false,
        admission_closed: false,
        liquidity_admission_closed: false,
        confirmed_admission_headroom: 7_000_000,
        admission_buffer_atomic: 0,
        admission_reopen_atomic: 0,
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
        // No Robinhood configured: contributes nothing.
        None,
        None,
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
    let report = build_report(
        Some(breached),
        None,
        0,
        None,
        None,
        // No Robinhood configured: contributes nothing.
        None,
        None,
        &[],
    );
    assert!(!report.healthy());
    assert_eq!(report.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(report.text().contains("BREACH goldcoin_reserve_invariant"));
}

#[test]
fn a_paused_reserve_is_reported_unhealthy_even_when_the_balance_invariant_holds() {
    let mut paused = healthy_snapshot(ReserveDirection::SolanaReserve);
    paused.paused = true;
    let report = build_report(
        None,
        Some(paused),
        0,
        None,
        None,
        // No Robinhood configured: contributes nothing.
        None,
        None,
        &[],
    );
    assert!(!report.healthy());
    assert!(report.text().contains("BREACH solana_reserve_active"));
    assert!(report.text().contains("solana_reserve_invariant")); // still reported, still OK
}

#[test]
fn a_manual_review_backlog_is_unhealthy() {
    let report = build_report(
        None,
        None,
        3,
        None,
        None,
        // No Robinhood configured: contributes nothing.
        None,
        None,
        &[],
    );
    assert!(!report.healthy());
    assert!(report
        .text()
        .contains("BREACH no_manual_review_backlog: 3 request(s) awaiting manual review"));
}

#[test]
fn a_halted_goldcoin_indexer_is_unhealthy_and_names_the_attempted_depth() {
    let report = build_report(
        None,
        None,
        0,
        Some(indexer(true)),
        None,
        // No Robinhood configured: contributes nothing.
        None,
        None,
        &[],
    );
    assert!(!report.healthy());
    assert!(report.text().contains("HALTED"));
    assert!(report.text().contains("attempted 12"));
}

#[test]
fn a_halted_solana_indexer_summary_never_produces_an_invariant() {
    // The Solana indexer has no halt concept — only Goldcoin's does.
    // Passing `halted: true` here should never happen in practice, but
    // even if it did, no invariant is generated for the Solana slot.
    let report = build_report(
        None,
        None,
        0,
        None,
        Some(indexer(true)),
        // No Robinhood configured: contributes nothing.
        None,
        None,
        &[],
    );
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
        // No Robinhood configured: contributes nothing.
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
        // No Robinhood configured: contributes nothing.
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
        // No Robinhood configured: contributes nothing.
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

// =====================================================================
// The Robinhood leg
// =====================================================================

fn robinhood(halted: bool) -> RobinhoodSummary {
    RobinhoodSummary {
        connected: true,
        expected_chain_id: Some(4663),
        observed_chain_id: Some(4663),
        chain_id_agrees: true,
        head_block: Some(1_000),
        finalized_block: Some(988),
        cursor_block: Some(995),
        lag_blocks: Some(5),
        seconds_since_success: 4,
        last_rpc_error_class: None,
        last_rpc_error: None,
        reorgs_reconciled: 1,
        deepest_reorg_blocks: 2,
        halted,
        halt_reason: halted.then_some("post_finality_reorg"),
        halt_detail: halted.then(|| "a finalized block was orphaned".to_string()),
        settlement_configured: true,
        deployment_verified: true,
        signers_available: 3,
        signers_required: 2,
        submitter_observed_nonce: Some(17),
        operations_in_flight: 0,
        operations_stalled: 0,
        // How this ships: every route closed.
        any_route_open: false,
    }
}

fn names(report: &HealthReport) -> Vec<&'static str> {
    report.invariants.iter().map(|i| i.name).collect()
}

/// The guarantee that matters most for an existing deployment: with no
/// Robinhood configuration, the report is byte-for-byte what it was.
/// Not "reports Robinhood as absent" — contributes NOTHING.
#[test]
fn an_absent_robinhood_config_changes_the_report_not_at_all() {
    let before = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        Some(healthy_snapshot(ReserveDirection::SolanaReserve)),
        0,
        Some(indexer(false)),
        Some(indexer(false)),
        None,
        None,
        &[],
    );
    assert!(before.healthy());
    // No Robinhood invariant, and no Robinhood metric family at all.
    assert!(!names(&before).iter().any(|n| n.contains("robinhood")));
    assert!(
        !before.metrics.contains("robinhood"),
        "an unconfigured deployment must emit no Robinhood gauge:\n{}",
        before.metrics
    );
    assert!(!before.text().contains("robinhood"));
}

#[test]
fn a_healthy_robinhood_leg_adds_invariants_and_gauges_without_breaching() {
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        Some(healthy_snapshot(ReserveDirection::SolanaReserve)),
        0,
        Some(indexer(false)),
        Some(indexer(false)),
        None,
        Some(robinhood(false)),
        &[],
    );
    assert!(report.healthy(), "{}", report.text());
    let names = names(&report);
    assert!(names.contains(&"robinhood_indexer_not_halted"));
    assert!(names.contains(&"robinhood_chain_id_agrees"));
    assert!(names.contains(&"robinhood_no_stalled_operations"));
    assert!(report.metrics.contains("glc_robinhood_lag_blocks"));
    assert!(report.metrics.contains("glc_robinhood_cursor_block"));
    assert!(report
        .metrics
        .contains("glc_robinhood_submitter_observed_nonce"));
}

/// A halted indexer observes nothing and never resolves on its own,
/// while the process stays alive for every other probe. It pages.
#[test]
fn a_halted_robinhood_indexer_breaches() {
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        Some(healthy_snapshot(ReserveDirection::SolanaReserve)),
        0,
        Some(indexer(false)),
        Some(indexer(false)),
        None,
        Some(robinhood(true)),
        &[],
    );
    assert!(!report.healthy());
    assert_eq!(report.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(report
        .text()
        .contains("BREACH robinhood_indexer_not_halted"));
    assert!(report.text().contains("post_finality_reorg"));
}

#[test]
fn a_chain_id_disagreement_breaches() {
    let mut rhn = robinhood(false);
    rhn.observed_chain_id = Some(1);
    rhn.chain_id_agrees = false;
    let report = build_report(None, None, 0, None, None, None, Some(rhn), &[]);
    assert!(!report.healthy());
    assert!(report.text().contains("BREACH robinhood_chain_id_agrees"));
}

#[test]
fn a_stalled_operation_breaches_because_it_is_never_retried() {
    let mut rhn = robinhood(false);
    rhn.operations_stalled = 2;
    let report = build_report(None, None, 0, None, None, None, Some(rhn), &[]);
    assert!(!report.healthy());
    assert!(report
        .text()
        .contains("BREACH robinhood_no_stalled_operations"));
}

/// The distinction that keeps this surface trustworthy. With every route
/// CLOSED — how this ships — an unformable quorum is a launch blocker,
/// and paging on it would train operators to ignore the page. With a
/// route OPEN it is a live outage.
#[test]
fn an_unformable_quorum_pages_only_when_a_route_is_actually_open() {
    let mut closed = robinhood(false);
    closed.signers_available = 0;
    closed.any_route_open = false;
    let report = build_report(None, None, 0, None, None, None, Some(closed), &[]);
    assert!(
        report.healthy(),
        "a missing quorum with every route closed is a launch blocker, not a page:\n{}",
        report.text()
    );
    assert!(!names(&report).contains(&"robinhood_signer_quorum_available"));
    // It is still REPORTED, as a gauge — visible without paging.
    assert!(report
        .metrics
        .contains("glc_robinhood_signer_quorum_available 0"));

    let mut open = robinhood(false);
    open.signers_available = 0;
    open.any_route_open = true;
    let report = build_report(None, None, 0, None, None, None, Some(open), &[]);
    assert!(!report.healthy());
    assert!(report
        .text()
        .contains("BREACH robinhood_signer_quorum_available"));
}

/// A third INDEPENDENT reserve: its own invariant, its own gauges, and no
/// arithmetic relationship to the other two.
#[test]
fn the_robinhood_reserve_is_reported_independently_and_never_netted() {
    let mut robinhood_reserve = healthy_snapshot(ReserveDirection::RobinhoodReserve);
    robinhood_reserve.total_reserve_balance = 4_000;
    robinhood_reserve.protected_minimum = 300;
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        Some(healthy_snapshot(ReserveDirection::SolanaReserve)),
        0,
        Some(indexer(false)),
        Some(indexer(false)),
        Some(robinhood_reserve),
        Some(robinhood(false)),
        &[],
    );
    assert!(report.healthy());
    assert!(names(&report).contains(&"robinhood_reserve_invariant"));
    assert!(names(&report).contains(&"robinhood_reserve_active"));
    // Its own figures, not a sum with anything.
    assert!(report
        .metrics
        .contains("glc_robinhood_reserve_balance_atomic 4000"));
    assert!(report
        .metrics
        .contains("glc_robinhood_reserve_protected_minimum_atomic 300"));
    // And the other two are unchanged by its presence.
    assert!(report
        .metrics
        .contains("glc_goldcoin_reserve_balance_atomic 10000000"));
    assert!(report
        .metrics
        .contains("glc_solana_reserve_balance_atomic 10000000"));
}

/// A Robinhood reserve that has not been configured contributes no row,
/// even when the indexer is running: watching the chain and holding a
/// reserve there are separate decisions.
#[test]
fn a_configured_indexer_without_a_configured_reserve_reports_no_reserve_row() {
    let report = build_report(
        Some(healthy_snapshot(ReserveDirection::GoldcoinReserve)),
        None,
        0,
        None,
        None,
        None,
        Some(robinhood(false)),
        &[],
    );
    assert!(!names(&report).contains(&"robinhood_reserve_invariant"));
    assert!(!report.metrics.contains("glc_robinhood_reserve_balance"));
    // But the indexer facts are still reported.
    assert!(names(&report).contains(&"robinhood_indexer_not_halted"));
}

/// A breached Robinhood reserve invariant pages, exactly like the other
/// two reserves'.
#[test]
fn a_breached_robinhood_reserve_invariant_breaches() {
    let mut breached = healthy_snapshot(ReserveDirection::RobinhoodReserve);
    breached.invariant_holds = false;
    let report = build_report(
        None,
        None,
        0,
        None,
        None,
        Some(breached),
        Some(robinhood(false)),
        &[],
    );
    assert!(!report.healthy());
    assert!(report.text().contains("BREACH robinhood_reserve_invariant"));
}
