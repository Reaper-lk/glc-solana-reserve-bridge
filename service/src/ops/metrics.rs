//! A minimal metrics registry and Prometheus text encoder
//! (docs/07-implementation-plan.md Phase 5). Ported near-verbatim from the
//! old bridge's `ops/metrics.rs` (docs/01-reuse-inventory.md: hand-rolled
//! registry/encoder reusable as-is — chain-agnostic, no mint/burn or
//! federation coupling at all).
//!
//! Hand-rolled rather than pulling a metrics crate, for the same reason
//! the Goldcoin RPC client is hand-rolled: the requirement is a few dozen
//! gauges and counters rendered as text. There is no dependency here at
//! all.
//!
//! # Format
//!
//! Prometheus text exposition: every operator toolchain reads it, and it
//! needs no schema negotiation. Each metric carries a `# HELP` and
//! `# TYPE` line, because a metric an operator cannot interpret at 3am is
//! not observability.
//!
//! # What is deliberately absent
//!
//! No histograms and no quantiles. They need either client-side bucketing
//! decisions this crate has no basis to make, or unbounded memory. Ages
//! and distributions are exposed as plain gauges (oldest, count per
//! state) and the operator's own scraper does the rest.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A metric's kind, as Prometheus understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Monotonically increasing.
    Counter,
    /// Goes up and down.
    Gauge,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Counter => "counter",
            Kind::Gauge => "gauge",
        }
    }
}

/// One sample: a value plus its label set.
#[derive(Debug, Clone, PartialEq)]
struct Sample {
    /// Sorted so encoding is deterministic — a metrics endpoint that
    /// reorders between scrapes is needlessly hard to diff.
    labels: BTreeMap<String, String>,
    value: f64,
}

#[derive(Debug, Clone)]
struct Family {
    kind: Kind,
    help: String,
    samples: Vec<Sample>,
}

/// A registry built fresh for each scrape.
///
/// Deliberately not a long-lived global: every value here is derived from
/// the ledger or a live chain read at scrape time, so there is no state to
/// keep between scrapes and no risk of a stale gauge outliving what it
/// described.
#[derive(Debug, Default)]
pub struct Registry {
    families: BTreeMap<String, Family>,
}

impl Registry {
    pub fn new() -> Self {
        Registry::default()
    }

    /// Records a sample, creating the family if needed.
    pub fn record(
        &mut self,
        name: &str,
        kind: Kind,
        help: &str,
        labels: &[(&str, &str)],
        value: f64,
    ) {
        let family = self
            .families
            .entry(name.to_string())
            .or_insert_with(|| Family {
                kind,
                help: help.to_string(),
                samples: Vec::new(),
            });
        family.samples.push(Sample {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            value,
        });
    }

    /// Convenience for an unlabelled gauge.
    pub fn gauge(&mut self, name: &str, help: &str, value: f64) {
        self.record(name, Kind::Gauge, help, &[], value);
    }

    /// Convenience for an unlabelled counter.
    pub fn counter(&mut self, name: &str, help: &str, value: f64) {
        self.record(name, Kind::Counter, help, &[], value);
    }

    /// Renders the Prometheus text exposition format.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for (name, family) in &self.families {
            let _ = writeln!(out, "# HELP {name} {}", family.help);
            let _ = writeln!(out, "# TYPE {name} {}", family.kind.as_str());
            for s in &family.samples {
                if s.labels.is_empty() {
                    let _ = writeln!(out, "{name} {}", format_value(s.value));
                } else {
                    let labels: Vec<String> = s
                        .labels
                        .iter()
                        .map(|(k, v)| format!("{k}=\"{}\"", escape(v)))
                        .collect();
                    let _ = writeln!(
                        out,
                        "{name}{{{}}} {}",
                        labels.join(","),
                        format_value(s.value)
                    );
                }
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.families.is_empty()
    }
}

/// Prometheus label values escape backslash, quote and newline. Nothing else.
fn escape(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Renders a value the way Prometheus expects.
///
/// Only the non-finite cases need special handling. Rust's `f64` `Display`
/// never uses scientific notation and already omits the fractional part of
/// an integral value, so `21000000000.0` prints as `21000000000` — exactly
/// what an operator reading atomic-unit amounts needs.
///
/// An earlier version of the old bridge's copy of this file special-cased
/// integers via `v as i64` on the theory that `{}` would produce `2.1e10`.
/// It does not, and the cast was worse than useless: `as i64` **saturates**,
/// so `1e20` rendered as `9223372036854775807` — a plausible-looking wrong
/// number in a metric an operator would act on. Found by a surviving
/// mutant, not by review — kept here as a regression test, not just a
/// comment, since it is trivial to reintroduce.
fn format_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "+Inf" } else { "-Inf" }.to_string()
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests;
