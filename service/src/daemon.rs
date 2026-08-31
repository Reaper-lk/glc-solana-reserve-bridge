//! The long-running tick loop a deployable process actually runs
//! (docs/15-post-phase6-audit.md, P0 item 1). Factored out of
//! `bin/glc-bridge-daemon.rs` so it can be exercised with the same mock
//! RPCs `orchestrator::tests` already uses, rather than only being
//! reachable by starting a real process against real nodes.
//!
//! # What this adds on top of [`Orchestrator::tick`]
//!
//! Every safety property — per-request fail-closed behavior, replay
//! protection, reconciliation auto-pause, idempotent state transitions —
//! already lives in [`crate::orchestrator::Orchestrator`] and is exercised
//! there regardless of what drives it. This module adds exactly two
//! things a bare `loop { tick().await; sleep().await; }` would not have:
//!
//! 1. **Backoff on total RPC outage.** When neither indexer can complete a
//!    tick (both Goldcoin and Solana are unreachable), hammering a downed
//!    endpoint at the normal tick rate helps nobody. Consecutive
//!    all-down ticks widen the inter-tick delay up to a configured
//!    ceiling; any tick where either indexer succeeds resets it
//!    immediately. A partial outage (only one chain down) never triggers
//!    backoff — half the bridge can and should keep making progress.
//! 2. **A clean, checked shutdown point between ticks**, never mid-tick.
//!    A tick, once started, always runs to completion — every settlement
//!    step it might take is already individually idempotent and safe to
//!    interrupt by a real process crash (that's what Phase 6's real-node
//!    crash/restart test exercises), so there is no correctness reason to
//!    interrupt one early; doing so would only add a code path nothing
//!    else needs.

use std::time::Duration;

use crate::goldcoin::indexer::GoldcoinRpc;
use crate::orchestrator::{Orchestrator, TickReport};
use crate::solana::rpc::SolanaRpc;

#[derive(Debug, Clone, Copy)]
pub struct DaemonLoopConfig {
    pub tick_interval: Duration,
    /// Ceiling for the backoff described in the module docs. Never
    /// applied to a healthy or partially-healthy tick, only to a run of
    /// consecutive all-down ticks.
    pub max_backoff: Duration,
}

/// Drives `orchestrator.tick()` until `shutdown` reports `true`, then
/// returns. `now` is injected (rather than reading the clock internally)
/// so tests can drive fake time without a real `tokio::time::sleep`
/// depending on wall-clock progress.
pub async fn run<GR, SR>(
    orchestrator: &mut Orchestrator<GR, SR>,
    config: DaemonLoopConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    now: impl Fn() -> i64,
) -> u32
where
    GR: GoldcoinRpc,
    SR: SolanaRpc,
{
    let mut ticks_run = 0u32;
    let mut consecutive_full_failures = 0u32;
    loop {
        if *shutdown.borrow() {
            return ticks_run;
        }
        let report = orchestrator.tick(now()).await;
        ticks_run += 1;
        log_tick(ticks_run, &report);

        consecutive_full_failures = if both_indexers_failed(&report) {
            consecutive_full_failures.saturating_add(1)
        } else {
            0
        };
        let delay = tick_backoff_delay(
            config.tick_interval,
            config.max_backoff,
            consecutive_full_failures,
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

fn both_indexers_failed(report: &TickReport) -> bool {
    let goldcoin_failed = !matches!(report.goldcoin_indexer, Some(Ok(_)));
    let solana_failed = !matches!(report.solana_indexer, Some(Ok(_)));
    goldcoin_failed && solana_failed
}

/// `base` for 0 consecutive failures, doubling per additional consecutive
/// all-down tick, capped at `max`. Never zero and never below `base`.
fn tick_backoff_delay(base: Duration, max: Duration, consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return base;
    }
    let factor = 2u32.saturating_pow(consecutive_failures.min(16));
    base.saturating_mul(factor).min(max)
}

fn log_tick(n: u32, report: &TickReport) {
    // Split-lifecycle events are operator-relevant regardless of overall
    // tick health (2026-08-31 final review: a real fund-loss signal must
    // never live only inside an unlogged report field).
    if let Some(shaping) = &report.utxo_shaping {
        if let Some(err) = &shaping.lifecycle_error {
            tracing::warn!(tick = n, %err, "split lifecycle needs attention");
        }
        if let Some((id, reason)) = &shaping.abandoned_split {
            tracing::warn!(tick = n, split = id, %reason, "split abandoned");
        }
        if !shaping.readopted_split_ids.is_empty() {
            tracing::warn!(
                tick = n,
                splits = ?shaping.readopted_split_ids,
                "abandoned split(s) re-adopted — the chain reports their transactions after all"
            );
        }
        if let Some(txid) = shaping.rebroadcast_split_txid {
            tracing::info!(tick = n, txid = %crate::goldcoin::hex::encode(&txid),
                "evicted split re-broadcast");
        }
        if let Some(txid) = shaping.new_split_txid.or(shaping.resumed_split_txid) {
            tracing::info!(tick = n, txid = %crate::goldcoin::hex::encode(&txid),
                "split broadcast");
        }
        if !shaping.confirmed_split_ids.is_empty() {
            tracing::info!(tick = n, splits = ?shaping.confirmed_split_ids, "split(s) confirmed");
        }
    }
    if !report.split_bookkeeping_healed.is_empty() {
        tracing::warn!(
            tick = n,
            splits = ?report.split_bookkeeping_healed,
            "split broadcast bookkeeping healed after a crash window"
        );
    }
    if report.errors.is_empty()
        && matches!(report.goldcoin_indexer, Some(Ok(_)))
        && matches!(report.solana_indexer, Some(Ok(_)))
    {
        tracing::debug!(tick = n, "tick completed cleanly");
        return;
    }
    tracing::warn!(
        tick = n,
        goldcoin_indexer = ?report.goldcoin_indexer.as_ref().map(|r| r.is_ok()),
        solana_indexer = ?report.solana_indexer.as_ref().map(|r| r.is_ok()),
        errors = ?report.errors,
        "tick completed with errors"
    );
}

#[cfg(test)]
mod tests;
