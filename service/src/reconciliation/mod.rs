//! Continuous reconciliation between the ledger's expected state and
//! observed chain state (docs/03-architecture.md, docs/09-runbook.md).
//!
//! # What this checks, per reserve direction
//!
//! - **Hard solvency invariant** (constraint: `available reserves >= all
//!   releases that can currently become payable`): `observed_balance +
//!   own_unconfirmed_change_atomic >= protected_minimum +
//!   pending_obligations`. `pending_obligations` are irreversible
//!   commitments (requests past `SourceFinalized` —
//!   docs/05-reserve-accounting.md); if the live balance, plus value
//!   already KNOWN (not guessed — see
//!   `Ledger::own_unconfirmed_change_atomic`'s docs) to be this service's
//!   own broadcast-but-immature payout change, still can't cover them plus
//!   the protected floor, that is always a breach regardless of any
//!   tolerance, full stop. `own_unconfirmed_change_atomic` is always `0`
//!   for `SolanaReserve`, which has no UTXO-maturity concept, and is
//!   grounded entirely in independently-observed chain state matched
//!   against this service's own already-broadcast payouts — it can never
//!   paper over an actual, unexplained loss (docs/09-runbook.md's "UTXO
//!   liquidity" section: this is the fix for a real incident where a
//!   temporarily-illiquid-but-fully-accounted-for reserve was
//!   misclassified as a breach and auto-paused).
//! - **Unexplained balance movement**: `observed_balance` vs. what the
//!   ledger last cached. A drop is first reduced by whatever amount is
//!   genuinely explained by settlements already broadcast to this
//!   reserve's chain but not yet folded into `Settled` bookkeeping
//!   (`Ledger::pending_destination_settlement_amount` —
//!   `Classification::InFlightExplained`); any *residual* drop beyond
//!   `tolerance` is a breach — presumptively unauthorized/anomalous, never
//!   assumed benign.
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
    /// An observed drop was real but is fully or partially explained by
    /// settlements already broadcast to this reserve's chain and not yet
    /// folded into `Settled` bookkeeping — see
    /// `Ledger::pending_destination_settlement_amount` and `reconcile`'s
    /// use of it (docs/05-reserve-accounting.md's `in_flight` tolerance).
    /// Never auto-paused: any *residual*, unexplained portion beyond
    /// tolerance still classifies as `Breach` and pauses as normal.
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
    /// Value already known to be this service's own broadcast-but-immature
    /// payout change (`Ledger::own_unconfirmed_change_atomic`) — folded
    /// into the hard invariant check below (never into
    /// `observed_balance`/`total_reserve_balance` themselves, which stay
    /// the true mature-only figures reported elsewhere). Always `0` for
    /// `SolanaReserve`.
    pub own_unconfirmed_change_atomic: u64,
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

    // Known, ledger-tracked value temporarily illiquid in this service's
    // own broadcast-but-immature payout change is not missing — it is
    // fully accounted for and will become spendable once it matures. The
    // hard invariant must not treat it as if it had vanished (the exact
    // production incident this fixes: a mature pool that drains while
    // equivalent value sits in known internal change), but genuine,
    // unexplained loss is untouched by this term — it is derived solely
    // from independently-observed `vault_utxos` state matched against this
    // service's own already-broadcast `goldcoin_payouts`, never from a
    // self-reported or otherwise attacker-influenceable figure.
    let own_unconfirmed_change_atomic = match direction {
        ReserveDirection::GoldcoinReserve => ledger.own_unconfirmed_change_atomic()?,
        ReserveDirection::SolanaReserve => 0,
    };
    let effective_balance_for_invariant =
        observed_balance.saturating_add(own_unconfirmed_change_atomic);
    let hard_invariant_holds =
        effective_balance_for_invariant >= protected_minimum + pending_obligations;

    let delta: i64 = observed_balance as i64 - cached_balance_before as i64;
    let raw_drop = if delta < 0 { (-delta) as u64 } else { 0 };

    // A drop can be legitimately explained, up to the amount actually
    // pending, by settlements this service has already broadcast to
    // `direction`'s chain but not yet folded into `Settled` bookkeeping —
    // never more than that real, currently-pending figure. This is a
    // SEPARATE mechanism from `own_unconfirmed_change_atomic` above: that
    // term redefines what "available" truly means for the hard invariant
    // (known-safe value is never "missing" in the first place); this term
    // only ever softens the UNEXPLAINED-DROP check below, and only up to
    // the real, currently-pending settlement amount.

    let in_flight_amount = ledger.pending_destination_settlement_amount(direction)?;
    let explained_by_in_flight = raw_drop.min(in_flight_amount);
    let residual_drop = raw_drop - explained_by_in_flight;
    let unexplained_drop = residual_drop > tolerance;

    let classification = if !hard_invariant_holds || unexplained_drop {
        Classification::Breach
    } else if explained_by_in_flight > 0 {
        Classification::InFlightExplained
    } else {
        Classification::WithinTolerance
    };

    let auto_paused = classification == Classification::Breach;
    if auto_paused {
        let reason = if !hard_invariant_holds {
            format!(
                "hard invariant breach: observed_balance {observed_balance} + \
                 own_unconfirmed_change_atomic {own_unconfirmed_change_atomic} = \
                 {effective_balance_for_invariant} < protected_minimum {protected_minimum} + \
                 pending_obligations {pending_obligations}"
            )
        } else {
            format!(
                "unexplained balance drop: {cached_balance_before} -> {observed_balance} \
                 (delta {delta}, tolerance {tolerance}, explained_by_in_flight \
                 {explained_by_in_flight}, residual {residual_drop})"
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
        own_unconfirmed_change_atomic,
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
