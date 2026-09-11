//! The Robinhood indexer's own tick loop.
//!
//! # Why this is a separate loop and not a phase of `daemon::run`
//!
//! [`crate::daemon::run`] drives [`crate::orchestrator::Orchestrator`],
//! which is generic over the Goldcoin and Solana RPC types and whose
//! every phase belongs to settlement — reservations, payouts, signing,
//! reconciliation. Threading a third chain through it would have meant a
//! third generic parameter on the orchestrator and a `TickReport` field
//! on every existing tick, for a chain that settles nothing.
//!
//! More importantly it would have COUPLED them. A Robinhood endpoint
//! being down would have shared the settlement loop's backoff, and a
//! Robinhood halt would have sat inside the same report an operator reads
//! for live traffic. Running as its own task keeps the blast radius where
//! it belongs: the Robinhood indexer can be stopped, halted, or absent
//! entirely without the Solana<->Goldcoin loop noticing.
//!
//! # Absent, not disabled
//!
//! Nothing spawns this unless `[robinhood.indexer]` is configured. With
//! no config there is no task, no client, and no socket — see
//! [`super`]'s module docs.
//!
//! # What it borrows from `daemon::run`
//!
//! The two behaviours that loop has and a bare `loop { tick; sleep }`
//! does not, for the same reasons recorded there:
//!
//! 1. **Backoff on a failing endpoint**, doubling per consecutive failed
//!    tick up to a ceiling, reset by any successful tick. Hammering a
//!    downed endpoint at the normal rate helps nobody.
//! 2. **Shutdown between ticks, never mid-tick.** A tick that has started
//!    always finishes: each chunk is committed atomically, so an
//!    interrupted tick is safe, but there is no correctness reason to
//!    interrupt one and doing so would add a path nothing else needs.
//!
//! A halted indexer keeps ticking at the base interval on purpose. Its
//! ticks are then a single cheap ledger read that re-publishes the halt
//! to the health state, which is what keeps a stopped indexer VISIBLE
//! rather than merely stopped — the exact failure `ops::indexer_status`
//! exists to prevent.

use std::time::Duration;

use super::indexer::{RobinhoodIndexer, RobinhoodTickOutcome};
use super::rpc::EvmRpc;

#[derive(Debug, Clone, Copy)]
pub struct RobinhoodLoopConfig {
    pub tick_interval: Duration,
    /// Ceiling for the backoff. Never applied to a successful tick.
    pub max_backoff: Duration,
}

/// Drives `indexer.tick()` until `shutdown` reports `true`, then returns
/// how many ticks ran.
///
/// `now` is injected rather than read internally so tests can drive the
/// loop without depending on wall-clock progress — the same choice
/// [`crate::daemon::run`] makes.
pub async fn run<R: EvmRpc>(
    indexer: &mut RobinhoodIndexer<R>,
    config: RobinhoodLoopConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    now: impl Fn() -> i64,
) -> u32 {
    let mut ticks_run = 0u32;
    let mut consecutive_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            return ticks_run;
        }
        let outcome = indexer.tick(now()).await;
        ticks_run += 1;
        consecutive_failures = match &outcome {
            Ok(_) => 0,
            Err(_) => consecutive_failures.saturating_add(1),
        };
        log_tick(ticks_run, &outcome);

        let delay = tick_backoff_delay(
            config.tick_interval,
            config.max_backoff,
            consecutive_failures,
        );
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return ticks_run;
                }
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// `base` for zero consecutive failures, doubling per additional
/// consecutive failed tick, capped at `max`. Identical policy to the one
/// `crate::daemon`'s loop applies, so an operator reading either chain's
/// logs sees the same timing.
fn tick_backoff_delay(base: Duration, max: Duration, consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return base;
    }
    let factor = 2u32.saturating_pow(consecutive_failures.min(16));
    base.saturating_mul(factor).min(max)
}

fn log_tick(n: u32, outcome: &Result<RobinhoodTickOutcome, super::indexer::RobinhoodIndexerError>) {
    match outcome {
        Ok(RobinhoodTickOutcome::Halted { reason, detail }) => {
            // Every tick, not once: a halted indexer that stopped
            // complaining is indistinguishable from one that recovered.
            tracing::error!(
                tick = n,
                reason = reason.as_str(),
                detail = %detail,
                "Robinhood indexer is halted and observing nothing"
            );
        }
        Ok(RobinhoodTickOutcome::Progressed {
            head,
            cursor,
            blocks_scanned,
            recorded,
            already_recorded,
            finalized,
            reorg,
        }) => {
            if let Some(reorg) = reorg {
                tracing::warn!(
                    tick = n,
                    fork_block = reorg.fork_block,
                    old_cursor_block = reorg.old_cursor_block,
                    orphaned_observations = reorg.orphaned_observations,
                    "Robinhood reorg reconciled"
                );
            }
            if *recorded > 0 || *finalized > 0 {
                tracing::info!(
                    tick = n,
                    head,
                    cursor = ?cursor,
                    recorded,
                    finalized,
                    "Robinhood deposits observed — recorded for visibility only; the Robinhood \
                     routes remain disabled and nothing is settled"
                );
            } else {
                tracing::debug!(
                    tick = n,
                    head,
                    cursor = ?cursor,
                    blocks_scanned,
                    already_recorded,
                    "Robinhood tick completed"
                );
            }
        }
        Err(e) => {
            tracing::warn!(tick = n, error = %e, "Robinhood tick failed; will retry");
        }
    }
}

#[cfg(test)]
mod tests;

// =====================================================================
// The settlement loop
// =====================================================================

/// Drives the Robinhood SETTLEMENT engine until `shutdown` reports
/// `true`, then returns how many ticks ran.
///
/// # Why this is a second loop rather than a phase of either existing one
///
/// It is not part of [`run`] because that loop OBSERVES: its RPC bound is
/// [`EvmRpc`] alone, which has no broadcast method, and widening it would
/// give an observation-only component the ability to act.
///
/// It is not part of [`crate::daemon::run`] for the reason that loop's
/// own docs record about the indexer: a Robinhood endpoint being down
/// would otherwise share the Solana<->Goldcoin settlement loop's backoff,
/// and a Robinhood incident would sit inside the report an operator reads
/// for live traffic. Running as its own task keeps the blast radius where
/// it belongs.
///
/// The two loops do interleave on one thing — a `RhnToGlc` request's
/// Goldcoin payout is built by the orchestrator and its settlement is
/// broadcast here — and that is safe because they interleave through the
/// LEDGER rather than through shared memory: every transition either
/// loop performs is a committed SQLite transaction with its own
/// preconditions, so neither can observe a half-applied step of the
/// other.
///
/// # The phase order within a tick
///
/// fold -> authorize -> broadcast -> receipts, and the order matters:
/// running receipts LAST means a transaction broadcast earlier in the
/// same tick gets its first receipt poll on the next one rather than
/// immediately, which is correct — a transaction is never mined in the
/// same instant it is sent, and polling for a receipt that cannot exist
/// yet is a wasted round trip on every single operation.
pub async fn run_settlement<R>(
    settler: &super::settlement::Settler<R>,
    ledger: &mut crate::ledger::Ledger,
    route_open: impl Fn(&crate::ledger::Ledger, crate::routes::Route) -> bool,
    config: RobinhoodLoopConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    now: impl Fn() -> i64,
) -> u64
where
    R: EvmRpc + super::rpc::EvmCallRpc + super::rpc::EvmSubmitRpc,
{
    let mut ticks = 0u64;
    let mut consecutive_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            return ticks;
        }
        let at = now();
        let mut report = super::settlement::SettlementReport::default();

        // The route gate is consulted ONCE per tick, per route, and
        // passed down rather than re-read inside each phase: a gate that
        // changed mid-tick would mean one phase folded a deposit as
        // payable while the next declined to pay it, which is a state
        // neither phase could explain.
        //
        // Per ROUTE, so that closing one cross route stops exactly that
        // route: an `RhnToGlc` deposit still folds and settles while
        // `SolToRhn` is shut, and vice versa. Folding happens either way;
        // the gate decides only whether the resulting request is
        // payable. See `super::fold`'s module docs.
        let open: std::collections::BTreeMap<crate::routes::Route, bool> =
            crate::routes::Route::ALL
                .into_iter()
                .filter(|route| route.contract_route_id().is_some())
                .map(|route| (route, route_open(ledger, route)))
                .collect();
        let is_open = |route: crate::routes::Route| open.get(&route).copied().unwrap_or(false);

        settler.tick_fold(
            ledger,
            is_open(crate::routes::Route::RhnToGlc),
            at,
            &mut report,
        );
        if open.values().any(|open| *open) {
            settler
                .tick_authorize_gated(ledger, &is_open, at, &mut report)
                .await;
            settler.tick_broadcast(ledger, at, &mut report).await;
        }
        // Receipts are polled EVEN WHEN THE ROUTE IS CLOSED. A route
        // closing does not un-broadcast a transaction that is already out
        // there, and refusing to look at it would leave an in-flight
        // operation permanently unresolved — the exact opposite of what
        // closing a route is for.
        settler.tick_receipts(ledger, at, &mut report).await;

        ticks += 1;
        if report.errors.is_empty() {
            consecutive_failures = 0;
        } else {
            consecutive_failures = consecutive_failures.saturating_add(1);
            for error in &report.errors {
                tracing::warn!(error = %error, "Robinhood settlement tick error");
            }
        }
        if report.folded > 0
            || report.authorized > 0
            || report.broadcast > 0
            || report.finalized > 0
        {
            tracing::info!(
                folded = report.folded,
                folded_parked = report.folded_parked,
                authorized = report.authorized,
                broadcast = report.broadcast,
                replaced = report.replaced,
                included = report.included,
                finalized = report.finalized,
                reverted = report.reverted,
                manual_review = report.manual_review,
                "Robinhood settlement tick"
            );
        }
        if report.reverted > 0 || report.manual_review > 0 {
            tracing::error!(
                reverted = report.reverted,
                manual_review = report.manual_review,
                "a Robinhood operation needs a human — it will NOT be retried automatically"
            );
        }

        // Same backoff policy as the observation loop above, for the same
        // reason: hammering a failing endpoint helps nobody.
        let delay = if consecutive_failures == 0 {
            config.tick_interval
        } else {
            config
                .tick_interval
                .saturating_mul(2u32.saturating_pow(consecutive_failures.min(6)))
                .min(config.max_backoff)
        };
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => {}
        }
    }
}

// =====================================================================
// The reserve reconciliation loop
// =====================================================================

/// Drives [`super::reserve::ReserveReconciler::tick`] until `shutdown`
/// reports `true`, then returns how many ticks ran.
///
/// # Why this is a THIRD loop
///
/// It is not part of [`run`] because that loop's RPC bound is [`EvmRpc`]
/// alone and its contract with a reviewer is that it performs exactly
/// four `eth_*` methods, none of them `eth_call`. Reserve reconciliation
/// needs `eth_call`, and widening the observation loop's bound to get it
/// would quietly change what that loop is.
///
/// It is not part of [`run_settlement`] because that loop exists only
/// when `[robinhood.settlement]` produced a verified deployment, and
/// reading a balance must not require the ability to broadcast one —
/// see [`super::reserve`]'s module docs. Production runs the indexer
/// with no settlement section at all, and the reserve must still report
/// the truth there.
///
/// It is not part of [`crate::daemon::run`] for the reason that loop's
/// docs already record about the indexer: a Robinhood endpoint being
/// down would otherwise share the Solana<->Goldcoin settlement loop's
/// backoff, and a Robinhood incident would sit inside the report an
/// operator reads for live traffic.
///
/// Its RPC bound is `EvmRpc + EvmCallRpc`. [`super::rpc::EvmSubmitRpc`]
/// is absent, so nothing reachable from this loop can broadcast.
pub async fn run_reserve_reconciliation<R>(
    reconciler: &super::reserve::ReserveReconciler<R>,
    ledger: &mut crate::ledger::Ledger,
    config: RobinhoodLoopConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    now: impl Fn() -> i64,
) -> u64
where
    R: EvmRpc + super::rpc::EvmCallRpc,
{
    let mut ticks = 0u64;
    let mut consecutive_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            return ticks;
        }
        let outcome = reconciler.tick(ledger, now()).await;
        ticks += 1;
        consecutive_failures = match &outcome {
            // A skip is a failed READ, and backing off on it is the
            // whole reason the counter exists. `NotConfigured` is not a
            // failure — it is a stable, correct answer — so it resets
            // the counter rather than escalating a delay forever.
            super::reserve::ReserveTickOutcome::Skipped { .. } => {
                consecutive_failures.saturating_add(1)
            }
            _ => 0,
        };
        log_reserve_tick(ticks, &outcome);

        let delay = tick_backoff_delay(
            config.tick_interval,
            config.max_backoff,
            consecutive_failures,
        );
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return ticks;
                }
            }
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

fn log_reserve_tick(n: u64, outcome: &super::reserve::ReserveTickOutcome) {
    use super::reserve::ReserveTickOutcome;
    use crate::reconciliation::Classification;
    match outcome {
        ReserveTickOutcome::NotConfigured => {
            tracing::debug!(
                target: "robinhood_reserve",
                tick = n,
                "no [reserve.robinhood] section — there is no Robinhood reserve row to reconcile"
            );
        }
        ReserveTickOutcome::Skipped { reason } => {
            // Every tick, not once: a reserve whose reads are failing
            // and which stopped complaining is indistinguishable from
            // one that recovered — the same reasoning the indexer's
            // halt logging records.
            tracing::error!(
                target: "robinhood_reserve",
                tick = n,
                %reason,
                "Robinhood reserve reconciliation SKIPPED — the cached balance was NOT updated \
                 and no balance was invented"
            );
        }
        ReserveTickOutcome::Reconciled {
            report,
            dust_remainder,
            block,
        } => {
            if report.auto_paused {
                tracing::error!(
                    target: "robinhood_reserve",
                    tick = n,
                    block,
                    observed_balance = report.observed_balance,
                    cached_balance_before = report.cached_balance_before,
                    protected_minimum = report.protected_minimum,
                    pending_obligations = report.pending_obligations,
                    "Robinhood reserve BREACH — the reserve has been PAUSED. Un-pausing is \
                     operator-only; reconciliation never resumes it automatically."
                );
            } else if report.classification == Classification::Breach {
                tracing::error!(
                    target: "robinhood_reserve",
                    tick = n,
                    block,
                    observed_balance = report.observed_balance,
                    "Robinhood reserve breach classified"
                );
            } else {
                tracing::debug!(
                    target: "robinhood_reserve",
                    tick = n,
                    block,
                    observed_balance = report.observed_balance,
                    cached_balance_before = report.cached_balance_before,
                    dust_remainder,
                    "Robinhood reserve reconciled"
                );
            }
        }
    }
}
