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

/// Re-opens admission for `direction` after three independent safety
/// checks, none weakening the others, all refusing unconditionally
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
/// 3. The confirmed-liquidity admission buffer
///    ([`Ledger::check_liquidity_buffer_for_admission`],
///    docs/09-runbook.md's "Confirmed-liquidity admission safety
///    buffer") — while the automatic gate is still closed, clearing the
///    operator flag would change nothing observable: every new fold
///    would keep parking. Refusing with the headroom and the reopen
///    threshold in the message is strictly more useful than a
///    successful command that silently does nothing.
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

/// Re-opens ONE route's own admission gate, behind the SAME three
/// safety checks [`open_admission_guarded`] applies — deliberately
/// reusing them rather than restating a subset.
///
/// # Why the identical guards, on a narrower switch
///
/// Opening a route gate is opening admission: with the reserve-wide
/// switches already open, this one write is the difference between a
/// newly observed deposit parking in `ManualReview` and being admitted
/// against real reserve capacity. A route-scoped command with weaker
/// checks than the reserve-scoped one would therefore be a way to route
/// around `open-admission`'s guards entirely — close the reserve-wide
/// switch, open the route, and admit onto a broken invariant or a thin
/// UTXO pool that `open-admission` would have refused.
///
/// So the checks are not merely similar, they are the same function
/// calls against the route's own destination reserve:
///
/// 1. the hard reserve invariant ([`Ledger::check_invariant`]),
/// 2. the count-based mature-UTXO floor
///    ([`Ledger::check_utxo_liquidity_for_admission`]),
/// 3. the confirmed-liquidity admission buffer
///    ([`Ledger::check_liquidity_buffer_for_admission`]).
///
/// Closing has no guard, exactly as closing the reserve-wide switch has
/// none: refusing to close is refusing to stop taking deposits.
///
/// # What this does NOT do
///
/// It does not touch the reserve-wide `paused` or `admission_closed`
/// flags, and it cannot. A route whose reserve is paused stays closed
/// after this returns `Ok(())` — the two gates are ANDed by
/// [`crate::ledger::InboundAdmissionGates`] and neither can clear the
/// other. Nor does it touch enablement
/// ([`crate::routes::RouteGate`]), which is a separate axis.
pub fn open_route_admission_guarded(
    ledger: &mut Ledger,
    route: crate::routes::Route,
    note: &str,
) -> Result<(), OpenAdmissionError> {
    // The reserve this route settles out of — the same one its folds
    // gate against, so the guards below examine the pool that a
    // re-opened route would actually draw on. A route with no
    // `Direction` has no destination reserve and no admission gate;
    // `Ledger::set_route_admission` refuses it below, inside the audited
    // scope, so this returns the refusal rather than guessing a reserve.
    let Some(direction) = route.as_direction() else {
        return ledger
            .set_route_admission(route, false, Some(note))
            .map_err(OpenAdmissionError::Ledger);
    };
    let reserve = direction.destination_reserve();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    ledger.check_invariant(reserve).map_err(|e| {
        OpenAdmissionError::Refused(format!(
            "refusing to open admission for route {}: {reserve:?}'s reserve invariant does not \
             hold ({e})",
            route.as_str()
        ))
    })?;
    ledger
        .check_utxo_liquidity_for_admission(reserve, now)
        .map_err(|e: LedgerError| {
            OpenAdmissionError::Refused(format!(
                "refusing to open admission for route {}: {e}",
                route.as_str()
            ))
        })?;
    ledger
        .check_liquidity_buffer_for_admission(reserve, now)
        .map_err(|e: LedgerError| {
            OpenAdmissionError::Refused(format!(
                "refusing to open admission for route {}: {e}",
                route.as_str()
            ))
        })?;
    ledger
        .set_route_admission(route, false, Some(note))
        .map_err(OpenAdmissionError::Ledger)
}
