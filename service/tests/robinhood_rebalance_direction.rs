//! `robinhood` is NOT an accepted rebalance direction, and cannot become
//! one by accident.
//!
//! # What this pins, and why it is a test rather than a comment
//!
//! The Solana reserve has an executable withdrawal path: an on-chain
//! `treasury_withdraw` instruction, an allowlisted destination governed by
//! a timelocked `RebalancePolicy`, a threshold attestation over a
//! domain-separated claim, and `glc-treasury-withdraw plan|attest|execute`
//! to drive it. A `rebalance-propose --direction solana --kind withdraw`
//! row therefore describes something an operator can actually carry out.
//!
//! Robinhood has none of that, and — unlike Solana — cannot acquire it
//! from this repository alone. `GlcRobinhoodBridge` has no owner, no
//! admin, no proxy and no upgrade path (its own module docs say so), and
//! its only three outbound token transfers are:
//!
//! - `executePayout` — a user's `GlcToRhn` payout, to the recipient named
//!   in the signed request.
//! - `executeRefund` — a depositor's own principal, to the `depositor` the
//!   contract itself recorded.
//! - `finalizeMigration` — the ENTIRE balance, to a successor contract
//!   committed 48 hours earlier and vetoable by any single guardian.
//!
//! None of the three can send an operator-chosen amount to an
//! operator-chosen treasury. There is no `ACTION_*` discriminator for a
//! withdrawal, no EIP-712 type for one, and no arm for one in
//! `signing::evm_policy`, which matches on a closed set of three actions
//! and refuses `UnknownAction` for everything else.
//!
//! So the dangerous state is not "the withdrawal is missing" — it is a
//! `rebalance_requests` row that is `Proposed`, then `Approved`, then
//! marked `Executed` with a `tx_reference` that cannot correspond to any
//! real Robinhood withdrawal, because none can exist. That row would
//! assert a reserve movement the chain never performed, and reconciliation
//! compares the ledger against `balanceOf(bridge)`.
//!
//! These tests pin the refusal at all three layers that could admit such a
//! row, and pin that Goldcoin and Solana are untouched by it.
//!
//! When the contract gains a withdrawal entry point (see
//! `docs/34-robinhood-reserve-withdrawal.md` for the smallest change that
//! would do it), these tests are the ones that must be deliberately
//! rewritten — which is the point.

use std::path::Path;
use std::process::Command;

use glc_reserve_bridge_service::ledger::{
    Ledger, LedgerError, RebalanceKind, RebalanceState, ReserveDirection,
};

const NOW: i64 = 1_700_000_000;

// ===================================================================
// Layer 1 — the ledger, where the invariant actually lives
// ===================================================================

/// The load-bearing test. Every other refusal in this file is a parser
/// being helpful; this is the one that holds for a caller that never went
/// near a parser.
#[test]
fn the_ledger_refuses_a_robinhood_withdraw_proposal() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let err = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            10_000_000,
            "top-up treasury",
            "ops:alice",
            2,
            NOW,
        )
        .expect_err("a Robinhood withdraw rebalance must not be recordable");
    assert!(
        matches!(err, LedgerError::RobinhoodWithdrawalNotExecutable),
        "expected RobinhoodWithdrawalNotExecutable, got {err:?}"
    );
}

/// The refusal must say WHY, not merely that it happened: an operator who
/// hits this needs to know the blocker is on-chain and cannot be argued
/// with from this host.
#[test]
fn the_ledger_refusal_names_the_missing_contract_entry_point() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let err = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            1,
            "why",
            "ops:alice",
            1,
            NOW,
        )
        .unwrap_err()
        .to_string();
    for needle in [
        "no reserve-withdrawal entry point",
        "executePayout",
        "executeRefund",
        "finalizeMigration",
    ] {
        assert!(
            err.contains(needle),
            "refusal should name {needle:?}; got: {err}"
        );
    }
}

/// Nothing is written on the way to the refusal — no row, no state-machine
/// transition, no partially-created request an operator could later find
/// and approve.
#[test]
fn a_refused_robinhood_withdraw_leaves_no_row_behind() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let before = ledger.list_rebalances(None, false).unwrap().len();
    let _ = ledger.propose_rebalance(
        ReserveDirection::RobinhoodReserve,
        RebalanceKind::Withdraw,
        10_000_000,
        "top-up treasury",
        "ops:alice",
        2,
        NOW,
    );
    let after = ledger.list_rebalances(None, false).unwrap();
    assert_eq!(after.len(), before, "no rebalance row may be created");
    assert!(
        after
            .iter()
            .all(|r| r.direction != ReserveDirection::RobinhoodReserve),
        "no RobinhoodReserve request may exist"
    );
}

/// `RobinhoodReserve` is excluded from `rebalance_requests` at the SCHEMA
/// level too, for both kinds:
///
/// ```sql
/// direction TEXT NOT NULL CHECK (direction IN ('GoldcoinReserve','SolanaReserve'))
/// ```
///
/// That predates this work and is deliberate — schema v23 widened
/// `reserve_ledger`'s identical CHECK to admit `RobinhoodReserve` and
/// pointedly did NOT widen this one. The Robinhood reserve is ACCOUNTED,
/// never REBALANCED through this table.
///
/// Pinned here so the two layers are known to be independent. The Rust
/// guard is not load-bearing for `Deposit`, and a future migration that
/// widens this CHECK would still meet the Rust guard on the `Withdraw`
/// path rather than silently opening it.
#[test]
fn the_schema_excludes_the_robinhood_direction_from_rebalance_requests() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let err = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Deposit,
            10_000_000,
            "fund the Robinhood reserve",
            "ops:alice",
            2,
            NOW,
        )
        .expect_err("rebalance_requests has never accepted RobinhoodReserve");
    assert!(
        matches!(err, LedgerError::Sqlite(_)),
        "a Deposit is stopped by the table CHECK, not by the Rust guard; got {err:?}"
    );
    assert!(
        ledger.list_rebalances(None, false).unwrap().is_empty(),
        "and nothing is written"
    );
}

/// The two layers refuse the same direction for different reasons, and the
/// difference is the point: the `Withdraw` refusal EXPLAINS itself, because
/// that is the one an operator will actually try and the one whose blocker
/// is on-chain and cannot be argued with from this host. A raw
/// `ConstraintViolation` would tell them nothing.
#[test]
fn the_withdraw_refusal_is_the_explanatory_one() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let deposit = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Deposit,
            1,
            "n",
            "ops:alice",
            1,
            NOW,
        )
        .unwrap_err();
    let withdraw = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            1,
            "n",
            "ops:alice",
            1,
            NOW,
        )
        .unwrap_err();
    assert!(matches!(deposit, LedgerError::Sqlite(_)));
    assert!(matches!(
        withdraw,
        LedgerError::RobinhoodWithdrawalNotExecutable
    ));
}

/// The Solana reserve withdrawal path is what this whole gate is defined
/// in contrast to. It must keep working exactly as it did — a regression
/// here would mean the guard was written against the wrong predicate.
#[test]
fn solana_and_goldcoin_withdraw_proposals_are_unaffected() {
    for direction in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        let mut ledger = Ledger::open_in_memory().unwrap();
        let id = ledger
            .propose_rebalance(
                direction,
                RebalanceKind::Withdraw,
                10_000_000,
                "planned treasury rebalance",
                "ops:alice",
                2,
                NOW,
            )
            .unwrap_or_else(|e| panic!("{direction:?} withdraw must still be proposable: {e}"));
        let row = ledger.get_rebalance(id).unwrap().unwrap();
        assert_eq!(row.direction, direction);
        assert_eq!(row.kind, RebalanceKind::Withdraw);
        assert_eq!(row.state, RebalanceState::Proposed);
    }
}

/// The full Solana lifecycle — propose, approve, record executed — still
/// reaches `Executed`. The gate must not have narrowed the path that DOES
/// have an executable counterpart behind it.
#[test]
fn the_solana_withdraw_lifecycle_still_reaches_executed() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Withdraw,
            10_000_000,
            "planned treasury rebalance",
            "ops:alice",
            1,
            NOW,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops:bob", NOW + 1).unwrap();
    ledger
        .record_rebalance_executed(id, "5x1solanaSignature", "ops:bob", NOW + 2)
        .unwrap();
    let row = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(row.state, RebalanceState::Executed);
}

// ===================================================================
// Layer 2 — glc-admin, the operator-facing surface
// ===================================================================

#[test]
fn glc_admin_refuses_direction_robinhood_on_rebalance_propose() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed(&db);

    let out = run_admin(&[
        "rebalance-propose",
        "--db",
        db.to_str().unwrap(),
        "--direction",
        "robinhood",
        "--kind",
        "withdraw",
        "--amount",
        "10000000",
        "--by",
        "ops:alice",
        "--required-approvals",
        "2",
        "--note",
        "treasury top-up",
    ]);
    assert!(
        !out.status.success(),
        "must exit non-zero; stdout was: {}",
        out.stdout
    );
    let text = format!("{}{}", out.stdout, out.stderr);
    assert!(
        text.contains("no reserve-withdrawal entry point")
            || text.contains("NO reserve-withdrawal entry point"),
        "the refusal must state the reason, not just 'unknown direction'; got: {text}"
    );
    assert!(
        text.contains("docs/34-robinhood-reserve-withdrawal.md"),
        "the refusal must point at the analysis; got: {text}"
    );

    // And nothing was written.
    let ledger = Ledger::open(&db).unwrap();
    assert!(
        ledger.list_rebalances(None, false).unwrap().is_empty(),
        "a refused proposal must leave the ledger empty"
    );
}

/// `robinhood` is refused by NAME. "Unknown direction" reads like an
/// unbuilt parser arm and invites someone to go add it; this asserts the
/// operator is told the real, on-chain reason instead.
#[test]
fn the_cli_refusal_is_specific_not_a_generic_unknown_direction() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed(&db);

    let out = run_admin(&[
        "rebalance-propose",
        "--db",
        db.to_str().unwrap(),
        "--direction",
        "robinhood",
        "--kind",
        "withdraw",
        "--amount",
        "1",
        "--by",
        "ops:alice",
        "--required-approvals",
        "1",
        "--note",
        "n",
    ]);
    let text = format!("{}{}", out.stdout, out.stderr);
    assert!(
        !text.contains("unknown --direction robinhood"),
        "robinhood must not fall through to the generic arm; got: {text}"
    );

    // A genuinely unknown direction still gets the generic message, so
    // the specific one above is really about robinhood and not a blanket
    // rewrite of the parser's error.
    let out = run_admin(&[
        "rebalance-propose",
        "--db",
        db.to_str().unwrap(),
        "--direction",
        "dogecoin",
        "--kind",
        "withdraw",
        "--amount",
        "1",
        "--by",
        "ops:alice",
        "--required-approvals",
        "1",
        "--note",
        "n",
    ]);
    let text = format!("{}{}", out.stdout, out.stderr);
    assert!(
        text.contains("unknown --direction dogecoin"),
        "an actually-unknown direction keeps the generic message; got: {text}"
    );
}

#[test]
fn glc_admin_refuses_direction_robinhood_on_rebalance_list() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed(&db);

    let out = run_admin(&[
        "rebalance-list",
        "--db",
        db.to_str().unwrap(),
        "--direction",
        "robinhood",
    ]);
    assert!(!out.status.success(), "must exit non-zero");
}

/// The Solana operator workflow through the real binary is unchanged.
#[test]
fn glc_admin_still_proposes_a_solana_withdraw() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed(&db);

    let out = run_admin(&[
        "rebalance-propose",
        "--db",
        db.to_str().unwrap(),
        "--direction",
        "solana",
        "--kind",
        "withdraw",
        "--amount",
        "10000000",
        "--by",
        "ops:alice",
        "--required-approvals",
        "2",
        "--note",
        "planned treasury rebalance",
    ]);
    assert!(
        out.status.success(),
        "solana must still work: {}{}",
        out.stdout,
        out.stderr
    );

    let ledger = Ledger::open(&db).unwrap();
    let rows = ledger.list_rebalances(None, false).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].direction, ReserveDirection::SolanaReserve);
}

// ===================================================================
// Layer 3 — the usage text an operator reads before typing anything
// ===================================================================

/// The reason belongs where an operator looks BEFORE they get an error,
/// not only in the error. `--help` is the cheapest place to stop someone
/// planning a Robinhood rebalance that can never complete.
#[test]
fn the_usage_text_states_that_there_is_no_robinhood_direction() {
    let out = run_admin(&["--help"]);
    assert!(
        out.stdout.contains("no `robinhood` direction"),
        "glc-admin --help must say why robinhood is absent"
    );
    assert!(
        out.stdout
            .contains("docs/34-robinhood-reserve-withdrawal.md"),
        "and point at the analysis"
    );
}

// ===================================================================
// Harness
// ===================================================================

/// A ledger with all three reserves configured — the state a real
/// deployment is in, so a refusal here is about the direction and not
/// about an unconfigured reserve.
fn seed(db: &Path) {
    let mut ledger = Ledger::open(db).unwrap();
    for direction in ReserveDirection::ALL {
        ledger
            .configure_reserve(
                direction,
                1_000_000_000,
                100_000_000,
                800_000_000,
                500_000_000,
                200_000_000,
                NOW,
            )
            .unwrap();
    }
}

struct Output {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_admin(args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_glc-admin"))
        .args(args)
        .output()
        .expect("could not run glc-admin");
    Output {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}
