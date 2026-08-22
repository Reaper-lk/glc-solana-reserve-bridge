//! Off-chain enforcement of the on-chain rolling-24h-volume quota's
//! auto-pause consequence (docs/09-runbook.md's "Auto-pause triggers"
//! table: "Rolling volume limit exceeded | Directional | Anomaly/attack
//! containment").
//!
//! # Why this exists, and what it does not do
//!
//! The rolling-volume quota itself is enforced ON-CHAIN
//! (`programs/glc-reserve-bridge/src/limits.rs::
//! enforce_and_record_rolling_volume`), on every real `release_from_
//! reserve`/`deposit_to_reserve` — this module never touches that check
//! and cannot weaken or bypass it; it does not read/write anything the
//! on-chain program itself doesn't already track. What it adds is purely
//! an OFF-CHAIN consequence: when this service's own periodic tick
//! observes a direction's window fully exhausted, it engages this
//! service's own local ledger pause (`Ledger::set_paused`) for that
//! direction — the same gate `reconciliation::reconcile` already uses
//! for a balance breach, applied here to quota exhaustion instead.
//!
//! # Never auto-unpauses, mirroring `reconciliation::reconcile` exactly
//!
//! This module NEVER calls `set_paused(direction, false, ...)` — not on
//! this tick, not on any later tick, not even once the on-chain window's
//! own fixed-bucket reset makes fresh quota available again. That reset
//! is real and automatic (`accounts::rolling_volume_remaining`'s own
//! docs), but it only lifts the ON-CHAIN quota check; it says nothing
//! about whether reserves have actually been reconciled/replenished to
//! an operator's satisfaction. Un-pausing this service's own local gate
//! stays exactly what it has always been: operator-controlled, via
//! `glc-admin unpause --direction <goldcoin|solana> --note TEXT` (or the
//! on-chain `glc-admin onchain-unpause --scope <release|deposit>` circuit
//! breaker, if that was engaged too) — see docs/09-runbook.md's
//! 2026-08-22 update for the full workflow and the exact end-user copy
//! shown for a paused/exhausted direction meanwhile
//! (`api::DIRECTION_UNAVAILABLE_MESSAGE`).

use crate::ledger::{Ledger, LedgerError, ReserveDirection};
use crate::solana::accounts::{self, RollingVolumeWindowSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaReport {
    pub direction: ReserveDirection,
    pub remaining: u64,
    /// `remaining < min_transfer_amount` — no further transfer of any
    /// legal size could succeed right now.
    pub quota_exhausted: bool,
    pub auto_paused: bool,
}

/// Checks one direction's already-observed on-chain
/// [`RollingVolumeWindowSnapshot`] against the current `rolling_volume_
/// limit`, and engages this service's own local pause if exhausted.
///
/// `direction` must be the RESERVE direction the checked window's
/// direction BYTE actually corresponds to — release (byte 0, `GlcToSol`)
/// -> [`ReserveDirection::SolanaReserve`], deposit (byte 1, `SolToGlc`)
/// -> [`ReserveDirection::GoldcoinReserve`] — callers own getting this
/// pairing right (mirrors `api::status`'s existing pairing).
///
/// Fail-closed contract mirrors `reconciliation::reconcile` exactly:
/// takes an ALREADY-OBSERVED window, never fetches anything itself, so a
/// caller that could not get a real chain read must not call this with a
/// guessed/stale window.
pub fn enforce_rolling_volume_quota(
    ledger: &mut Ledger,
    direction: ReserveDirection,
    rolling_volume_limit: u64,
    rolling_window_seconds: i64,
    min_transfer_amount: u64,
    window: RollingVolumeWindowSnapshot,
    now: i64,
) -> Result<QuotaReport, LedgerError> {
    let remaining = accounts::rolling_volume_remaining(
        rolling_volume_limit,
        rolling_window_seconds,
        window,
        now,
    );
    let quota_exhausted = remaining < min_transfer_amount;

    if quota_exhausted {
        let reason = format!(
            "rolling-24h-volume quota exhausted: remaining {remaining} < min_transfer_amount \
             {min_transfer_amount} (rolling_volume_limit {rolling_volume_limit}) — auto-paused \
             by this service's local admission gate; the on-chain quota itself resets on its \
             own at the next rolling window boundary, but this local pause does not — an \
             operator must explicitly unpause after confirming reserves are ready \
             (docs/09-runbook.md)"
        );
        ledger.set_paused(direction, true, Some(&reason))?;
    }

    Ok(QuotaReport {
        direction,
        remaining,
        quota_exhausted,
        auto_paused: quota_exhausted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::solana::accounts::RollingVolumeWindowSnapshot;

    fn window(window_start: i64, window_total: u64) -> RollingVolumeWindowSnapshot {
        RollingVolumeWindowSnapshot {
            direction: 0,
            window_start,
            window_total,
        }
    }

    fn configured_ledger(dir: &std::path::Path) -> Ledger {
        let db_path = dir.join("ledger.sqlite3");
        let mut ledger = Ledger::open(&db_path).unwrap();
        for direction in [
            ReserveDirection::GoldcoinReserve,
            ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(direction, 10_000_000, 0, 5_000_000, 2_000_000, 1_000_000, 0)
                .unwrap();
        }
        ledger
    }

    #[test]
    fn does_not_pause_while_headroom_remains() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = configured_ledger(dir.path());
        let report = enforce_rolling_volume_quota(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            2_000_000,
            3_600,
            100,
            window(1_000, 500_000),
            1_050,
        )
        .unwrap();
        assert!(!report.quota_exhausted);
        assert!(!report.auto_paused);
        assert_eq!(report.remaining, 1_500_000);
        assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
    }

    #[test]
    fn auto_pauses_the_affected_direction_only_when_quota_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = configured_ledger(dir.path());
        let report = enforce_rolling_volume_quota(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            2_000_000,
            3_600,
            100,
            window(1_000, 2_000_000),
            1_050,
        )
        .unwrap();
        assert!(report.quota_exhausted);
        assert!(report.auto_paused);
        assert_eq!(report.remaining, 0);
        assert!(
            ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
            "the affected direction must be auto-paused"
        );
        assert!(
            !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
            "the opposite, untouched direction must remain unaffected"
        );
    }

    #[test]
    fn a_fresh_bucket_reset_reports_no_exhaustion_even_with_a_high_prior_total() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = configured_ledger(dir.path());
        // window_start far enough in the past that bucket_age >=
        // rolling_window_seconds: the old total no longer applies.
        let report = enforce_rolling_volume_quota(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            2_000_000,
            3_600,
            100,
            window(0, 2_000_000),
            3_600,
        )
        .unwrap();
        assert!(!report.quota_exhausted);
        assert_eq!(report.remaining, 2_000_000);
        assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
    }

    /// The defining invariant this whole module exists to provide: once
    /// auto-paused, a later tick observing the SAME still-exhausted
    /// window must never itself lift the pause — only an explicit
    /// operator `unpause` can, and that path is entirely outside this
    /// function (it never calls `set_paused(direction, false, ...)`,
    /// checked directly: no such call appears in this module at all).
    #[test]
    fn never_auto_unpauses_across_repeated_ticks_of_continued_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = configured_ledger(dir.path());
        for now in [1_050, 1_100, 1_200] {
            enforce_rolling_volume_quota(
                &mut ledger,
                ReserveDirection::SolanaReserve,
                2_000_000,
                3_600,
                100,
                window(1_000, 2_000_000),
                now,
            )
            .unwrap();
        }
        assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());

        // Simulate an operator's own, unrelated explicit unpause — this
        // function does not do this itself; it's asserted here only to
        // demonstrate that, unlike this module's own calls, an explicit
        // unpause is a completely separate act, not something quota
        // exhaustion alone ever triggers.
        ledger
            .set_paused(ReserveDirection::SolanaReserve, false, Some("operator"))
            .unwrap();
        assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
    }
}
