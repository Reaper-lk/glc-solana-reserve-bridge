//! Reserve-model equivalent of the old bridge's `ops/solvency.rs`
//! (docs/01-reuse-inventory.md: the *shape* — continuously compare a
//! DB-derived running total against the invariant it must satisfy, zero
//! tolerance for unexplained slack — is exactly what reserve health needs;
//! the *formula* is rewritten because there is no `wrapped_supply`/mint
//! concept in this reserve-backed design). Wraps
//! [`crate::ledger::Ledger`]'s own already-tested invariant methods rather
//! than re-implementing the check — this module's only job is to shape
//! that answer for [`crate::ops::health`]/[`crate::ops::collector`].

use crate::ledger::{Ledger, LedgerError, ReserveDirection};

/// One reserve direction's health, at the moment it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveSnapshot {
    pub direction: ReserveDirection,
    pub total_reserve_balance: u64,
    pub protected_minimum: u64,
    pub reserved_liquidity: u64,
    pub pending_obligations: u64,
    pub paused: bool,
    /// `total_reserve_balance >= protected_minimum + reserved_liquidity`
    /// (docs/05-reserve-accounting.md's hard invariant) — see
    /// [`Ledger::check_invariant`].
    pub invariant_holds: bool,
}

pub fn check(ledger: &Ledger, direction: ReserveDirection) -> Result<ReserveSnapshot, LedgerError> {
    let (total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations) =
        ledger.reserve_snapshot(direction)?;
    let paused = ledger.is_paused(direction)?;
    let invariant_holds = ledger.check_invariant(direction).is_ok();
    Ok(ReserveSnapshot {
        direction,
        total_reserve_balance,
        protected_minimum,
        reserved_liquidity,
        pending_obligations,
        paused,
        invariant_holds,
    })
}

#[cfg(test)]
mod tests;
