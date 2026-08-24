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
    /// Cumulative bridge-fee revenue accrued on this direction's row, in
    /// canonical units (docs/20-bridge-fee.md) — reported for audit
    /// visibility only; never counted toward `invariant_holds` or any
    /// available-capacity figure (`reserved_liquidity`/
    /// `pending_obligations` already track NET, post-fee amounts only, so
    /// fee revenue was never customer-obligation capacity to begin with).
    pub accrued_fees: u64,
    /// Value the Goldcoin vault holds in UTXOs that exist on-chain but
    /// have not yet reached `vault_min_confirmations` — e.g. a payout's
    /// own change output, still maturing. Already excluded from
    /// `total_reserve_balance` (see [`Ledger::immature_vault_utxo_total`]
    /// docs) and from every invariant/pause decision; reported here purely
    /// so an operator can see whether a low/paused reserve is already
    /// self-resolving. Always `0` for `SolanaReserve`, which has no
    /// UTXO-maturity concept.
    pub immature_vault_utxo_total: u64,
    pub paused: bool,
    /// Whether NEW obligations are currently admitted for this direction —
    /// a separate axis from `paused` (see [`Ledger::set_admission`]/
    /// `docs/09-runbook.md`'s "Admission control (Solana->Goldcoin)"
    /// section). Never auto-set; only an explicit `glc-admin
    /// close-admission`/`open-admission` call changes it.
    pub admission_closed: bool,
    /// `total_reserve_balance >= protected_minimum + reserved_liquidity`
    /// (docs/05-reserve-accounting.md's hard invariant) — see
    /// [`Ledger::check_invariant`].
    pub invariant_holds: bool,
}

pub fn check(ledger: &Ledger, direction: ReserveDirection) -> Result<ReserveSnapshot, LedgerError> {
    let (total_reserve_balance, protected_minimum, reserved_liquidity, pending_obligations) =
        ledger.reserve_snapshot(direction)?;
    let accrued_fees = ledger.accrued_fees(direction)?;
    let immature_vault_utxo_total = match direction {
        ReserveDirection::GoldcoinReserve => ledger.immature_vault_utxo_total()?,
        ReserveDirection::SolanaReserve => 0,
    };
    let paused = ledger.is_paused(direction)?;
    let admission_closed = ledger.is_admission_closed(direction)?;
    let invariant_holds = ledger.check_invariant(direction).is_ok();
    Ok(ReserveSnapshot {
        direction,
        total_reserve_balance,
        protected_minimum,
        reserved_liquidity,
        pending_obligations,
        accrued_fees,
        immature_vault_utxo_total,
        paused,
        admission_closed,
        invariant_holds,
    })
}

#[cfg(test)]
mod tests;
