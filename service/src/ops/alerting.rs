//! Outbound webhook notification on a reserve-pause transition
//! (docs/16-p0-checkpoint.md P1 item: `ops::health`'s own docs explicitly
//! say "no alerting integration lives here... an operator's existing
//! uptime monitoring raises the alarm" — this module is that missing
//! piece, kept genuinely separate rather than folded into `health` or the
//! tick loop).
//!
//! # Independent of the tick loop, on purpose
//!
//! This polls [`Ledger::is_paused`] on its own interval rather than
//! hooking into [`crate::daemon::run`]/[`crate::orchestrator::Orchestrator::tick`]
//! directly. A pause can be set by `reconciliation::reconcile` (inside a
//! tick) or by an operator's own `glc-admin onchain-pause`/`pause` outside
//! any tick at all — polling the ledger's actual persisted state, the same
//! thing both paths write to, is the one place that is guaranteed to see
//! either, and keeps this module free of any dependency on the tick
//! loop's internals.
//!
//! # Edge-triggered, not level-triggered
//!
//! A webhook fires once, on the `false -> true` transition, not on every
//! poll while a direction stays paused — repeated identical alerts for a
//! condition an operator already knows about and hasn't cleared yet would
//! just be noise, and could look like a fresh, currently-happening
//! incident when the underlying breach might be hours old.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Serialize;

use crate::ledger::{Ledger, LedgerError, ReserveDirection};

#[derive(Debug, Clone)]
pub struct AlertConfig {
    pub webhook_url: String,
    pub poll_interval: Duration,
}

#[derive(Debug, Serialize)]
struct PauseAlert<'a> {
    event: &'a str,
    direction: &'a str,
    timestamp: i64,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One poll: reads current pause state for both directions, returns which
/// ones just transitioned `false -> true` since `previous`, and updates
/// `previous` in place. Pure with respect to the network — never sends
/// anything itself — so it's testable without a real HTTP client or
/// webhook endpoint.
fn detect_new_pauses(
    ledger: &Ledger,
    previous: &mut HashMap<ReserveDirection, bool>,
) -> Result<Vec<ReserveDirection>, LedgerError> {
    let mut newly_paused = Vec::new();
    for direction in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
    ] {
        let paused = ledger.is_paused(direction)?;
        let was_paused = previous.get(&direction).copied().unwrap_or(false);
        if paused && !was_paused {
            newly_paused.push(direction);
        }
        previous.insert(direction, paused);
    }
    Ok(newly_paused)
}

async fn send_alert(http: &reqwest::Client, webhook_url: &str, direction: ReserveDirection) {
    let body = PauseAlert {
        event: "reserve_paused",
        direction: match direction {
            ReserveDirection::GoldcoinReserve => "goldcoin",
            ReserveDirection::SolanaReserve => "solana",
        },
        timestamp: now_unix(),
    };
    match http.post(webhook_url).json(&body).send().await {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!(?direction, "pause alert webhook delivered");
        }
        Ok(resp) => {
            tracing::warn!(?direction, status = %resp.status(), "pause alert webhook rejected");
        }
        Err(e) => {
            tracing::warn!(?direction, error = %e, "pause alert webhook delivery failed");
        }
    }
}

/// Polls until `shutdown` fires. A failure to even open the ledger (or to
/// read pause state) is logged and retried next interval — this task
/// existing to alert on problems must never itself become a second
/// problem that takes the process down.
pub async fn run(
    db_path: PathBuf,
    config: AlertConfig,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let http = reqwest::Client::new();
    let mut previous: HashMap<ReserveDirection, bool> = HashMap::new();
    loop {
        if *shutdown.borrow() {
            return;
        }
        match Ledger::open(&db_path) {
            Ok(ledger) => match detect_new_pauses(&ledger, &mut previous) {
                Ok(newly_paused) => {
                    for direction in newly_paused {
                        send_alert(&http, &config.webhook_url, direction).await;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "alerting: could not read pause state"),
            },
            Err(e) => tracing::warn!(error = %e, "alerting: could not open ledger"),
        }
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(config.poll_interval) => {}
        }
    }
}

#[cfg(test)]
mod tests;
