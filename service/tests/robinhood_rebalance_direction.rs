//! `robinhood` IS an accepted rebalance direction — because the executable
//! path now exists.
//!
//! # History, and why this file was rewritten
//!
//! Until `GlcRobinhoodBridge.executeTreasuryWithdraw` existed, this file
//! pinned the OPPOSITE: that a `RobinhoodReserve` `Withdraw` could not be
//! proposed at all, at the ledger, at the CLI and at the admin API,
//! because a proposal nothing could execute would have been an
//! approvable row asserting a reserve movement the chain never performed
//! (PR #80, `docs/34-robinhood-reserve-withdrawal.md` §7). Its own docs
//! said it would have to be deliberately rewritten when the contract
//! gained a withdrawal entry point. It has:
//!
//! - the contract has `executeTreasuryWithdraw` (action `0x0C`) paying
//!   the immutable `TREASURY`, gated on both directions being paused;
//! - schema v26 admits `RobinhoodReserve` in `rebalance_requests` and a
//!   `TreasuryWithdraw` kind in `robinhood_transactions`;
//! - `glc-admin robinhood-treasury-withdraw` executes an APPROVED request
//!   and exits 0 only on a finalized receipt.
//!
//! So what this file pins now is the acceptance, the exactness of the
//! amount pipeline that feeds it, and that Goldcoin and Solana are as
//! untouched by the addition as they were by the refusal.

use std::path::Path;
use std::process::Command;

use glc_reserve_bridge_service::amount_conversion::robinhood::RobinhoodAtomic;
use glc_reserve_bridge_service::amount_conversion::CanonicalAtomic;
use glc_reserve_bridge_service::chain_policy::human::parse_glc;
use glc_reserve_bridge_service::ledger::{Ledger, RebalanceKind, RebalanceState, ReserveDirection};

const NOW: i64 = 1_700_000_000;

// ===================================================================
// Layer 1 — the ledger
// ===================================================================

#[test]
fn the_ledger_accepts_a_robinhood_withdraw_proposal() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            10_000_000,
            "move reserve to treasury",
            "ops:alice",
            2,
            NOW,
        )
        .expect("a Robinhood withdraw is executable now, so it is proposable");
    let row = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(row.direction, ReserveDirection::RobinhoodReserve);
    assert_eq!(row.kind, RebalanceKind::Withdraw);
    assert_eq!(row.state, RebalanceState::Proposed);
    assert_eq!(row.amount_atomic, 10_000_000);
}

/// The full approval lifecycle the executor requires: a withdrawal is
/// executed from `Approved` and nowhere else, so the ledger must get it
/// there.
#[test]
fn a_robinhood_withdraw_reaches_approved_through_the_ordinary_approvals() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Withdraw,
            10_000_000,
            "move reserve to treasury",
            "ops:alice",
            2,
            NOW,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops:bob", NOW + 1).unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Proposed,
        "one of two approvals"
    );
    ledger.approve_rebalance(id, "ops:carol", NOW + 2).unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Approved
    );
}

#[test]
fn a_robinhood_deposit_proposal_is_accepted_too() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::RobinhoodReserve,
            RebalanceKind::Deposit,
            10_000_000,
            "fund the Robinhood reserve",
            "ops:alice",
            1,
            NOW,
        )
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().kind,
        RebalanceKind::Deposit
    );
}

/// Untouched by the addition, as it was by the refusal.
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
            .unwrap();
        let row = ledger.get_rebalance(id).unwrap().unwrap();
        assert_eq!(row.direction, direction);
        assert_eq!(row.state, RebalanceState::Proposed);
    }
}

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
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Executed
    );
}

// ===================================================================
// Layer 2 — the amount pipeline: whole GLC -> canonical 8dp -> Robinhood 18dp
// ===================================================================
//
// Three units meet here. An operator types GLC; the proposal stores
// canonical 8dp; the contract is paid in 18dp. The two scale factors are
// 10^8 and 10^10, and confusing either — or applying 10^12 by "helpfully"
// going GLC -> 18dp in one step somewhere the code expected canonical —
// is exactly the kind of silent, decimal-looking mistake this pins
// against.

const CANONICAL_PER_GLC: u64 = 100_000_000; // 10^8
const ROBINHOOD_PER_CANONICAL: u128 = 10_000_000_000; // 10^10
const ROBINHOOD_PER_GLC: u128 = 1_000_000_000_000_000_000; // 10^18

#[test]
fn whole_glc_parses_to_canonical_exactly() {
    assert_eq!(parse_glc("1000").unwrap().0, 1000 * CANONICAL_PER_GLC);
    assert_eq!(
        parse_glc("1000.5").unwrap().0,
        1000 * CANONICAL_PER_GLC + 50_000_000
    );
    assert_eq!(parse_glc("0.00000001").unwrap().0, 1, "one canonical unit");
    assert_eq!(
        parse_glc("1,000,000").unwrap().0,
        1_000_000 * CANONICAL_PER_GLC
    );
}

#[test]
fn glc_finer_than_eight_decimals_is_refused_not_rounded() {
    assert!(parse_glc("1.000000001").is_err(), "nine decimals");
    assert!(parse_glc("0.123456789").is_err());
    // And a Robinhood-atomic figure typed where GLC was expected does
    // not parse into something plausible either: it is a 22-digit
    // integer, which is a legal (if absurd) GLC count, so the guard
    // against that mistake is the dry run's printed GLC line — asserted
    // below on the real binary.
}

/// canonical -> 18dp is EXACTLY 10^10, never 10^12 (GLC -> 18dp) and
/// never 10^8 (GLC -> canonical).
#[test]
fn canonical_widens_to_robinhood_by_exactly_ten_to_the_ten() {
    let one_glc_canonical = CanonicalAtomic(CANONICAL_PER_GLC);
    let widened = RobinhoodAtomic::from_canonical(one_glc_canonical).unwrap();
    assert_eq!(widened.get(), ROBINHOOD_PER_GLC, "1 GLC = 10^18 atomic");
    assert_eq!(
        widened.get(),
        u128::from(CANONICAL_PER_GLC) * ROBINHOOD_PER_CANONICAL
    );
    // The two wrong factors, stated so a future "fix" cannot pass.
    assert_ne!(
        widened.get(),
        u128::from(CANONICAL_PER_GLC) * 1_000_000_000_000
    );
    assert_ne!(widened.get(), u128::from(CANONICAL_PER_GLC) * 100_000_000);
}

/// End to end: the figure an operator types is the figure the contract
/// is paid, through both conversions, with nothing lost or invented.
#[test]
fn glc_to_canonical_to_robinhood_round_trips_exactly() {
    for text in ["1", "1000", "1000.5", "12345.67891234", "0.00000001"] {
        let canonical = parse_glc(text).unwrap();
        let robinhood = RobinhoodAtomic::from_canonical(canonical).unwrap();
        assert_eq!(
            robinhood.get() % ROBINHOOD_PER_CANONICAL,
            0,
            "{text}: exact multiple"
        );
        assert_eq!(
            robinhood.to_canonical().unwrap().0,
            canonical.0,
            "{text}: narrows back to the same canonical figure"
        );
    }
}

/// The 8dp-vs-18dp regression: a canonical figure mistaken for an 18dp
/// one is 10^10 times too SMALL, and an 18dp figure mistaken for canonical
/// is 10^10 times too LARGE. Both are unrepresentable as a round trip.
#[test]
fn an_18dp_figure_stored_as_canonical_does_not_survive_the_round_trip() {
    // 20 GLC in 18dp, wrongly written into a canonical (u64) field: it
    // does not even fit — u64 tops out at 18.44 GLC at 18 decimals, which
    // is why the Robinhood unit is a u128 in the first place.
    assert!(
        u64::try_from(ROBINHOOD_PER_GLC * 20).is_err(),
        "2 * 10^19 overflows u64"
    );
    // 1 GLC in canonical, wrongly read as 18dp: narrowing it back yields
    // a fraction of a canonical unit, which the conversion refuses.
    let wrong = RobinhoodAtomic::new(u128::from(CANONICAL_PER_GLC));
    assert!(
        wrong.to_canonical().is_err(),
        "10^8 is not a multiple of 10^10"
    );
}

// ===================================================================
// Layer 3 — glc-admin
// ===================================================================

#[test]
fn glc_admin_accepts_direction_robinhood_on_rebalance_propose() {
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
    assert!(out.status.success(), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout.contains("robinhood-treasury-withdraw"),
        "the proposal must tell the operator how it gets executed: {}",
        out.stdout
    );
    let ledger = Ledger::open(&db).unwrap();
    let rows = ledger.list_rebalances(None, false).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].direction, ReserveDirection::RobinhoodReserve);
    assert_eq!(rows[0].kind, RebalanceKind::Withdraw);
}

/// `--amount-glc` converts EXACTLY and the confirmation line shows both
/// the canonical figure and the GLC figure, so an operator who typed the
/// wrong unit sees it before anyone approves.
#[test]
fn glc_admin_amount_glc_converts_exactly_and_echoes_both_units() {
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
        "--amount-glc",
        "1000.5",
        "--by",
        "ops:alice",
        "--required-approvals",
        "2",
        "--note",
        "treasury top-up",
    ]);
    assert!(out.status.success(), "{}{}", out.stdout, out.stderr);
    assert!(
        out.stdout.contains("100050000000 canonical atomic"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("1,000.5 GLC"), "{}", out.stdout);
    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(
        ledger.list_rebalances(None, false).unwrap()[0].amount_atomic,
        100_050_000_000
    );
}

#[test]
fn glc_admin_refuses_amount_glc_finer_than_eight_decimals() {
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
        "--amount-glc",
        "1.000000001",
        "--by",
        "ops:alice",
        "--required-approvals",
        "1",
        "--note",
        "n",
    ]);
    assert!(!out.status.success());
    assert!(
        Ledger::open(&db)
            .unwrap()
            .list_rebalances(None, false)
            .unwrap()
            .is_empty(),
        "nothing written"
    );
}

#[test]
fn glc_admin_refuses_both_amount_flags_at_once() {
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
        "--amount-glc",
        "1",
        "--by",
        "ops:alice",
        "--required-approvals",
        "1",
        "--note",
        "n",
    ]);
    assert!(!out.status.success());
    assert!(out.stderr.contains("not both"), "{}", out.stderr);
}

/// The generic-unknown message names all three now.
#[test]
fn an_unknown_direction_names_the_three_accepted_ones() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed(&db);
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
    assert!(!out.status.success());
    assert!(
        out.stderr.contains("goldcoin|solana|robinhood"),
        "{}",
        out.stderr
    );
}

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
    assert!(out.status.success(), "{}{}", out.stdout, out.stderr);
    assert!(
        !out.stdout.contains("robinhood-treasury-withdraw"),
        "a Solana proposal is executed by glc-treasury-withdraw, not the Robinhood command"
    );
}

/// `pause`/`unpause` deliberately still do not take `robinhood` — the
/// local Robinhood gate has its own command — so the two parsers really
/// are separate.
#[test]
fn pause_still_refuses_direction_robinhood() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed(&db);
    let out = run_admin(&[
        "pause",
        "--db",
        db.to_str().unwrap(),
        "--direction",
        "robinhood",
        "--note",
        "n",
    ]);
    assert!(!out.status.success());
}

/// The dry run of the executor refuses without a Robinhood-configured
/// service config and never touches the ledger — proven here at the
/// binary boundary with a config that has no Robinhood sections.
#[test]
fn the_executor_takes_no_destination_and_no_amount_flags() {
    let out = run_admin(&["--help"]);
    assert!(out.stdout.contains("robinhood-treasury-withdraw"));
    let flat = out.stdout.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        flat.contains("There is no --destination and no --amount"),
        "{}",
        out.stdout
    );
    assert!(out
        .stdout
        .contains("REQUIRES depositsPaused AND payoutsPaused"));
    assert!(out.stdout.contains("Exit 0 ONLY when the operation is"));
}

// ===================================================================
// Harness
// ===================================================================

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
