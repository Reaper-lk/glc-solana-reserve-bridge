//! One-route fee edit tests.
//!
//! The property under test throughout: **exactly one route moves**, the
//! file is still the operator's file (comments and unrelated sections
//! intact), and nothing is written unless the candidate loaded.

use super::*;
use crate::config::tests::valid_config;

fn config_with(dir: &std::path::Path, appended: &str) -> PathBuf {
    let path = valid_config(dir);
    let mut toml = fs::read_to_string(&path).unwrap();
    toml.push_str(appended);
    fs::write(&path, toml).unwrap();
    path
}

const LAUNCH_FEES: &str =
    "\n# OPERATOR NOTE: approved 2026-09-10, ticket OPS-1234.\n[fees]\nGlcToSol = 300\n\
     SolToGlc = 300\nGlcToRhn = 600\nRhnToGlc = 600\n";

#[test]
fn a_dry_run_writes_nothing_to_the_config() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);
    let before = fs::read_to_string(&path).unwrap();

    let plan = plan(&path, Route::RhnToGlc, 300).unwrap();
    assert_eq!(plan.before(), Some(600));
    assert_eq!(plan.after(), 300);
    assert!(!plan.is_noop());
    // The candidate exists and holds the new content...
    assert!(plan.rendered().unwrap().contains("RhnToGlc = 300"));
    // ...and the real file is untouched, byte for byte.
    assert_eq!(fs::read_to_string(&path).unwrap(), before);

    plan.discard();
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    // And the candidate is gone: a dry run leaves nothing behind.
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("candidate"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn committing_changes_exactly_one_route() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);

    let plan = plan(&path, Route::RhnToGlc, 300).unwrap();
    let report = commit(plan, 1_757_462_400).unwrap();

    let after = Config::load(&path).unwrap();
    assert_eq!(after.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(after.route_fees.fee_bps(Route::GlcToSol).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::SolToGlc).unwrap(), 300);

    // The backup exists and holds the ORIGINAL rates.
    assert!(report.backup.exists());
    let backed_up = fs::read_to_string(&report.backup).unwrap();
    assert!(backed_up.contains("RhnToGlc = 600"), "{backed_up}");
}

#[test]
fn the_operators_comments_and_unrelated_sections_survive() {
    // A config file's comments are the reasoning behind its numbers. A
    // tool that silently deleted them would cost more than it saved.
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);
    let before = fs::read_to_string(&path).unwrap();

    let plan = plan(&path, Route::GlcToRhn, 300).unwrap();
    commit(plan, 1_757_462_400).unwrap();
    let after = fs::read_to_string(&path).unwrap();

    assert!(
        after.contains("# OPERATOR NOTE: approved 2026-09-10, ticket OPS-1234."),
        "{after}"
    );
    assert!(after.contains("[solana]"), "{after}");
    assert!(after.contains("[operators]"), "{after}");
    // Exactly one line differs.
    let differing: Vec<_> = before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(differing.len(), 1, "{differing:?}");
    assert_eq!(differing[0].0.trim(), "GlcToRhn = 600");
    assert_eq!(differing[0].1.trim(), "GlcToRhn = 300");
}

#[test]
fn the_first_edit_creates_a_complete_fees_section_from_the_rates_in_force() {
    // A production config has no `[fees]`. Creating it makes it
    // authoritative, so it must be created COMPLETE — seeded with exactly
    // what the deployment was already charging, plus the one change.
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(
        dir.path(),
        "\n[robinhood.policy]\nfee_bps = 600\nper_transfer_limit = 2000000000000\n\
         rolling_daily_limit = 1000000000000000\n",
    );
    let before = Config::load(&path).unwrap();
    assert_eq!(before.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);

    let plan = plan(&path, Route::RhnToGlc, 300).unwrap();
    // The operator is TOLD which keys had to appear.
    let mut seeded: Vec<&str> = plan.seeded_routes().iter().map(|r| r.as_str()).collect();
    seeded.sort_unstable();
    assert_eq!(seeded, vec!["GlcToRhn", "GlcToSol", "SolToGlc"]);
    commit(plan, 1_757_462_400).unwrap();

    let after = Config::load(&path).unwrap();
    // The one requested change...
    assert_eq!(after.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 300);
    // ...and every other route pinned at what it was ALREADY charging,
    // now stated explicitly rather than inherited.
    assert_eq!(after.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(
        after.route_fees.fee_bps(Route::GlcToSol).unwrap(),
        crate::amount_conversion::BRIDGE_FEE_BPS
    );
    assert_eq!(
        after.route_fees.fee_bps(Route::SolToGlc).unwrap(),
        crate::amount_conversion::BRIDGE_FEE_BPS
    );
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[fees]"), "{text}");
}

#[test]
fn a_subsequent_edit_seeds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);
    let plan = plan(&path, Route::GlcToSol, 100).unwrap();
    assert!(plan.seeded_routes().is_empty());
    plan.discard();
}

#[test]
fn changing_the_robinhood_fee_never_moves_the_solana_fee() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);

    for (route, bps) in [(Route::GlcToRhn, 300u64), (Route::RhnToGlc, 100u64)] {
        let plan = plan(&path, route, bps).unwrap();
        commit(plan, 1_757_462_400).unwrap();
    }

    let after = Config::load(&path).unwrap();
    assert_eq!(after.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 100);
    assert_eq!(after.route_fees.fee_bps(Route::GlcToSol).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::SolToGlc).unwrap(), 300);
}

#[test]
fn changing_the_solana_fee_never_moves_the_robinhood_fee() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);

    let plan = plan(&path, Route::GlcToSol, 100).unwrap();
    commit(plan, 1_757_462_400).unwrap();

    let after = Config::load(&path).unwrap();
    assert_eq!(after.route_fees.fee_bps(Route::GlcToSol).unwrap(), 100);
    assert_eq!(after.route_fees.fee_bps(Route::SolToGlc).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(after.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 600);
}

#[test]
fn a_cross_route_can_be_priced_for_the_first_time_without_touching_the_others() {
    // The one edit with no "before": a Solana<->Robinhood route that
    // has never been priced. Every other rate is untouched, and pricing
    // the route enables nothing.
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);

    let plan = plan(&path, Route::SolToRhn, 450).unwrap();
    assert_eq!(plan.before(), None);
    assert!(!plan.is_noop());
    assert!(plan.seeded_routes().is_empty());
    commit(plan, 1_757_462_400).unwrap();

    let after = Config::load(&path).unwrap();
    assert_eq!(after.route_fees.fee_bps(Route::SolToRhn).unwrap(), 450);
    assert!(after.route_fees.get(Route::RhnToSol).is_none());
    assert_eq!(after.route_fees.fee_bps(Route::GlcToSol).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::SolToGlc).unwrap(), 300);
    assert_eq!(after.route_fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(after.route_fees.fee_bps(Route::RhnToGlc).unwrap(), 600);
    assert!(!after.routes.enabled(Route::SolToRhn));
}

#[test]
fn creating_the_section_never_seeds_an_unpriced_cross_route() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), "");
    let plan = plan(&path, Route::RhnToGlc, 300).unwrap();
    let mut seeded: Vec<&str> = plan.seeded_routes().iter().map(|r| r.as_str()).collect();
    seeded.sort_unstable();
    assert_eq!(seeded, vec!["GlcToRhn", "GlcToSol", "SolToGlc"]);
    commit(plan, 1_757_462_400).unwrap();
    let after = Config::load(&path).unwrap();
    assert!(after.route_fees.get(Route::SolToRhn).is_none());
    assert!(after.route_fees.get(Route::RhnToSol).is_none());
}

#[test]
fn an_invalid_rate_is_refused_before_the_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);
    let before = fs::read_to_string(&path).unwrap();

    for (bad, expect) in [
        (10_000u64, "deliver nothing"),
        (10_001, "deliver nothing"),
        (u64::MAX, "deliver nothing"),
    ] {
        let err = plan(&path, Route::RhnToGlc, bad).unwrap_err().to_string();
        assert!(err.contains(expect), "rate {bad}: {err}");
    }
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("candidate"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_noop_edit_is_planned_and_reported_as_one() {
    // Not an error — an operator re-stating the current rate is allowed
    // to see "no change" rather than a refusal — but it must be visible.
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);
    let plan = plan(&path, Route::RhnToGlc, 600).unwrap();
    assert!(plan.is_noop());
    assert_eq!(plan.before(), Some(plan.after()));
    plan.discard();
}

#[test]
fn the_resulting_table_is_readable_from_the_plan_before_committing() {
    // What the operator is shown as "after", sourced from the RELOADED
    // candidate rather than from the document the planner built — so the
    // preview is what the daemon would actually resolve.
    let dir = tempfile::tempdir().unwrap();
    let path = config_with(dir.path(), LAUNCH_FEES);
    let plan = plan(&path, Route::RhnToGlc, 100).unwrap();

    let resulting: Vec<(String, u64)> = plan
        .resulting()
        .iter()
        .map(|(r, bps)| (r.as_str().to_string(), bps))
        .collect();
    assert_eq!(
        resulting,
        vec![
            ("GlcToSol".to_string(), 300),
            ("SolToGlc".to_string(), 300),
            ("GlcToRhn".to_string(), 600),
            ("RhnToGlc".to_string(), 100),
        ]
    );
    plan.discard();
}
