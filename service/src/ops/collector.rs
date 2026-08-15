//! Wires the ledger and the orchestrator's shared indexer-status handles
//! into a [`crate::ops::health::ReportSource`] (docs/07-implementation-plan.md
//! Phase 5). Ported from the old bridge's `ops/collector.rs` shape
//! (docs/01-reuse-inventory.md) — opens a fresh, independent read
//! connection to the SQLite database on every scrape (same "concurrent
//! operators, not one shared in-process handle" concurrency model the
//! rest of this ledger already relies on — see `orchestrator::tests`'
//! `build_orchestrator` doc comment) rather than holding a long-lived
//! `&mut Ledger` the orchestrator's own tick loop also needs.
//!
//! # What "reserve balance" means here
//!
//! [`crate::ops::reserve_health::check`] reads `reserve_ledger`'s cached
//! `total_reserve_balance` — the value as of the orchestrator's last
//! reconciliation tick, not a fresh on-chain/on-Goldcoin read performed by
//! this collector itself. That is an honest, deliberate choice: a health
//! endpoint reporting "what this service currently believes" is exactly
//! what an operator needs, and a scrape-time live chain read would
//! duplicate work the orchestrator's own reconciliation tick already does
//! on its own schedule.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ledger::{Direction, Ledger, RequestState, ReserveDirection};
use crate::ops::health::{build_report, HealthReport, IndexerSummary, ReportSource};
use crate::ops::indexer_status::IndexerStatus;
use crate::ops::reserve_health;

pub struct OpsCollector {
    db_path: PathBuf,
    goldcoin_indexer_status: Arc<IndexerStatus>,
    solana_indexer_status: Arc<IndexerStatus>,
}

impl OpsCollector {
    pub fn new(
        db_path: PathBuf,
        goldcoin_indexer_status: Arc<IndexerStatus>,
        solana_indexer_status: Arc<IndexerStatus>,
    ) -> Self {
        OpsCollector {
            db_path,
            goldcoin_indexer_status,
            solana_indexer_status,
        }
    }

    fn build(&self) -> HealthReport {
        // If the database can't even be opened, report 503-empty rather
        // than panic — a scrape must never be able to take the process
        // down, and "could not open the database" is itself the loudest
        // possible signal something is wrong.
        let Ok(ledger) = Ledger::open(&self.db_path) else {
            return HealthReport::default();
        };
        let now = now_unix();

        let goldcoin_reserve =
            reserve_health::check(&ledger, ReserveDirection::GoldcoinReserve).ok();
        let solana_reserve = reserve_health::check(&ledger, ReserveDirection::SolanaReserve).ok();
        let manual_review_count = manual_review_count(&ledger);
        let goldcoin_open_rebalances =
            open_rebalance_count(&ledger, ReserveDirection::GoldcoinReserve);
        let solana_open_rebalances = open_rebalance_count(&ledger, ReserveDirection::SolanaReserve);
        let post_finality_reorg_events = ledger.post_finality_reorg_event_count().unwrap_or(0);

        let goldcoin_indexer = Some(IndexerSummary {
            halted: self.goldcoin_indexer_status.is_halted(),
            halted_depth: self.goldcoin_indexer_status.halted_depth(),
            seconds_since_tick: self.goldcoin_indexer_status.seconds_since_tick(now),
            deepest_reorg: self.goldcoin_indexer_status.deepest_reorg(),
            max_reorg_depth: self.goldcoin_indexer_status.max_reorg_depth(),
        });
        let solana_indexer = Some(IndexerSummary {
            halted: false, // no halt concept for the Solana indexer
            halted_depth: 0,
            seconds_since_tick: self.solana_indexer_status.seconds_since_tick(now),
            deepest_reorg: 0,
            max_reorg_depth: 0,
        });

        build_report(
            goldcoin_reserve,
            solana_reserve,
            manual_review_count,
            goldcoin_indexer,
            solana_indexer,
            &[
                (
                    "glc_goldcoin_rebalance_requests_open",
                    goldcoin_open_rebalances as f64,
                    "Rebalance requests for the Goldcoin reserve not yet Confirmed/Rejected/Cancelled/Failed",
                ),
                (
                    "glc_solana_rebalance_requests_open",
                    solana_open_rebalances as f64,
                    "Rebalance requests for the Solana reserve not yet Confirmed/Rejected/Cancelled/Failed",
                ),
                (
                    "glc_post_finality_reorg_events_total",
                    post_finality_reorg_events as f64,
                    "Cumulative post-finality Goldcoin reorg events ever detected (docs/10-threat-model.md) — any nonzero value means both reserves were paused at least once for this reason",
                ),
            ],
        )
    }
}

impl ReportSource for OpsCollector {
    fn report(&self) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(async move { self.build() })
    }
}

/// Requests parked in `ManualReview`, summed across both directions —
/// there is no single-direction concept for this state (docs/04-state-
/// machines.md: a request can land there from either leg).
fn manual_review_count(ledger: &Ledger) -> u64 {
    [Direction::GlcToSol, Direction::SolToGlc]
        .iter()
        .map(|&d| {
            ledger
                .requests_by_state(d, RequestState::ManualReview)
                .map(|r| r.len() as u64)
                .unwrap_or(0)
        })
        .sum()
}

/// Rebalance requests for `direction` still in a non-terminal state
/// (`RebalanceState::is_open`) — a stuck one (e.g. `Approved` for days
/// with no execution recorded) is exactly the kind of thing an operator
/// wants visible on a dashboard, not just discoverable via `glc-admin
/// rebalance-list`.
fn open_rebalance_count(ledger: &Ledger, direction: ReserveDirection) -> u64 {
    ledger
        .list_rebalances(Some(direction), true)
        .map(|r| r.len() as u64)
        .unwrap_or(0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
