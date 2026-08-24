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

/// Builds the invariant list and metric registry from whatever the caller
/// has gathered. Pure: takes measurements, returns a report — everything
/// that touches the ledger or a chain happens in the caller
/// ([`crate::ops::collector::OpsCollector`]), which is what makes this
/// testable without either.
pub fn build_report(
    goldcoin_reserve: Option<ReserveSnapshot>,
    solana_reserve: Option<ReserveSnapshot>,
    manual_review_count: u64,
    goldcoin_indexer: Option<IndexerSummary>,
    solana_indexer: Option<IndexerSummary>,
    extra: &[(&str, f64, &'static str)],
) -> HealthReport {
    let mut invariants = Vec::new();
    let mut r = Registry::new();

    for (label, prefix, snapshot) in [
        ("Goldcoin", "goldcoin", goldcoin_reserve),
        ("Solana", "solana", solana_reserve),
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
