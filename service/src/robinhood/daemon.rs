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
