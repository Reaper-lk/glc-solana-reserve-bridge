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

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

use crate::evm::EvmAddress;
use crate::ledger::{CustodyTransitionKind, Direction, Ledger, RequestState, ReserveDirection};
use crate::ops::health::{
    build_report, HealthReport, IndexerSummary, ReportSource, RobinhoodSummary,
};
use crate::ops::indexer_status::IndexerStatus;
use crate::ops::reserve_health;
use crate::robinhood::RobinhoodHealth;
use crate::routes::{Route, RouteGate};

/// Everything the collector needs to report the Robinhood leg, gathered
/// once at startup by the daemon.
///
/// Held behind an `Option` on the collector so that a deployment with no
/// `[robinhood.indexer]` section is not merely reported as empty — it
/// contributes NOTHING to the report at all. That is the difference
/// between "unchanged when Robinhood config is absent" as a claim and as
/// a property.
pub struct RobinhoodOps {
    /// The live health state the indexer publishes to. Strings read out
    /// of it are already redacted — see `robinhood::redact`.
    pub health: Arc<RobinhoodHealth>,
    /// The route gate, consulted per scrape against a fresh ledger read,
    /// so a scrape can never disagree with what `POST /transfers` would
    /// decide at the same instant.
    pub route_gate: Arc<RouteGate>,
    /// `[robinhood.settlement]` was present in this process.
    pub settlement_configured: bool,
    /// Its startup preflight produced a `VerifiedDeployment`.
    pub deployment_verified: bool,
    /// Authorization signers this process actually loaded/connected, and
    /// how many a quorum needs.
    pub signers_available: usize,
    pub signers_required: usize,
    /// The submitter and the chain it settles on, for the nonce read.
    /// `None` when no settlement section is configured.
    pub submitter: Option<(EvmAddress, u64)>,
}

pub struct OpsCollector {
    db_path: PathBuf,
    goldcoin_indexer_status: Arc<IndexerStatus>,
    solana_indexer_status: Arc<IndexerStatus>,
    /// `None` for every deployment that has never configured Robinhood.
    robinhood: Option<RobinhoodOps>,
    /// The daemon's Solana program compatibility cache; `None` until
    /// `with_program_compat` (a collector that was never handed one
    /// reports the program as unprobed).
    program_compat: Option<Arc<crate::solana::program_compat::ProgramCompatCache>>,
    /// The daemon's bridge-rate book (docs/38-elastic-bridge-rate.md);
    /// `None` until `with_rate_book`. Only a LIVE book adds gauges — a
    /// fixed unit rate has nothing to report.
    rate_book: Option<crate::bridge_rate::RateBook>,
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
            robinhood: None,
            program_compat: None,
            rate_book: None,
        }
    }

    /// Shares the daemon's bridge-rate book, for the per-rail and
    /// per-route gauges (`glc_bridge_rate_*`).
    pub fn with_rate_book(mut self, rate_book: crate::bridge_rate::RateBook) -> Self {
        self.rate_book = Some(rate_book);
        self
    }

    /// Adds the deployed Solana program's instruction-support probe to
    /// this collector's report.
    pub fn with_program_compat(
        mut self,
        cache: Arc<crate::solana::program_compat::ProgramCompatCache>,
    ) -> Self {
        self.program_compat = Some(cache);
        self
    }

    /// Adds the Robinhood leg to this collector's report.
    ///
    /// A BUILDER rather than a fourth constructor parameter, so every
    /// existing call site is untouched and a deployment that never calls
    /// it produces exactly the report it always did.
    pub fn with_robinhood(mut self, robinhood: RobinhoodOps) -> Self {
        self.robinhood = Some(robinhood);
        self
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
            reserve_health::check(&ledger, ReserveDirection::GoldcoinReserve, unix_now()).ok();
        let solana_reserve =
            reserve_health::check(&ledger, ReserveDirection::SolanaReserve, unix_now()).ok();
        let manual_review_count = manual_review_count(&ledger);
        let goldcoin_open_rebalances =
            open_rebalance_count(&ledger, ReserveDirection::GoldcoinReserve);
        let solana_open_rebalances = open_rebalance_count(&ledger, ReserveDirection::SolanaReserve);
        let post_finality_reorg_events = ledger.post_finality_reorg_event_count().unwrap_or(0);
        let open_attestation_rotations =
            open_custody_transition_count(&ledger, CustodyTransitionKind::AttestationKeyRotation);
        let open_vault_sweeps =
            open_custody_transition_count(&ledger, CustodyTransitionKind::GoldcoinVaultSweep);

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

        // Both `None` unless Robinhood is configured — and the reserve
        // is separately `None` unless `[reserve.robinhood]` exists, since
        // a deployment can watch the chain long before it holds a reserve
        // there.
        //
        // `.ok()` discards the error deliberately: an unconfigured
        // reserve has no `reserve_ledger` row and reports
        // `ReserveNotInitialized`, which here means "absent", not
        // "broken". Reporting zeroes instead would read as "configured
        // and empty" — a different and much more alarming statement.
        let robinhood_reserve = self.robinhood.as_ref().and_then(|_| {
            reserve_health::check(&ledger, ReserveDirection::RobinhoodReserve, unix_now()).ok()
        });
        let robinhood = self
            .robinhood
            .as_ref()
            .map(|rhn| self.robinhood_summary(rhn, &ledger, now));

        let solana_program = self.program_compat.as_ref().and_then(|cache| {
            cache
                .snapshot()
                .compat
                .as_ref()
                .map(super::health::SolanaProgramSummary::from_compat)
        });

        let mut extra: Vec<(&str, f64, &'static str)> = vec![
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
                (
                    "glc_attestation_key_rotations_open",
                    open_attestation_rotations as f64,
                    "Attestation-key-rotation custody transitions not yet Confirmed/Rejected/Cancelled/Failed/RolledBack",
                ),
                (
                    "glc_goldcoin_vault_sweeps_open",
                    open_vault_sweeps as f64,
                    "Goldcoin-vault-sweep custody transitions not yet Confirmed/Rejected/Cancelled/Failed/RolledBack",
                ),
        ];
        if let Some(book) = self.rate_book.as_ref().and_then(|b| b.live_book()) {
            extra.extend(bridge_rate_gauges(book, now));
        }

        build_report(
            goldcoin_reserve,
            solana_reserve,
            manual_review_count,
            goldcoin_indexer,
            solana_indexer,
            robinhood_reserve,
            robinhood,
            solana_program,
            &extra,
        )
    }
}

/// The live bridge-rate gauges (docs/38-elastic-bridge-rate.md,
/// "Observability"): per rail the smoothed price, the newest sample's
/// age and whether the rail is usable; per route the struck rate, its
/// movement against one window ago and whether the route is admitting.
/// Static metric names — no per-scrape allocation.
fn bridge_rate_gauges(
    book: &crate::bridge_rate::LiveBook,
    now: i64,
) -> Vec<(&'static str, f64, &'static str)> {
    use crate::bridge_rate::live::RailStatus;
    use crate::routes::Chain;
    let mut out = Vec::new();
    for snap in book.snapshots(now) {
        let (price, age, ok): (&'static str, &'static str, &'static str) = match snap.chain {
            Chain::Goldcoin => (
                "glc_bridge_rate_goldcoin_price_e12",
                "glc_bridge_rate_goldcoin_sample_age_secs",
                "glc_bridge_rate_goldcoin_ok",
            ),
            Chain::Solana => (
                "glc_bridge_rate_solana_price_e12",
                "glc_bridge_rate_solana_sample_age_secs",
                "glc_bridge_rate_solana_ok",
            ),
            Chain::Robinhood => (
                "glc_bridge_rate_robinhood_price_e12",
                "glc_bridge_rate_robinhood_sample_age_secs",
                "glc_bridge_rate_robinhood_ok",
            ),
        };
        out.push((
            price,
            snap.smoothed_price_e12.unwrap_or(0) as f64,
            "Smoothed USD price of GLC on this rail, fixed-point x 1e12 (0 = none)",
        ));
        out.push((
            age,
            snap.newest_feed_at
                .map(|t| (now - t) as f64)
                .unwrap_or(-1.0),
            "Seconds since this rail's newest price sample (-1 = no sample)",
        ));
        out.push((
            ok,
            if snap.status == RailStatus::Ok {
                1.0
            } else {
                0.0
            },
            "1 when this rail has two full gap-free windows of fresh history, 0 otherwise",
        ));
    }
    for route in Route::ALL {
        let (rate_name, movement_name, admitting_name): (&'static str, &'static str, &'static str) =
            match route {
                Route::GlcToSol => (
                    "glc_bridge_rate_glc_to_sol_rate_e12",
                    "glc_bridge_rate_glc_to_sol_movement_bps",
                    "glc_bridge_rate_glc_to_sol_admitting",
                ),
                Route::SolToGlc => (
                    "glc_bridge_rate_sol_to_glc_rate_e12",
                    "glc_bridge_rate_sol_to_glc_movement_bps",
                    "glc_bridge_rate_sol_to_glc_admitting",
                ),
                Route::GlcToRhn => (
                    "glc_bridge_rate_glc_to_rhn_rate_e12",
                    "glc_bridge_rate_glc_to_rhn_movement_bps",
                    "glc_bridge_rate_glc_to_rhn_admitting",
                ),
                Route::RhnToGlc => (
                    "glc_bridge_rate_rhn_to_glc_rate_e12",
                    "glc_bridge_rate_rhn_to_glc_movement_bps",
                    "glc_bridge_rate_rhn_to_glc_admitting",
                ),
                Route::SolToRhn => (
                    "glc_bridge_rate_sol_to_rhn_rate_e12",
                    "glc_bridge_rate_sol_to_rhn_movement_bps",
                    "glc_bridge_rate_sol_to_rhn_admitting",
                ),
                Route::RhnToSol => (
                    "glc_bridge_rate_rhn_to_sol_rate_e12",
                    "glc_bridge_rate_rhn_to_sol_movement_bps",
                    "glc_bridge_rate_rhn_to_sol_admitting",
                ),
            };
        let (rate, movement, admitting) = match book.route_rate(route, now) {
            Ok(r) => (
                r.rate_e12().unwrap_or(0) as f64,
                r.movement_bps as f64,
                if r.band_exceeded { 0.0 } else { 1.0 },
            ),
            Err(_) => (0.0, -1.0, 0.0),
        };
        out.push((
            rate_name,
            rate,
            "This route's bridge rate (source price / destination price), fixed-point x 1e12 (0 = none)",
        ));
        out.push((
            movement_name,
            movement,
            "This route's rate movement against one smoothing window ago, basis points (-1 = none)",
        ));
        out.push((
            admitting_name,
            admitting,
            "1 when the bridge rate admits new deposits on this route, 0 when halted or outside the band",
        ));
    }
    out
}

impl OpsCollector {
    /// Flattens the live health state, the ledger and the startup facts
    /// into one [`RobinhoodSummary`].
    ///
    /// Every string it copies out of the health snapshot is already
    /// redacted; this function adds no new source of text and must never
    /// gain one — `/health` is unauthenticated.
    fn robinhood_summary(&self, rhn: &RobinhoodOps, ledger: &Ledger, now: i64) -> RobinhoodSummary {
        let snapshot = rhn.health.snapshot();
        // A chain id can only DISAGREE once one has been observed. An
        // indexer that has not ticked yet is not reported as being on the
        // wrong network.
        let chain_id_agrees = match (snapshot.expected_chain_id, snapshot.observed_chain_id) {
            (Some(expected), Some(observed)) => expected == observed,
            _ => true,
        };
        let in_flight = crate::robinhood::admin::open_operations(ledger)
            .map(|v| v.len())
            .unwrap_or(0);
        let stalled = crate::robinhood::admin::stalled_operations(ledger)
            .map(|v| v.len())
            .unwrap_or(0);
        let submitter_observed_nonce = rhn.submitter.and_then(|(address, chain_id)| {
            ledger
                .evm_submitter_nonce(address.to_bytes(), chain_id)
                .ok()
                .flatten()
                .map(|(nonce, _)| nonce)
        });
        // Evaluated per scrape through the SAME gate the write paths use.
        let any_route_open = Route::ALL
            .iter()
            .filter(|r| r.contract_route_id().is_some())
            .any(|r| rhn.route_gate.is_enabled(ledger, *r));

        RobinhoodSummary {
            connected: snapshot.connected,
            expected_chain_id: snapshot.expected_chain_id,
            observed_chain_id: snapshot.observed_chain_id,
            chain_id_agrees,
            head_block: snapshot.head_block,
            finalized_block: snapshot.finalized_block,
            cursor_block: snapshot.cursor_block,
            lag_blocks: snapshot.lag_blocks,
            seconds_since_success: snapshot
                .last_success_unix
                .map(|at| now.saturating_sub(at))
                .unwrap_or(0),
            last_rpc_error_class: snapshot.last_rpc_error_class.map(|c| c.as_str()),
            last_rpc_error: snapshot.last_rpc_error.clone(),
            reorgs_reconciled: snapshot.reorgs_reconciled,
            deepest_reorg_blocks: snapshot.deepest_reorg_blocks,
            halted: snapshot.halt.is_some(),
            halt_reason: snapshot.halt.as_ref().map(|h| h.reason.as_str()),
            halt_detail: snapshot.halt.as_ref().map(|h| h.detail.clone()),
            settlement_configured: rhn.settlement_configured,
            deployment_verified: rhn.deployment_verified,
            signers_available: rhn.signers_available,
            signers_required: rhn.signers_required,
            submitter_observed_nonce,
            operations_in_flight: in_flight,
            operations_stalled: stalled,
            any_route_open,
            obligation_audit_age_secs: snapshot
                .obligation_audit
                .as_ref()
                .map(|a| now.saturating_sub(a.at_unix)),
            obligation_audit_error: snapshot.obligation_audit_error.map(|(e, _)| e),
            obligation_audit: snapshot.obligation_audit,
        }
    }
}

impl ReportSource for OpsCollector {
    fn report(&self) -> Pin<Box<dyn Future<Output = HealthReport> + Send + '_>> {
        Box::pin(async move { self.build() })
    }
}

/// Requests parked in `ManualReview`, summed across every direction —
/// there is no single-direction concept for this state (docs/04-state-
/// machines.md: a request can land there from either leg). Iterating
/// [`Direction::ALL`] rather than a hand-written pair means a direction
/// added later is counted from the day it exists, instead of accruing an
/// invisible backlog until someone notices the list is short.
fn manual_review_count(ledger: &Ledger) -> u64 {
    Direction::ALL
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

/// Custody transitions of `kind` still in a non-terminal state
/// (`CustodyTransitionState::is_open`) — a stuck one (e.g. `Approved` for
/// days with no execution recorded) is exactly the kind of thing an
/// operator wants visible on a dashboard, not just discoverable via
/// `glc-admin custody-list`.
fn open_custody_transition_count(ledger: &Ledger, kind: CustodyTransitionKind) -> u64 {
    ledger
        .list_custody_transitions(Some(kind), true)
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
