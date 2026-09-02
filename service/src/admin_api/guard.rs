//! The open-admission safety gate, extracted from `glc-admin
//! open-admission` so the CLI and the admin API run the IDENTICAL checks
//! — one implementation, two callers, no way for the two paths to drift.

use crate::ledger::{Ledger, LedgerError, ReserveDirection};

/// Why [`open_admission_guarded`] did not open admission. The two cases
/// are deliberately distinct so the HTTP layer can keep its error
/// policy: a `Refused` is a validated, operator-facing safety refusal
/// (409 with the message verbatim — the same text `glc-admin` prints),
/// while `Ledger` is a storage failure that must go through
/// `AdminError::from(LedgerError)`'s redaction (a raw SQLite message can
/// embed the database path) and be reported as the 500 it is, never
/// dressed up as a refusal.
#[derive(Debug)]
pub enum OpenAdmissionError {
    Refused(String),
    Ledger(LedgerError),
}

impl std::fmt::Display for OpenAdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenAdmissionError::Refused(message) => f.write_str(message),
            OpenAdmissionError::Ledger(e) => write!(f, "{e}"),
        }
    }
}

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
/// 3. The confirmed-liquidity safety buffer
///    ([`Ledger::check_liquidity_buffer_for_admission`]) — refuses while
///    confirmed unreserved headroom is still below the REOPEN threshold.
///    Deliberately the reopen threshold, not the close one: an operator
///    reopening by hand lands on the same side of the hysteresis band an
///    automatic reopen would, so a manual open cannot be used to slip
///    admission back on inside the band it exists to sit out.
///
/// Only on all three passing does it call [`Ledger::set_admission`].
pub fn open_admission_guarded(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    note: &str,
) -> Result<(), OpenAdmissionError> {
    ledger.check_invariant(direction).map_err(|e| {
        OpenAdmissionError::Refused(format!(
            "refusing to open admission: {direction:?}'s reserve invariant does not hold ({e})"
        ))
    })?;
    ledger
        .check_utxo_liquidity_for_admission(
            direction,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        )
        .map_err(|e: LedgerError| {
            OpenAdmissionError::Refused(format!("refusing to open admission: {e}"))
        })?;
    ledger
        .check_liquidity_buffer_for_admission(
            direction,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        )
        .map_err(|e: LedgerError| {
            OpenAdmissionError::Refused(format!("refusing to open admission: {e}"))
        })?;
    ledger
        .set_admission(direction, false, Some(note))
        .map_err(OpenAdmissionError::Ledger)
}
