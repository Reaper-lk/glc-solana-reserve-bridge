//! The open-admission safety gate, extracted from `glc-admin
//! open-admission` so the CLI and the admin API run the IDENTICAL checks
//! — one implementation, two callers, no way for the two paths to drift.

use crate::ledger::{Ledger, LedgerError, ReserveDirection};

/// Re-opens admission for `direction` after two independent safety
/// checks, neither weakening the other, both refusing unconditionally
/// (no override exists):
///
/// 1. The hard reserve invariant ([`Ledger::check_invariant`]) — so
///    admission is never re-opened onto an already-broken reserve.
/// 2. The same count-based UTXO-liquidity gate `fold_sol_deposit`
///    applies to a brand-new obligation
///    ([`Ledger::check_utxo_liquidity_for_admission`],
///    docs/09-runbook.md's "UTXO liquidity" section) — reopening
///    admission onto a mature UTXO pool still at or below the configured
///    floor would immediately re-admit exactly the demand backpressure
///    exists to hold back.
///
/// Only on both passing does it call [`Ledger::set_admission`]. The
/// error is the operator-facing refusal message (the same text
/// `glc-admin` prints).
pub fn open_admission_guarded(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    note: &str,
) -> Result<(), String> {
    ledger.check_invariant(direction).map_err(|e| {
        format!("refusing to open admission: {direction:?}'s reserve invariant does not hold ({e})")
    })?;
    ledger
        .check_utxo_liquidity_for_admission(direction)
        .map_err(|e: LedgerError| format!("refusing to open admission: {e}"))?;
    ledger
        .set_admission(direction, false, Some(note))
        .map_err(|e| e.to_string())
}
