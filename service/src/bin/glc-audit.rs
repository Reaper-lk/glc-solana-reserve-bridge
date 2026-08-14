//! `glc-audit` — the offline integrity auditor
//! (docs/07-implementation-plan.md Phase 5). Ported from the old bridge's
//! `glc-audit` shape (docs/01-reuse-inventory.md).
//!
//! Re-verifies every frozen attestation-claim commitment against the
//! fields they were computed from, plus SQLite's own
//! `PRAGMA integrity_check`, and reports.
//!
//! # Run it against a snapshot, not the live database
//!
//! It opens the database **read-only in effect** (it never writes) and is
//! safe to run against the live file, but the point of an offline audit is
//! answering whether a *backup* is worth restoring from — the live
//! signing path already re-derives independently at signing time
//! ([`glc_reserve_bridge_service::signing::attestation`]).
//!
//! # Exit codes
//!
//! - `0` — checked, and everything agreed
//! - `1` — findings; the report names each one
//! - `2` — could not run (bad arguments, unreadable database)
//!
//! Distinct so a cron job can page on `1` and alert differently on `2`. An
//! audit that could not run is not a passing audit, and collapsing the two
//! would make a broken cron entry look like a clean bill of health.

use std::path::PathBuf;

use glc_reserve_bridge_service::ledger::Ledger;
use glc_reserve_bridge_service::ops::audit;

const USAGE: &str = "glc-audit — offline integrity auditor

  glc-audit --db PATH [--quiet]

Re-verifies every frozen attestation record against the fields it was
computed from, plus SQLite's own PRAGMA integrity_check. Never writes, never
halts a request, and is safe to run against a backup on another host.

Exit 0 = clean, 1 = findings, 2 = could not run.";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return;
    }
    let Some(db_path) = args
        .iter()
        .position(|a| a == "--db")
        .and_then(|i| args.get(i + 1))
    else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };
    let quiet = args.iter().any(|a| a == "--quiet");

    let ledger = match Ledger::open(&PathBuf::from(db_path)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("could not open {db_path}: {e}");
            std::process::exit(2);
        }
    };

    let report = match audit::audit(&ledger) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("the audit could not complete: {e}");
            std::process::exit(2);
        }
    };

    if !quiet || !report.is_clean() {
        println!("{}", report.summary());
    }
    if report.is_clean() {
        // Say what was checked even on success. "Clean" over zero records
        // is a very different statement from "clean" over ten thousand,
        // and an operator reading a cron mail deserves to be able to tell
        // them apart.
        if !quiet {
            println!("\nclean");
        }
        return;
    }

    println!();
    for f in &report.findings {
        println!("FINDING: {f}");
    }
    println!(
        "\n{} finding(s). These are reported, never repaired: recovery is an\n\
         operator decision made with `glc-admin` so it lands in the audit trail\n\
         (docs/09-runbook.md).",
        report.findings.len()
    );
    std::process::exit(1);
}
