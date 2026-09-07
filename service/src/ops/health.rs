//! The operator-facing HTTP surface (docs/07-implementation-plan.md
//! Phase 5). Ported from the old bridge's `ops/health.rs`
//! (docs/01-reuse-inventory.md: hand-rolled registry/encoder and
//! health/metrics separation reusable as-is) with the invariant list
//! rewritten around this bridge's reserve model instead of a
//! wrapped-supply solvency check.
//!
//! Two read-only endpoints:
//!
//! | path | purpose |
//! |---|---|
//! | `/health` | one line per invariant, and an HTTP status an uptime check can read |
//! | `/metrics` | Prometheus text exposition |
//!
//! # It exposes; it does not page
//!
//! No alerting integration lives here. `/health` returns **503** when any
//! page-immediately invariant is breached, so an operator's existing
//! uptime monitoring raises the alarm using credentials this process
//! never sees.
//!
//! # Bind it privately
//!
//! There is no authentication, because adding one would mean this process
//! holding another secret. The endpoint reveals reserve balances and
//! per-state request counts — operational detail, not key material, but
//! not public either. Bind it to a private interface or a loopback
//! address behind the operator's own proxy.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

use crate::ops::metrics::Registry;
use crate::ops::reserve_health::ReserveSnapshot;

/// One thing an operator is expected to be paged about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invariant {
    pub name: &'static str,
    pub healthy: bool,
    pub detail: String,
}

/// Everything the endpoint reports, rebuilt on every scrape.
#[derive(Debug, Clone, Default)]
pub struct HealthReport {
    pub invariants: Vec<Invariant>,
    /// Rendered Prometheus text.
    pub metrics: String,
}

impl HealthReport {
    /// Whether every page-immediately invariant holds.
    ///
    /// An empty report is **not** healthy: it means the collector produced
    /// nothing, and a monitor that reports OK when it has measured nothing
    /// is worse than no monitor.
    pub fn healthy(&self) -> bool {
        !self.invariants.is_empty() && self.invariants.iter().all(|i| i.healthy)
    }

    pub fn status(&self) -> StatusCode {
        if self.healthy() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }

    /// The plain-text body of `/health`, one line per invariant.
    pub fn text(&self) -> String {
        if self.invariants.is_empty() {
            return "UNKNOWN no invariants were evaluated\n".to_string();
        }
        let mut out = String::new();
        for i in &self.invariants {
            out.push_str(if i.healthy { "OK   " } else { "BREACH " });
            out.push_str(i.name);
            if !i.detail.is_empty() {
                out.push_str(": ");
                out.push_str(&i.detail);
            }
            out.push('\n');
        }
        out
    }
}

/// Flattened indexer facts, gathered by the caller so [`build_report`]
/// stays a pure function of its measurements — everything that touches
/// the ledger or a live chain read happens in
/// [`crate::ops::collector::OpsCollector`], not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexerSummary {
    /// Always `false` for the Solana indexer, which has no halt concept —
    /// see `ops::indexer_status` module docs.
    pub halted: bool,
    /// Meaningless unless `halted`.
    pub halted_depth: i64,
    pub seconds_since_tick: i64,
    /// Deepest reorg rolled back so far, and the ceiling beyond which the
    /// Goldcoin indexer halts. Gauges, not an invariant: a deep-but-
    /// survivable reorg is a fact about the chain, not a fault in the
    /// bridge, and the threshold that should worry a given deployment is
    /// the operator's to set. Always `0` for the Solana indexer.
    pub deepest_reorg: i64,
    pub max_reorg_depth: i64,
}

/// The Robinhood leg's facts, flattened by the caller for the same
/// reason [`IndexerSummary`] is: [`build_report`] stays a pure function
/// of its measurements.
///
/// # Nothing here can carry the endpoint's identity
///
/// `last_rpc_error` and `halt_detail` arrive already redacted — they are
/// copied straight out of
/// [`crate::robinhood::RobinhoodHealthSnapshot`], which applies
/// [`crate::robinhood::redact::Redactor`] on the way IN. That matters
/// specifically here: `/health` has no authentication (see this module's
/// "Bind it privately"), so an RPC URL with credentials in it reaching
/// this struct would be readable by anything that can reach the port.
/// There is deliberately no field for the RPC URL, the endpoint host, or
/// any credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodSummary {
    pub connected: bool,
    pub expected_chain_id: Option<u64>,
    pub observed_chain_id: Option<u64>,
    /// False only when a chain id was actually OBSERVED and disagreed —
    /// never merely because none has been read yet.
    pub chain_id_agrees: bool,
    pub head_block: Option<u64>,
    /// The highest block this service would treat as irreversible at that
    /// head — the scannable frontier.
    pub finalized_block: Option<u64>,
    pub cursor_block: Option<u64>,
    pub lag_blocks: Option<u64>,
    pub seconds_since_success: i64,
    /// A stable category, derived from the error's TYPE. Survives
    /// redaction.
    pub last_rpc_error_class: Option<&'static str>,
    /// Redacted message text, or `None` if no tick has failed.
    pub last_rpc_error: Option<String>,
    pub reorgs_reconciled: u64,
    pub deepest_reorg_blocks: u64,
    pub halted: bool,
    pub halt_reason: Option<&'static str>,
    /// Redacted.
    pub halt_detail: Option<String>,
    /// A `[robinhood.settlement]` section is present in this process.
    pub settlement_configured: bool,
    /// Its startup preflight produced a verified deployment.
    pub deployment_verified: bool,
    pub signers_available: usize,
    pub signers_required: usize,
    /// The last `eth_getTransactionCount(..., "pending")` this service
    /// recorded for the submitter. A reconciliation input, never the
    /// allocator.
    pub submitter_observed_nonce: Option<u64>,
    pub operations_in_flight: usize,
    /// Operations that have stopped and will not resume without a human.
    pub operations_stalled: usize,
    /// Whether ANY Robinhood route is currently open at every gate. When
    /// no route is open, an unformable signer quorum is a launch blocker
    /// rather than an outage, and this is what tells the two apart.
    pub any_route_open: bool,
}

impl RobinhoodSummary {
    pub fn signer_quorum_available(&self) -> bool {
        self.signers_required > 0 && self.signers_available >= self.signers_required
    }
}

/// Builds the invariant list and metric registry from whatever the caller
/// has gathered. Pure: takes measurements, returns a report — everything
/// that touches the ledger or a chain happens in the caller
/// ([`crate::ops::collector::OpsCollector`]), which is what makes this
/// testable without either.
#[allow(clippy::too_many_arguments)]
pub fn build_report(
    goldcoin_reserve: Option<ReserveSnapshot>,
    solana_reserve: Option<ReserveSnapshot>,
    manual_review_count: u64,
    goldcoin_indexer: Option<IndexerSummary>,
    solana_indexer: Option<IndexerSummary>,
    // `None` — every deployment with no `[robinhood.indexer]` section —
    // adds NOTHING: no invariant, no gauge, no reserve row. The report is
    // byte-for-byte what it was before this parameter existed, which is
    // the property `existing health behavior must remain unchanged when
    // Robinhood config is absent` actually means.
    robinhood_reserve: Option<ReserveSnapshot>,
    robinhood: Option<RobinhoodSummary>,
    extra: &[(&str, f64, &'static str)],
) -> HealthReport {
    let mut invariants = Vec::new();
    let mut r = Registry::new();

    // A third INDEPENDENT reserve. Listed alongside the other two, never
    // summed with them: they are different physical pools on different
    // chains and a combined figure would imply a fungibility that does
    // not exist. `None` (no `[reserve.robinhood]`) contributes nothing at
    // all — not a zero row, which would read as "configured and empty".
    for (label, prefix, snapshot) in [
        ("Goldcoin", "goldcoin", goldcoin_reserve),
        ("Solana", "solana", solana_reserve),
        ("Robinhood", "robinhood", robinhood_reserve),
    ] {
        let Some(s) = snapshot else { continue };
        invariants.push(Invariant {
            name: leak_name(format!("{prefix}_reserve_invariant")),
            healthy: s.invariant_holds,
            detail: if s.invariant_holds {
                format!(
                    "balance {} >= protected_minimum {} + reserved {}",
                    s.total_reserve_balance, s.protected_minimum, s.reserved_liquidity
                )
            } else if s.immature_vault_utxo_total > 0 {
                // Worth stating explicitly at exactly the moment it
                // matters: an operator seeing this breach can tell, from
                // the detail alone, whether it is already self-resolving
                // (a maturing change output, not a genuine shortfall).
                format!(
                    "balance {} BELOW protected_minimum {} + reserved {} (docs/05-reserve-accounting.md hard invariant) — \
                     {} atomic units of vault UTXO value is still maturing (below vault_min_confirmations) and excluded from balance above; \
                     may resolve automatically once it matures",
                    s.total_reserve_balance,
                    s.protected_minimum,
                    s.reserved_liquidity,
                    s.immature_vault_utxo_total
                )
            } else {
                format!(
                    "balance {} BELOW protected_minimum {} + reserved {} (docs/05-reserve-accounting.md hard invariant)",
                    s.total_reserve_balance, s.protected_minimum, s.reserved_liquidity
                )
            },
        });
        invariants.push(Invariant {
            name: leak_name(format!("{prefix}_reserve_active")),
            healthy: !s.paused,
            detail: if s.paused {
                format!("{label} reserve is PAUSED — see reconciliation_findings / operator log for why")
            } else {
                String::new()
            },
        });
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_balance_atomic")),
            "Reserve balance, atomic units",
            s.total_reserve_balance as f64,
        );
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_protected_minimum_atomic")),
            "Reserve protected minimum, atomic units",
            s.protected_minimum as f64,
        );
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_reserved_liquidity_atomic")),
            "Reserve liquidity currently reserved against in-flight requests, atomic units",
            s.reserved_liquidity as f64,
        );
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_pending_obligations_atomic")),
            "Reserve liquidity committed to SourceFinalized-or-later requests, atomic units",
            s.pending_obligations as f64,
        );
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_paused")),
            "1 when this reserve direction is paused, 0 otherwise",
            u8::from(s.paused) as f64,
        );
        // Reported for audit visibility only (docs/20-bridge-fee.md) —
        // never counted toward `reserve_invariant`/`reserve_active` above,
        // or toward any available-capacity figure: `reserved_liquidity`/
        // `pending_obligations` already track NET (post-fee) amounts only.
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_accrued_fees_atomic")),
            "Cumulative bridge-fee revenue accrued on this reserve's row, canonical atomic units",
            s.accrued_fees as f64,
        );
        // Already excluded from `total_reserve_balance` above and from
        // every invariant/pause decision — reported so a paused-or-low
        // reserve can be seen as already self-resolving (a maturing
        // change output) rather than needing a fresh deposit. Always 0
        // for Solana, which has no UTXO-maturity concept.
        r.gauge(
            leak_name(format!("glc_{prefix}_reserve_immature_vault_utxo_total_atomic")),
            "Goldcoin vault UTXO value observed on-chain but not yet past vault_min_confirmations, atomic units — already excluded from the reserve balance above",
            s.immature_vault_utxo_total as f64,
        );
        // UTXO-pool liquidity (docs/09-runbook.md's "UTXO liquidity"
        // section) — zeroed and skipped for Solana, which has no UTXO-pool
        // concept. `own_unconfirmed_change_atomic` is a subset of
        // `immature_vault_utxo_total` above, broken out because it is
        // KNOWN to be this service's own payout change rather than any
        // other still-maturing deposit.
        if prefix == "goldcoin" {
            r.gauge(
                leak_name(format!("glc_{prefix}_utxo_pool_mature_available_atomic")),
                "Real, currently spendable Goldcoin vault liquidity — the same pool coin selection draws from",
                s.utxo_pool.mature_available_atomic as f64,
            );
            r.gauge(
                leak_name(format!("glc_{prefix}_utxo_pool_own_unconfirmed_change_atomic")),
                "Reserve value known to be locked in this service's own broadcast-but-immature payout change — not missing, not yet spendable",
                s.utxo_pool.own_unconfirmed_change_atomic as f64,
            );
            r.gauge(
                leak_name(format!("glc_{prefix}_utxo_pool_available_count")),
                "Number of mature, currently spendable Goldcoin vault UTXOs",
                s.utxo_pool.available_utxo_count as f64,
            );
            r.gauge(
                leak_name(format!("glc_{prefix}_utxo_pool_unconfirmed_change_count")),
                "Number of this service's own broadcast-but-immature payout change UTXOs",
                s.utxo_pool.unconfirmed_change_utxo_count as f64,
            );
            // Deliberately a gauge, not an `Invariant`: a thin mature UTXO
            // pool is worth an operator's attention before
            // utxo_pool_min_available_count admission backpressure
            // actually engages, but it is not itself a fault (it recovers
            // automatically once change matures) and must never flip
            // `/health` to 503 — that would be exactly the kind of
            // misleading "reserves are down" signal this mechanism exists
            // to avoid (docs/09-runbook.md "UTXO liquidity").
            r.gauge(
                leak_name(format!("glc_{prefix}_utxo_pool_warning")),
                "1 when the mature UTXO pool is down to utxo_pool_warning_count or fewer available UTXOs, 0 otherwise — informational only, never pages",
                u8::from(s.utxo_pool_warning) as f64,
            );
            // Confirmed-liquidity admission safety buffer
            // (docs/09-runbook.md). Gauges, never `Invariant`s, for
            // exactly the reason above: a closed gate is the mechanism
            // working as designed on a fully solvent reserve — new
            // SolToGlc deposits park while every already-accepted
            // obligation keeps settling — so it must never flip `/health`
            // to 503. Alert on it staying closed, not on it closing.
            r.gauge(
                leak_name(format!("glc_{prefix}_admission_liquidity_closed")),
                "1 when the automatic confirmed-liquidity gate is holding new SolToGlc obligations back, 0 otherwise — separate from the operator-only admission_closed flag",
                u8::from(s.liquidity_admission_closed) as f64,
            );
            r.gauge(
                leak_name(format!("glc_{prefix}_confirmed_admission_headroom_atomic")),
                "Confirmed unreserved headroom (balance - protected_minimum - reserved_liquidity), atomic units — immature payout change deliberately excluded",
                s.confirmed_admission_headroom as f64,
            );
            r.gauge(
                leak_name(format!("glc_{prefix}_admission_buffer_atomic")),
                "Configured admission safety buffer: headroom below this closes SolToGlc admission, atomic units (0 = disabled)",
                s.admission_buffer_atomic as f64,
            );
            r.gauge(
                leak_name(format!("glc_{prefix}_admission_reopen_atomic")),
                "Configured reopen threshold: headroom at or above this reopens SolToGlc admission, atomic units",
                s.admission_reopen_atomic as f64,
            );
        }
    }

    // A request parked in ManualReview needs an operator's judgment and
    // never resolves on its own — the reserve-model equivalent of the old
    // bridge's "no_integrity_halts" invariant.
    invariants.push(Invariant {
        name: "no_manual_review_backlog",
        healthy: manual_review_count == 0,
        detail: if manual_review_count == 0 {
            String::new()
        } else {
            format!("{manual_review_count} request(s) awaiting manual review")
        },
    });
    r.gauge(
        "glc_manual_review_backlog",
        "Bridge requests currently in ManualReview",
        manual_review_count as f64,
    );

    // A halted Goldcoin indexer stops observing deposits and never
    // resolves on its own, yet the process stays alive for liveness
    // probes — without this the bridge could stop observing Goldcoin
    // entirely and still report 200 (see `ops::indexer_status`).
    if let Some(i) = goldcoin_indexer {
        invariants.push(Invariant {
            name: "goldcoin_indexer_not_halted",
            healthy: !i.halted,
            detail: if i.halted {
                format!(
                    "HALTED on a reorg deeper than max_reorg_depth (attempted {}); \
                     deposits are no longer being indexed and an operator must intervene",
                    i.halted_depth
                )
            } else {
                format!("last tick {}s ago", i.seconds_since_tick)
            },
        });
        push_indexer_gauges(&mut r, "goldcoin", i);
    }
    if let Some(i) = solana_indexer {
        // No halt concept for the Solana indexer (see `ops::indexer_status`
        // module docs) — freshness is reported as a gauge only, same
        // reasoning as the reorg gauges above.
        push_indexer_gauges(&mut r, "solana", i);
    }

    if let Some(rhn) = &robinhood {
        push_robinhood(&mut invariants, &mut r, rhn);
    }

    for (name, value, help) in extra {
        r.gauge(name, help, *value);
    }
    r.gauge(
        "glc_health",
        "1 when every page-immediately invariant holds, 0 otherwise",
        if invariants.iter().all(|i| i.healthy) {
            1.0
        } else {
            0.0
        },
    );

    HealthReport {
        invariants,
        metrics: r.encode(),
    }
}

/// The Robinhood leg's invariants and gauges.
///
/// # Which facts page, and which only report
///
/// An invariant here means "wake someone up". The bar is the same one
/// the Goldcoin indexer's halt already meets: a condition that does not
/// resolve on its own and that leaves the process looking healthy to
/// every other probe.
///
/// - **A halt pages.** Deposits stop being observed and nothing clears it
///   but a human.
/// - **A chain-id disagreement pages.** The endpoint is not the network
///   this deployment settles on.
/// - **A stalled operation pages.** Reverted or moved to ManualReview; it
///   will never be retried automatically.
/// - **An unformable signer quorum pages ONLY WHEN A ROUTE IS OPEN.**
///   With every route closed — which is how this ships — no operation can
///   be authorized anyway, so the missing quorum is a launch blocker and
///   paging on it would train operators to ignore the page. With a route
///   open it is a live outage.
///
/// Everything else is a gauge. Lag, reorg depth and in-flight counts are
/// facts about the chain and the workload, not faults, and the threshold
/// that should worry a given deployment is the operator's to set — the
/// same stance `push_indexer_gauges` already takes.
fn push_robinhood(invariants: &mut Vec<Invariant>, r: &mut Registry, rhn: &RobinhoodSummary) {
    invariants.push(Invariant {
        name: "robinhood_indexer_not_halted",
        healthy: !rhn.halted,
        detail: if rhn.halted {
            format!(
                "HALTED ({}): {} — deposits are no longer being indexed and an operator must \
                 intervene",
                rhn.halt_reason.unwrap_or("unknown"),
                rhn.halt_detail.as_deref().unwrap_or("")
            )
        } else {
            format!("last successful tick {}s ago", rhn.seconds_since_success)
        },
    });
    invariants.push(Invariant {
        name: "robinhood_chain_id_agrees",
        healthy: rhn.chain_id_agrees,
        detail: if rhn.chain_id_agrees {
            String::new()
        } else {
            format!(
                "endpoint reports chain id {:?} but this deployment is configured for {:?}",
                rhn.observed_chain_id, rhn.expected_chain_id
            )
        },
    });
    invariants.push(Invariant {
        name: "robinhood_no_stalled_operations",
        healthy: rhn.operations_stalled == 0,
        detail: if rhn.operations_stalled == 0 {
            String::new()
        } else {
            format!(
                "{} Robinhood operation(s) reverted or in ManualReview — these are NEVER retried \
                 automatically",
                rhn.operations_stalled
            )
        },
    });
    if rhn.any_route_open {
        invariants.push(Invariant {
            name: "robinhood_signer_quorum_available",
            healthy: rhn.signer_quorum_available(),
            detail: if rhn.signer_quorum_available() {
                String::new()
            } else {
                format!(
                    "{} of {} authorization signers available while a route is OPEN — no \
                     Robinhood operation can be authorized",
                    rhn.signers_available, rhn.signers_required
                )
            },
        });
    }

    let g =
        |r: &mut Registry, name: &'static str, help: &'static str, v: f64| r.gauge(name, help, v);
    g(
        r,
        "glc_robinhood_connected",
        "1 when the last Robinhood tick reached the endpoint, 0 otherwise",
        u8::from(rhn.connected) as f64,
    );
    g(
        r,
        "glc_robinhood_indexer_halted",
        "1 when the Robinhood indexer has halted and requires an operator, 0 otherwise",
        u8::from(rhn.halted) as f64,
    );
    g(
        r,
        "glc_robinhood_chain_id_agrees",
        "1 when the endpoint's chain id is the configured one, 0 otherwise",
        u8::from(rhn.chain_id_agrees) as f64,
    );
    g(
        r,
        "glc_robinhood_indexer_seconds_since_tick",
        "Seconds since the Robinhood indexer last completed a tick without erroring",
        rhn.seconds_since_success as f64,
    );
    g(
        r,
        "glc_robinhood_head_block",
        "The Robinhood endpoint's head block at the last successful read",
        rhn.head_block.unwrap_or(0) as f64,
    );
    g(
        r,
        "glc_robinhood_finalized_block",
        "The highest Robinhood block this service treats as irreversible at that head",
        rhn.finalized_block.unwrap_or(0) as f64,
    );
    g(
        r,
        "glc_robinhood_cursor_block",
        "The durable Robinhood scan cursor",
        rhn.cursor_block.unwrap_or(0) as f64,
    );
    g(
        r,
        "glc_robinhood_lag_blocks",
        "head - cursor, in blocks",
        rhn.lag_blocks.unwrap_or(0) as f64,
    );
    g(
        r,
        "glc_robinhood_reorgs_reconciled",
        "Robinhood reorgs this process has reconciled",
        rhn.reorgs_reconciled as f64,
    );
    g(
        r,
        "glc_robinhood_reorg_deepest_observed",
        "Deepest Robinhood reorg this process has rolled back, in blocks",
        rhn.deepest_reorg_blocks as f64,
    );
    g(
        r,
        "glc_robinhood_settlement_configured",
        "1 when a [robinhood.settlement] section is present in this process, 0 otherwise",
        u8::from(rhn.settlement_configured) as f64,
    );
    g(
        r,
        "glc_robinhood_deployment_verified",
        "1 when the startup preflight against the deployed contracts passed, 0 otherwise",
        u8::from(rhn.deployment_verified) as f64,
    );
    g(
        r,
        "glc_robinhood_signers_available",
        "Robinhood authorization signers this process could load/connect",
        rhn.signers_available as f64,
    );
    g(
        r,
        "glc_robinhood_signers_required",
        "Signatures a Robinhood authorization quorum requires",
        rhn.signers_required as f64,
    );
    g(
        r,
        "glc_robinhood_signer_quorum_available",
        "1 when enough authorization signers are available to form a quorum, 0 otherwise",
        u8::from(rhn.signer_quorum_available()) as f64,
    );
    g(r, "glc_robinhood_submitter_observed_nonce", "Last eth_getTransactionCount(pending) recorded for the submitter — a reconciliation input, never the allocator", rhn.submitter_observed_nonce.unwrap_or(0) as f64);
    g(
        r,
        "glc_robinhood_operations_in_flight",
        "Robinhood operations neither finalized nor terminally failed",
        rhn.operations_in_flight as f64,
    );
    g(
        r,
        "glc_robinhood_operations_stalled",
        "Robinhood operations reverted or moved to ManualReview — never retried automatically",
        rhn.operations_stalled as f64,
    );
    g(
        r,
        "glc_robinhood_any_route_open",
        "1 when at least one Robinhood route is open at every gate, 0 otherwise",
        u8::from(rhn.any_route_open) as f64,
    );
}

fn push_indexer_gauges(r: &mut Registry, prefix: &str, i: IndexerSummary) {
    r.gauge(
        leak_name(format!("glc_{prefix}_indexer_halted")),
        "1 when this indexer has halted and requires an operator, 0 otherwise",
        u8::from(i.halted) as f64,
    );
    r.gauge(
        leak_name(format!("glc_{prefix}_indexer_seconds_since_tick")),
        "Seconds since this indexer last completed a tick without erroring",
        i.seconds_since_tick as f64,
    );
    r.gauge(
        leak_name(format!("glc_{prefix}_reorg_deepest_observed")),
        "Deepest reorg this process has rolled back, in blocks",
        i.deepest_reorg as f64,
    );
    r.gauge(
        leak_name(format!("glc_{prefix}_reorg_max_depth_configured")),
        "Configured max_reorg_depth; beyond this the indexer halts",
        i.max_reorg_depth as f64,
    );
}

/// Prometheus family/invariant names are built with a `{direction}` prefix
/// at report-build time, but [`Registry::gauge`]/[`Invariant::name`] both
/// want `&'static str`. Leaking a handful of short strings per scrape (a
/// registry built fresh per request, never retained) is a deliberate,
/// bounded trade for keeping the family-name API's simplicity — the
/// alternative is threading owned `String`s through every call site in
/// this module for no real benefit, since the process is short-lived
/// relative to a scrape.
fn leak_name(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Produces a fresh report. Called once per request, so a scrape always
/// reflects the state at scrape time rather than a cached snapshot.
pub trait ReportSource: Send + Sync + 'static {
    fn report(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = HealthReport> + Send + '_>>;
}

async fn handle<S: ReportSource>(
    req: Request<hyper::body::Incoming>,
    source: Arc<S>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let (status, content_type, body) = match req.uri().path() {
        "/health" => {
            let report = source.report().await;
            (report.status(), "text/plain; charset=utf-8", report.text())
        }
        "/metrics" => {
            let report = source.report().await;
            // Always 200: a scrape failing because the bridge is unhealthy
            // would lose the very metrics an operator needs to diagnose it.
            (
                StatusCode::OK,
                "text/plain; version=0.0.4; charset=utf-8",
                report.metrics,
            )
        }
        _ => (
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "not found\n".to_string(),
        ),
    };
    Ok(Response::builder()
        .status(status)
        .header("content-type", content_type)
        // Nothing here is ever worth caching: every value is a point-in-time
        // reading.
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .expect("well-formed response"))
}

/// Serves `/health` and `/metrics` until shutdown.
pub async fn serve<S: ReportSource>(
    addr: SocketAddr,
    source: Arc<S>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "health and metrics endpoint listening (bind this privately)");
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("health endpoint: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        // A failed accept must never take the endpoint down:
                        // losing observability during an incident is exactly
                        // when it is least affordable.
                        tracing::warn!(error = %e, "health endpoint: accept failed");
                        continue;
                    }
                };
                let source = Arc::clone(&source);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(req, Arc::clone(&source)));
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        tracing::debug!(%peer, error = %e, "health connection ended");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
