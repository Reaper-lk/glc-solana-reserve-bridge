//! Whether an indexer is still working (docs/07-implementation-plan.md
//! Phase 5). Ported near-verbatim from the old bridge's
//! `ops/indexer_status.rs` (docs/01-reuse-inventory.md: chain-agnostic,
//! reuse unchanged).
//!
//! # Why this exists
//!
//! The old bridge's own operational history is the reason this exists at
//! all: verifying its deep-reorg runbook found that a **halted indexer was
//! invisible**. On a reorg deeper than `max_reorg_depth` the Goldcoin
//! indexer stops writing and refuses to progress until an operator
//! intervenes ([`crate::goldcoin::indexer::Indexer`], a deliberate
//! security property — it never guesses a fork point). But the halt lived
//! in process-local memory, reachable only by the indexer itself, and a
//! process that stays alive for liveness probes looked healthy from every
//! angle an operator monitors.
//!
//! That is not a safety failure — a halted indexer credits nothing, so it
//! fails closed — but deposits silently stop being observed, and a
//! runbook's *detection* step with no honest answer beyond "grep the
//! logs" is what this module exists to eliminate. [`crate::orchestrator::
//! Orchestrator::tick`] updates one of these per chain from the
//! `TickOutcome`/`SolanaTickOutcome` it already receives every tick — no
//! changes needed to the indexers themselves.
//!
//! # What is and is not an invariant
//!
//! The **halt** (Goldcoin only — the Solana indexer has no halt concept,
//! it simply errors and is retried next tick) is an invariant: it is
//! unambiguous, requires an operator, and never resolves on its own.
//!
//! Time since the last successful tick is exposed as a **gauge only**. A
//! quiet chain produces no blocks, and a node being briefly unreachable is
//! retried — neither is a fault, and this crate has no basis for choosing
//! the threshold that separates "slow" from "broken". The operator's own
//! scraper decides.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

/// Shared between the orchestrator's tick loop and the ops collector.
#[derive(Debug)]
pub struct IndexerStatus {
    halted: AtomicBool,
    /// The depth that triggered the halt, for the report. Meaningless while
    /// `halted` is false.
    halted_depth: AtomicI64,
    /// Unix seconds of the last tick that completed **without** erroring.
    last_tick_unix: AtomicI64,
    /// The deepest reorg this process has rolled back. Exposed so an
    /// operator sees a chain trending toward `max_reorg_depth` **before**
    /// the indexer halts on it — the halt is the failure, not the warning.
    deepest_reorg: AtomicI64,
    /// The configured ceiling, so the gauge can be read as a ratio without
    /// the scraper having to know the deployment's configuration.
    max_reorg_depth: AtomicI64,
}

impl IndexerStatus {
    /// Starts un-halted, with the first tick not yet recorded.
    ///
    /// `started_at` seeds `last_tick_unix` so the "seconds since a tick"
    /// gauge measures from process start rather than from the epoch, which
    /// would read as decades of silence on the first scrape.
    pub fn new(started_at: i64) -> Self {
        IndexerStatus {
            halted: AtomicBool::new(false),
            halted_depth: AtomicI64::new(0),
            last_tick_unix: AtomicI64::new(started_at),
            deepest_reorg: AtomicI64::new(0),
            max_reorg_depth: AtomicI64::new(0),
        }
    }

    /// Records the configured ceiling once at startup.
    pub fn set_max_reorg_depth(&self, depth: i64) {
        self.max_reorg_depth.store(depth, Ordering::SeqCst);
    }

    /// Records a rolled-back reorg. Keeps the **deepest** seen, not the
    /// most recent: a single 40-block reorg an hour ago is what an operator
    /// needs to know about, and a later 1-block reorg must not erase it.
    pub fn record_reorg(&self, depth: i64) {
        self.deepest_reorg.fetch_max(depth, Ordering::SeqCst);
    }

    pub fn deepest_reorg(&self) -> i64 {
        self.deepest_reorg.load(Ordering::SeqCst)
    }

    pub fn max_reorg_depth(&self) -> i64 {
        self.max_reorg_depth.load(Ordering::SeqCst)
    }

    /// Records a tick that did real work (or found nothing to do).
    pub fn record_tick(&self, at_unix: i64) {
        self.last_tick_unix.store(at_unix, Ordering::SeqCst);
    }

    /// Records the halt. Deliberately one-way: nothing in-process ever
    /// clears it, because clearing it means an operator has widened
    /// `max_reorg_depth` and restarted the process.
    pub fn record_halt(&self, attempted_depth: i64) {
        // Depth first, then the flag that vouches for it, so a concurrent
        // reader can never see `halted` with a stale depth beside it.
        self.halted_depth.store(attempted_depth, Ordering::SeqCst);
        self.halted.store(true, Ordering::SeqCst);
    }

    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::SeqCst)
    }

    pub fn halted_depth(&self) -> i64 {
        self.halted_depth.load(Ordering::SeqCst)
    }

    pub fn last_tick_unix(&self) -> i64 {
        self.last_tick_unix.load(Ordering::SeqCst)
    }

    /// Seconds since the last non-erroring tick, floored at zero.
    ///
    /// Clamped because a clock that steps backwards must not report negative
    /// staleness, which would read as "very recent" to a threshold check.
    pub fn seconds_since_tick(&self, now_unix: i64) -> i64 {
        now_unix.saturating_sub(self.last_tick_unix()).max(0)
    }
}

#[cfg(test)]
mod tests;
