//! Reserve-imbalance detection and rebalance planning
//! (docs/22-production-readiness-review.md P1 "rebalancing",
//! docs/09-runbook.md's "Threshold bands and responses" table). Pure,
//! read-only: [`assess`] never writes anything and never itself proposes a
//! [`crate::ledger::RebalanceRequest`] — it only classifies the current
//! situation and, where policy allows it, suggests a size for one. Turning
//! a suggestion into an actual, approvable request is always a separate,
//! explicit `Ledger::propose_rebalance` call an operator (or a future
//! automated policy, not built here) makes deliberately, never a side
//! effect of merely checking status.
//!
//! # Never invents a production threshold
//!
//! Every number this module reasons about — `target_reserve`,
//! `warning_reserve`, `critical_reserve`, `protected_minimum` — is read
//! live from `reserve_ledger`, i.e. whatever the operator already
//! configured (`Ledger::configure_reserve`, docs/12-management-decisions.md
//! item 5). This module adds no new policy value of its own; it only
//! applies the bands the operator already chose.

use crate::ledger::{Ledger, LedgerError, ReserveDirection};

/// Mirrors docs/09-runbook.md's "Threshold bands and responses" table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImbalanceSeverity {
    /// `balance >= warning_reserve`. No action suggested.
    Normal,
    /// `critical_reserve <= balance < warning_reserve`. A top-up is worth
    /// planning; not urgent.
    Warning,
    /// `balance < critical_reserve` (but `>= protected_minimum`). The
    /// reserve-health invariant (`ops::reserve_health`,
    /// `reconciliation::reconcile`) may already be triggering an automatic
    /// pause for this direction independently of anything in this module —
    /// see those modules for the actual pause mechanism. This severity is
    /// a read-only classification, not itself a trigger.
    Critical,
}

/// A read-only snapshot of one reserve direction's health against its own
/// configured bands, plus (only in the `Warning`/`Critical` case) a
/// suggested rebalance size to restore `target_reserve` — sizing
/// information only, never a decision to act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImbalanceAssessment {
    pub direction: ReserveDirection,
    pub severity: ImbalanceSeverity,
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub target_reserve: u64,
    pub warning_reserve: u64,
    pub critical_reserve: u64,
    /// `Some(target_reserve - total_reserve_balance)` when `severity` is
    /// `Warning` or `Critical` and `target_reserve > total_reserve_balance`
    /// — a `Deposit` of this size into `direction` would restore the
    /// operator's own configured target. `None` when `Normal`, or in the
    /// (unusual, but not asserted against here) case where
    /// `target_reserve` has been configured at or below the current
    /// balance despite a Warning/Critical classification.
    pub suggested_deposit_atomic: Option<u64>,
}

/// Assesses `direction` against its own currently-configured thresholds.
/// Read-only — does not create, approve, or affect any
/// [`crate::ledger::RebalanceRequest`].
pub fn assess(
    ledger: &Ledger,
    direction: ReserveDirection,
) -> Result<ImbalanceAssessment, LedgerError> {
    let (
        total_reserve_balance,
        protected_minimum,
        target_reserve,
        warning_reserve,
        critical_reserve,
    ) = ledger.reserve_thresholds(direction)?;

    let severity = if total_reserve_balance < critical_reserve {
        ImbalanceSeverity::Critical
    } else if total_reserve_balance < warning_reserve {
        ImbalanceSeverity::Warning
    } else {
        ImbalanceSeverity::Normal
    };

    let suggested_deposit_atomic = match severity {
        ImbalanceSeverity::Normal => None,
        ImbalanceSeverity::Warning | ImbalanceSeverity::Critical => target_reserve
            .checked_sub(total_reserve_balance)
            .filter(|d| *d > 0),
    };

    Ok(ImbalanceAssessment {
        direction,
        severity,
        total_reserve_balance,
        protected_minimum,
        target_reserve,
        warning_reserve,
        critical_reserve,
        suggested_deposit_atomic,
    })
}

/// Convenience: assesses both reserve directions.
pub fn assess_both(
    ledger: &Ledger,
) -> Result<(ImbalanceAssessment, ImbalanceAssessment), LedgerError> {
    Ok((
        assess(ledger, ReserveDirection::GoldcoinReserve)?,
        assess(ledger, ReserveDirection::SolanaReserve)?,
    ))
}

#[cfg(test)]
mod tests;
