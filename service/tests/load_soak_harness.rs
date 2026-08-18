//! Load/soak testing for sustained bidirectional bridge traffic
//! (docs/22-production-readiness-review.md P1-3, docs/24-load-soak-
//! harness.md). The actual harness engine lives in
//! `tests/support/load_harness.rs`; this file is the thin `#[tokio::test]`
//! entry points, mirroring how `tests/regtest_acceptance.rs` itself is a
//! thin set of scenarios over `tests/support/mod.rs`.
//!
//! Both tests are skipped (never failed) unless the same Phase 6
//! prerequisites `regtest_acceptance.rs` requires are present — see
//! `support::phase6_prereqs`.

mod support;

use std::time::Duration;

use support::load_harness::{run_load_profile, LoadProfile};

/// Deterministic, short profile — safe to run in CI/local verification.
/// Finishes in well under a minute of real regtest/validator time.
#[tokio::test(flavor = "multi_thread")]
async fn smoke_load_profile_completes_with_healthy_accounting() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping smoke_load_profile_completes_with_healthy_accounting: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    let profile = LoadProfile::smoke();
    let report = run_load_profile(&goldcoind, &cli, &so, &profile).await;

    eprintln!("{}", report.summary());
    for stuck in &report.stuck_requests {
        eprintln!(
            "STUCK: id={} direction={:?} state={:?} age={}s",
            stuck.id, stuck.direction, stuck.state, stuck.age_seconds
        );
    }
    for err in &report.tick_errors {
        eprintln!("TICK ERROR: {err}");
    }

    assert!(
        report.accounting_healthy(),
        "load harness accounting invariants failed: {}",
        report.summary()
    );
    // A handful of transient reconciliation breaches under concurrent
    // settlement is a known, pre-existing, documented gap — reconciliation
    // has no tolerance for a payout that is broadcast but not yet folded
    // into this service's own "settled" bookkeeping at the instant a
    // reconciliation tick runs (`Classification::InFlightExplained`
    // exists in the type but is, per its own doc comment, "not yet
    // produced by this phase's logic"). This is real and worth watching,
    // but not something this harness invented or can fix — see
    // docs/24-load-soak-harness.md. What must never happen is a breach
    // that then blocks the run from completing: `stuck_requests` below is
    // the check that actually matters.
    assert!(
        report.reconciliation_breaches <= 2,
        "reconciliation breached more than the small transient allowance expected from the \
         known in-flight-settlement timing gap — investigate: {}",
        report.summary()
    );
    // A short, bounded profile is expected to fully drain: every issued
    // request should reach a terminal state within the drain timeout,
    // regardless of any transient reconciliation breach above.
    assert!(
        report.stuck_requests.is_empty(),
        "smoke profile must fully drain within its timeout, found stuck requests: {:?}",
        report.stuck_requests
    );
    let total_issued: u32 = report.requests_issued.values().sum();
    assert!(
        total_issued > 0,
        "smoke profile must actually issue some traffic: {}",
        report.summary()
    );
}

/// A genuine multi-hour soak is deliberately NOT run automatically here —
/// see docs/24-load-soak-harness.md. This test proves the *soak profile
/// itself* is wired correctly end-to-end (a short-duration instance of
/// it), not that a representative multi-hour run has been performed.
/// `#[ignore]`d so `cargo test` (and CI) never accidentally runs even
/// this short instance by default; run explicitly with
/// `cargo +nightly test --test load_soak_harness -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "short-duration soak-profile wiring check; not part of default cargo test — see docs/24-load-soak-harness.md"]
async fn soak_profile_wiring_short_duration_smoke() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping soak_profile_wiring_short_duration_smoke: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    let mut profile = LoadProfile::soak(Duration::from_secs(30));
    profile.name = "soak-wiring-check";
    profile.drain_timeout = Duration::from_secs(60);
    let report = run_load_profile(&goldcoind, &cli, &so, &profile).await;

    eprintln!("{}", report.summary());
    assert!(
        report.accounting_healthy(),
        "soak profile accounting invariants failed: {}",
        report.summary()
    );
    assert!(
        report.reconciliation_breaches <= 2,
        "reconciliation breached more than the small transient allowance expected from the \
         known in-flight-settlement timing gap — investigate: {}",
        report.summary()
    );
    let total_issued: u32 = report.requests_issued.values().sum();
    assert!(
        total_issued > 0,
        "soak profile must issue traffic: {}",
        report.summary()
    );
}

/// Regression coverage for the regtest funding-bootstrap gap discovered
/// when `LoadProfile::soak` was first driven at a real multi-hour
/// duration: its duration-scaled `initial_goldcoin_reserve` (72,000 GLC
/// at 4 hours) exceeded what the bootstrap's fixed 101-block mine
/// matures (one coinbase, 10,000 GLC on this regtest binary), so the
/// vault-funding `sendtoaddress` call failed before any workload ran.
/// `run_load_profile`'s bootstrap now mines additional blocks until the
/// wallet's own reported spendable balance covers the request,
/// independent of profile size — this test proves that by requesting a
/// reserve well above the old single-coinbase ceiling, on a short
/// duration so the test itself stays fast. `#[ignore]`d for the same
/// reason as `soak_profile_wiring_short_duration_smoke`: needs real
/// regtest/validator infrastructure, not part of default `cargo test`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "funding-bootstrap regression check; needs real regtest/validator infrastructure — not part of default cargo test"]
async fn funding_bootstrap_matures_enough_balance_for_a_large_reserve_profile() {
    let Some((goldcoind, cli, so)) = support::phase6_prereqs() else {
        eprintln!(
            "skipping funding_bootstrap_matures_enough_balance_for_a_large_reserve_profile: \
             Phase 6 prerequisites not available (see docs/13-phase6-readiness-audit.md)"
        );
        return;
    };

    let mut profile = LoadProfile::soak(Duration::from_secs(30));
    profile.name = "funding-bootstrap-regression";
    profile.drain_timeout = Duration::from_secs(60);
    // 80,000 GLC — well above the single-matured-coinbase ceiling (10,000
    // GLC on this regtest binary) that caused the original failure, and
    // above the 4-hour profile's own 72,000 GLC requirement, so this
    // proves the bootstrap generalizes rather than merely covering one
    // specific duration's auto-sized value.
    profile.initial_goldcoin_reserve = 80_000 * 100_000_000;
    let report = run_load_profile(&goldcoind, &cli, &so, &profile).await;

    eprintln!("{}", report.summary());
    assert!(
        report.accounting_healthy(),
        "load harness accounting invariants failed for a large-reserve funding profile: {}",
        report.summary()
    );
    let total_issued: u32 = report.requests_issued.values().sum();
    assert!(
        total_issued > 0,
        "large-reserve funding profile must actually issue traffic (proves the funding \
         transfer itself succeeded, not just that the bootstrap didn't panic): {}",
        report.summary()
    );
}
