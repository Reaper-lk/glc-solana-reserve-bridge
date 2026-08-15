//! Continuous reconciliation between the ledger's expected state and
//! observed chain state (docs/03-architecture.md, docs/09-runbook.md).
//!
//! # What this checks, per reserve direction
//!
//! - **Hard solvency invariant** (constraint: `available reserves >= all
//!   releases that can currently become payable`): `observed_balance >=
//!   protected_minimum + pending_obligations`. `pending_obligations` are
//!   irreversible commitments (requests past `SourceFinalized` —
//!   docs/05-reserve-accounting.md); if the live balance can't cover them
//!   plus the protected floor, that is always a breach regardless of any
//!   tolerance, full stop.
//! - **Unexplained balance movement**: `observed_balance` vs. what the
//!   ledger last cached. A drop beyond `tolerance` is a breach — this
//!   phase has no code path that releases reserve funds yet (no signing
//!   client, Phase 4+), so ANY unexplained drop is presumptively
//!   unauthorized/anomalous, never assumed benign.
//!
//! # Fail-closed contract
//!
//! `reconcile` takes an ALREADY-OBSERVED `u64` balance — there is no
//! "unknown"/`Option` input, by design: if the caller could not get a real
//! chain read (RPC failure, stale height, insufficient confirmations), it
//! must not call `reconcile` with a guessed or stale value. Call
//! [`record_skipped`] instead, so the skip itself is auditable rather than
//! silently absent (constraint: never silently treat an RPC failure or
//! unknown chain state as success).
//!
//! # Pause is one-directional here
//!
//! A breach triggers [`Ledger::set_paused`]`(direction, true, ...)`.
//! Reconciliation NEVER calls `set_paused(direction, false, ...)` —
//! un-pausing is always operator-controlled (docs/09-runbook.md's
//! asymmetric pause design: fast/automatic to pause, slow/manual to
//! resume), so a transient recovery in the observed balance does not
//! silently resume settlement.

use crate::ledger::{Ledger, LedgerError, ReserveDirection};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    WithinTolerance,
    /// Reserved for a later phase once real settlement broadcasts are
    /// tracked and their amounts can be subtracted before classifying a
    /// delta (docs/05-reserve-accounting.md's `in_flight` tolerance). Not
    /// yet produced by this phase's logic — see module docs.
    InFlightExplained,
    Breach,
}

#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    pub direction: ReserveDirection,
    pub observed_balance: u64,
    pub cached_balance_before: u64,
    pub protected_minimum: u64,
    pub reserved_liquidity: u64,
    pub pending_obligations: u64,
    /// Cumulative bridge-fee revenue accrued on this direction's row, in
    /// canonical units (`amount_conversion::CanonicalAtomic`,
    /// docs/20-bridge-fee.md) — reported here purely for audit visibility.
    /// Never subtracted from or otherwise mixed into `observed_balance`/
    /// `pending_obligations`/the solvency check above: `reserved_liquidity`
    /// and `pending_obligations` already track NET (post-fee) amounts
    /// only, so fee revenue was never counted as customer-obligation
    /// capacity in the first place and this field cannot silently inflate
    /// what looks available.
    pub accrued_fees: u64,
    pub classification: Classification,
    pub auto_paused: bool,
}

/// Reconciles one reserve direction against an already-observed live
/// balance. See module docs for the fail-closed contract and the
/// never-auto-unpause rule.
pub fn reconcile(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    observed_balance: u64,
    tolerance: u64,
    now: i64,
) -> Result<ReconciliationReport, LedgerError> {
    let (cached_balance_before, protected_minimum, reserved_liquidity, pending_obligations) =
        ledger.reserve_snapshot(direction)?;
    let accrued_fees = ledger.accrued_fees(direction)?;

    let hard_invariant_holds = observed_balance >= protected_minimum + pending_obligations;

    let delta: i64 = observed_balance as i64 - cached_balance_before as i64;
    let unexplained_drop = delta < 0 && (-delta) as u64 > tolerance;

    let classification = if !hard_invariant_holds || unexplained_drop {
        Classification::Breach
    } else {
        Classification::WithinTolerance
    };

    let auto_paused = classification == Classification::Breach;
    if auto_paused {
        let reason = if !hard_invariant_holds {
            format!(
                "hard invariant breach: observed_balance {observed_balance} < protected_minimum \
                 {protected_minimum} + pending_obligations {pending_obligations}"
            )
        } else {
            format!(
                "unexplained balance drop: {cached_balance_before} -> {observed_balance} \
                 (delta {delta}, tolerance {tolerance})"
            )
        };
        ledger.set_paused(direction, true, Some(&reason))?;
    }

    ledger.refresh_reserve_balance(direction, observed_balance, now)?;
    ledger.record_reconciliation_finding(
        direction,
        cached_balance_before as i64,
        observed_balance as i64,
        delta,
        classification_str(classification),
        auto_paused,
        now,
    )?;

    Ok(ReconciliationReport {
        direction,
        observed_balance,
        cached_balance_before,
        protected_minimum,
        reserved_liquidity,
        pending_obligations,
        accrued_fees,
        classification,
        auto_paused,
    })
}

/// Records that reconciliation was SKIPPED this tick (RPC failure, unknown
/// chain state, stale height, insufficient confirmations) rather than
/// silently doing nothing — the skip itself must be auditable.
pub fn record_skipped(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    reason: &str,
    now: i64,
) -> Result<(), LedgerError> {
    ledger.record_reconciliation_finding(
        direction,
        0,
        0,
        0,
        &format!("SKIPPED: {reason}"),
        false,
        now,
    )
}

fn classification_str(c: Classification) -> &'static str {
    match c {
        Classification::WithinTolerance => "WITHIN_TOLERANCE",
        Classification::InFlightExplained => "IN_FLIGHT_EXPLAINED",
        Classification::Breach => "BREACH",
    }
}

#[cfg(test)]
mod tests;
