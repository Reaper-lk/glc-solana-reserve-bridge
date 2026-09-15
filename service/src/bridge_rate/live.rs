//! The live bridge-rate book (docs/38-elastic-bridge-rate.md, Phase 2B):
//! one [`PriceHistory`] per rail, fed by the feed poller, read by every
//! pricing site. This is the only place a rail price and a route rate are
//! turned into a verdict.
//!
//! # What a rail is
//!
//! A rail is a [`Chain`]: Goldcoin L1, Solana, Robinhood Chain. Each has
//! exactly one USD price series. A route's rate is the ratio of its source
//! rail's smoothed price to its destination rail's, so the six routes are
//! three rails read pairwise — and a rail that cannot be priced takes
//! every route touching it down, and only those.
//!
//! # The verdicts, in order
//!
//! For a route at `now`, each of its two rails must produce a smoothed
//! price over the CURRENT window `[now − W, now)` AND over the REFERENCE
//! window `[now − 2W, now − W)`:
//!
//! 1. No samples, or the feed's last attempt failed and nothing usable is
//!    held → [`RateRefusal::FeedUnavailable`].
//! 2. The newest sample is older than the staleness bound →
//!    [`RateRefusal::FeedStale`].
//! 3. Either window is not covered by gap-free history →
//!    [`RateRefusal::WarmingUp`]. This is the founder's restart rule
//!    (J-6): after a start or a feed gap the route stays halted until two
//!    full windows of continuous history exist, because the band check
//!    needs the reference and nothing may stand in for it.
//! 4. Otherwise the rate is struck, and its movement against the
//!    reference rate is measured in basis points; movement STRICTLY
//!    GREATER than the configured band is a breach ([`RouteRate::
//!    band_exceeded`]). Exactly at the band is not a breach — the
//!    documented boundary rule.
//!
//! A breach does not refuse the rate: the quote is struck and returned
//! flagged, because a deposit observed under a breach must be parked
//! WITH its locked quote, never lost (Phase 2B "Band check").
//!
//! # No fallback
//!
//! Nothing here ever answers with a previous price. A rail whose feed
//! stopped answering ages into `FeedStale` and stays there; a rail whose
//! samples have a hole ages into `WarmingUp` until the hole leaves both
//! windows. The history is the only state, and the history only ever
//! contains what a feed actually printed.

use std::sync::Mutex;

use super::bigmath::{mul_div, U256};
use super::smoothing::{Coverage, PriceHistory, Sample};
use super::{RailPrices, PRICE_SCALE};
use crate::routes::{Chain, Route};

/// The live book's tunables, from `[bridge_rate]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveRateConfig {
    /// `W` above: the smoothing window and the reference distance.
    pub price_window_secs: i64,
    /// A newest sample older than this at evaluation time is stale; a gap
    /// wider than this inside a window invalidates the history behind it.
    pub price_staleness_secs: i64,
    /// The band, in basis points of the reference rate (25% = 2 500).
    pub rate_band_bps: u64,
}

/// Why a route cannot be priced right now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RateRefusal {
    #[error("bridge rate unavailable: the {} price feed has no usable sample ({detail})", .chain.as_str())]
    FeedUnavailable { chain: Chain, detail: String },
    #[error(
        "bridge rate unavailable: the {} price feed is stale ({age_secs}s old, bound \
         {staleness_secs}s)",
        .chain.as_str()
    )]
    FeedStale {
        chain: Chain,
        age_secs: i64,
        staleness_secs: i64,
    },
    #[error(
        "bridge rate warming up: the {} price history does not yet cover two full windows \
         without a gap (covered from {covered_from:?}, needed from {needed_from})",
        .chain.as_str()
    )]
    WarmingUp {
        chain: Chain,
        covered_from: Option<i64>,
        needed_from: i64,
    },
}

impl RateRefusal {
    pub fn chain(&self) -> Chain {
        match self {
            RateRefusal::FeedUnavailable { chain, .. }
            | RateRefusal::FeedStale { chain, .. }
            | RateRefusal::WarmingUp { chain, .. } => *chain,
        }
    }

    /// The stable identifier the API's `availability_reason` and the
    /// ManualReview note carry.
    pub fn reason(&self) -> &'static str {
        match self {
            RateRefusal::FeedUnavailable { .. } => REASON_FEED_UNAVAILABLE,
            RateRefusal::FeedStale { .. } => REASON_FEED_STALE,
            RateRefusal::WarmingUp { .. } => REASON_WARMING_UP,
        }
    }
}

pub const REASON_FEED_UNAVAILABLE: &str = "bridge_rate_feed_unavailable";
pub const REASON_FEED_STALE: &str = "bridge_rate_feed_stale";
pub const REASON_WARMING_UP: &str = "bridge_rate_warming_up";
pub const REASON_BAND_EXCEEDED: &str = "bridge_rate_band_exceeded";

/// A struck route rate: the two smoothed prices, the reference they are
/// measured against, and the band verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteRate {
    pub prices: RailPrices,
    pub reference_source_price_e12: u64,
    pub reference_destination_price_e12: u64,
    /// `|rate_now − rate_ref| / rate_ref`, in basis points, floored.
    pub movement_bps: u64,
    pub band_bps: u64,
    pub band_exceeded: bool,
}

impl RouteRate {
    /// `source / destination` scaled by `PRICE_SCALE`, for display.
    pub fn rate_e12(&self) -> Option<u64> {
        mul_div(
            u128::from(self.prices.source_price_e12),
            u128::from(PRICE_SCALE),
            u128::from(self.prices.destination_price_e12),
        )
        .and_then(|r| u64::try_from(r).ok())
    }
}

/// One rail's read-only state for operators (`/bridge-rate`, `/metrics`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailSnapshot {
    pub chain: Chain,
    pub status: RailStatus,
    pub smoothed_price_e12: Option<u64>,
    pub reference_price_e12: Option<u64>,
    pub newest_feed_at: Option<i64>,
    pub newest_observed_at: Option<i64>,
    pub sample_count: usize,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailStatus {
    Ok,
    Empty,
    Stale,
    WarmingUp,
}

impl RailStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RailStatus::Ok => "ok",
            RailStatus::Empty => "empty",
            RailStatus::Stale => "stale",
            RailStatus::WarmingUp => "warming_up",
        }
    }
}

#[derive(Debug, Default)]
struct RailState {
    history: PriceHistory,
    last_error: Option<(i64, String)>,
}

/// The live book. Cheap to share (`Arc`), locked only for the microseconds
/// a read or a sample write takes.
#[derive(Debug)]
pub struct LiveBook {
    config: LiveRateConfig,
    rails: Mutex<[RailState; 3]>,
}

fn rail_index(chain: Chain) -> usize {
    match chain {
        Chain::Goldcoin => 0,
        Chain::Solana => 1,
        Chain::Robinhood => 2,
    }
}

impl LiveBook {
    pub fn new(config: LiveRateConfig) -> LiveBook {
        LiveBook {
            config,
            rails: Mutex::new(Default::default()),
        }
    }

    pub fn config(&self) -> LiveRateConfig {
        self.config
    }

    /// The oldest instant any evaluation at `now` can reach: the start of
    /// the reference window, less one staleness bound so the gap rule can
    /// look across that boundary.
    fn retention_horizon(&self, now: i64) -> i64 {
        now - 2 * self.config.price_window_secs - self.config.price_staleness_secs
    }

    /// Records a usable sample for `chain` and prunes what can no longer
    /// matter. Returns whether the sample was newer than the newest held.
    pub fn record_sample(&self, chain: Chain, sample: Sample) -> bool {
        let mut rails = self.rails.lock().unwrap_or_else(|p| p.into_inner());
        let rail = &mut rails[rail_index(chain)];
        let kept = rail.history.push(sample);
        rail.history
            .prune(self.retention_horizon(sample.observed_at));
        kept
    }

    /// Records that the feed for `chain` failed at `at`. The history is
    /// untouched — a failure adds nothing and removes nothing; it is the
    /// ABSENCE of new samples that ages the rail into staleness.
    pub fn record_failure(&self, chain: Chain, at: i64, detail: String) {
        let mut rails = self.rails.lock().unwrap_or_else(|p| p.into_inner());
        rails[rail_index(chain)].last_error = Some((at, detail));
    }

    fn rail_prices(
        &self,
        rail: &RailState,
        chain: Chain,
        now: i64,
    ) -> Result<(u64, u64), RateRefusal> {
        let w = self.config.price_window_secs;
        let staleness = self.config.price_staleness_secs;
        let current = rail.history.twap(now, w, now, staleness);
        let reference = rail.history.twap(now - w, w, now, staleness);
        let needed_from = now - 2 * w;
        match (current, reference) {
            (Coverage::Ok { price_e12: c }, Coverage::Ok { price_e12: r }) => Ok((c, r)),
            (Coverage::Empty, _) | (_, Coverage::Empty) => Err(RateRefusal::FeedUnavailable {
                chain,
                detail: rail
                    .last_error
                    .as_ref()
                    .map(|(at, e)| format!("no samples; last feed error at {at}: {e}"))
                    .unwrap_or_else(|| "no samples yet".to_string()),
            }),
            (Coverage::Stale { age_secs, .. }, _) | (_, Coverage::Stale { age_secs, .. }) => {
                Err(RateRefusal::FeedStale {
                    chain,
                    age_secs,
                    staleness_secs: staleness,
                })
            }
            (Coverage::WarmingUp { covered_from }, _)
            | (_, Coverage::WarmingUp { covered_from }) => Err(RateRefusal::WarmingUp {
                chain,
                covered_from,
                needed_from,
            }),
        }
    }

    /// The rate for `route` at `now`, or why there is none.
    pub fn route_rate(&self, route: Route, now: i64) -> Result<RouteRate, RateRefusal> {
        let rails = self.rails.lock().unwrap_or_else(|p| p.into_inner());
        let source_chain = route.source_chain();
        let destination_chain = route.destination_chain();
        let source = &rails[rail_index(source_chain)];
        let destination = &rails[rail_index(destination_chain)];
        let (src_now, src_ref) = self.rail_prices(source, source_chain, now)?;
        let (dst_now, dst_ref) = self.rail_prices(destination, destination_chain, now)?;
        let movement_bps = movement_bps(src_now, dst_now, src_ref, dst_ref);
        let source_feed_at = source.history.newest().map(|s| s.feed_at).unwrap_or(now);
        let destination_feed_at = destination
            .history
            .newest()
            .map(|s| s.feed_at)
            .unwrap_or(now);
        Ok(RouteRate {
            prices: RailPrices {
                source_price_e12: src_now,
                destination_price_e12: dst_now,
                source_feed_at,
                destination_feed_at,
            },
            reference_source_price_e12: src_ref,
            reference_destination_price_e12: dst_ref,
            movement_bps,
            band_bps: self.config.rate_band_bps,
            band_exceeded: movement_bps > self.config.rate_band_bps,
        })
    }

    /// Every rail's state at `now`, for operators.
    pub fn snapshots(&self, now: i64) -> Vec<RailSnapshot> {
        let rails = self.rails.lock().unwrap_or_else(|p| p.into_inner());
        Chain::ALL
            .iter()
            .map(|&chain| {
                let rail = &rails[rail_index(chain)];
                let w = self.config.price_window_secs;
                let staleness = self.config.price_staleness_secs;
                let current = rail.history.twap(now, w, now, staleness);
                let reference = rail.history.twap(now - w, w, now, staleness);
                let status = match (current, reference) {
                    (Coverage::Ok { .. }, Coverage::Ok { .. }) => RailStatus::Ok,
                    (Coverage::Empty, _) => RailStatus::Empty,
                    (Coverage::Stale { .. }, _) => RailStatus::Stale,
                    _ => RailStatus::WarmingUp,
                };
                let newest = rail.history.newest();
                RailSnapshot {
                    chain,
                    status,
                    smoothed_price_e12: current.price_e12(),
                    reference_price_e12: reference.price_e12(),
                    newest_feed_at: newest.map(|s| s.feed_at),
                    newest_observed_at: newest.map(|s| s.observed_at),
                    sample_count: rail.history.len(),
                    last_error: rail.last_error.as_ref().map(|(_, e)| e.clone()),
                    last_error_at: rail.last_error.as_ref().map(|(at, _)| *at),
                }
            })
            .collect()
    }
}

/// `floor(|src_now/dst_now − src_ref/dst_ref| / (src_ref/dst_ref) × 10 000)`
/// computed as `|src_now·dst_ref − src_ref·dst_now| × 10 000 / (src_ref·dst_now)`
/// over exact 256-bit products. Saturates at `u64::MAX` (a movement no
/// band could admit) rather than failing.
pub fn movement_bps(src_now: u64, dst_now: u64, src_ref: u64, dst_ref: u64) -> u64 {
    let lhs = U256::mul_u128(u128::from(src_now), u128::from(dst_ref));
    let rhs = U256::mul_u128(u128::from(src_ref), u128::from(dst_now));
    let (diff, base) = if lhs >= rhs {
        (sub(lhs, rhs), rhs)
    } else {
        (sub(rhs, lhs), rhs)
    };
    if base == U256::ZERO {
        return u64::MAX;
    }
    // diff × 10 000 / base. diff < 2^128 in every realistic case (two
    // u64 prices), so route through mul_div; if it is not, saturate.
    if diff.hi != 0 || base.hi != 0 {
        // Both huge: shift both down by the same amount to fit u128 —
        // the ratio survives to well within a basis point.
        let shift = 128 - (base.hi.leading_zeros().min(diff.hi.leading_zeros()));
        let d = (diff.hi << (128 - shift)) | (diff.lo >> shift);
        let b = (base.hi << (128 - shift)) | (base.lo >> shift);
        return mul_div(d, 10_000, b.max(1))
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(u64::MAX);
    }
    mul_div(diff.lo, 10_000, base.lo)
        .and_then(|v| u64::try_from(v).ok())
        .unwrap_or(u64::MAX)
}

fn sub(a: U256, b: U256) -> U256 {
    let (lo, borrow) = a.lo.overflowing_sub(b.lo);
    U256 {
        hi: a.hi - b.hi - u128::from(borrow),
        lo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i64 = 360;
    const STALE: i64 = 120;

    fn config() -> LiveRateConfig {
        LiveRateConfig {
            price_window_secs: W,
            price_staleness_secs: STALE,
            rate_band_bps: 2_500,
        }
    }

    fn sample(t: i64, price_e12: u64) -> Sample {
        Sample {
            feed_at: t,
            observed_at: t,
            price_e12,
        }
    }

    /// Feeds `chain` a constant price every 30 s from `from` to `to`.
    fn feed_constant(book: &LiveBook, chain: Chain, from: i64, to: i64, price: u64) {
        let mut t = from;
        while t <= to {
            book.record_sample(chain, sample(t, price));
            t += 30;
        }
    }

    const ALL_ROUTES: [Route; 6] = [
        Route::GlcToSol,
        Route::SolToGlc,
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ];

    #[test]
    fn a_fresh_book_refuses_every_route_as_unavailable() {
        let book = LiveBook::new(config());
        for route in ALL_ROUTES {
            assert!(matches!(
                book.route_rate(route, 1_000),
                Err(RateRefusal::FeedUnavailable { .. })
            ));
        }
    }

    #[test]
    fn a_rail_warms_up_only_after_two_full_gap_free_windows() {
        let book = LiveBook::new(config());
        // Every rail fed from t=0.
        for chain in Chain::ALL {
            feed_constant(&book, chain, 0, 700, PRICE_SCALE);
        }
        // At t=700 the reference window is [-20, 340): not covered.
        assert!(matches!(
            book.route_rate(Route::GlcToSol, 700),
            Err(RateRefusal::WarmingUp {
                chain: Chain::Goldcoin,
                ..
            })
        ));
        // At t=720 the reference window is [0, 360): covered exactly.
        let rate = book.route_rate(Route::GlcToSol, 720).unwrap();
        assert_eq!(rate.prices.source_price_e12, PRICE_SCALE);
        assert_eq!(rate.movement_bps, 0);
        assert!(!rate.band_exceeded);
        assert_eq!(rate.rate_e12(), Some(PRICE_SCALE));
    }

    #[test]
    fn only_the_routes_touching_a_dark_rail_halt() {
        let book = LiveBook::new(config());
        for chain in Chain::ALL {
            feed_constant(&book, chain, 0, 720, PRICE_SCALE);
        }
        // Solana goes dark after t=720; Goldcoin and Robinhood keep going.
        feed_constant(&book, Chain::Goldcoin, 750, 1_200, PRICE_SCALE);
        feed_constant(&book, Chain::Robinhood, 750, 1_200, PRICE_SCALE);
        let now = 1_200;
        for route in [
            Route::GlcToSol,
            Route::SolToGlc,
            Route::SolToRhn,
            Route::RhnToSol,
        ] {
            assert!(
                matches!(
                    book.route_rate(route, now),
                    Err(RateRefusal::FeedStale {
                        chain: Chain::Solana,
                        ..
                    })
                ),
                "{route:?}"
            );
        }
        for route in [Route::GlcToRhn, Route::RhnToGlc] {
            assert!(book.route_rate(route, now).is_ok(), "{route:?}");
        }
        // Solana comes back: stale until fresh, then warming up until the
        // hole leaves the reference window, then open — no fallback ever.
        book.record_sample(Chain::Solana, sample(1_210, PRICE_SCALE));
        assert!(matches!(
            book.route_rate(Route::GlcToSol, 1_210),
            Err(RateRefusal::WarmingUp {
                chain: Chain::Solana,
                ..
            })
        ));
        feed_constant(&book, Chain::Solana, 1_240, 1_930, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 1_230, 1_930, PRICE_SCALE);
        assert!(book.route_rate(Route::GlcToSol, 1_930).is_ok());
    }

    #[test]
    fn a_feed_failure_is_reported_but_never_replaces_a_sample() {
        let book = LiveBook::new(config());
        book.record_failure(Chain::Solana, 5, "HTTP 503".to_string());
        let err = book.route_rate(Route::SolToGlc, 10).unwrap_err();
        assert!(matches!(
            err,
            RateRefusal::FeedUnavailable {
                chain: Chain::Solana,
                ..
            }
        ));
        assert!(err.to_string().contains("HTTP 503"));
        let snap = &book.snapshots(10)[1];
        assert_eq!(snap.chain, Chain::Solana);
        assert_eq!(snap.status, RailStatus::Empty);
        assert_eq!(snap.last_error.as_deref(), Some("HTTP 503"));
        assert_eq!(snap.sample_count, 0);
    }

    #[test]
    fn the_band_compares_the_current_rate_against_one_window_ago() {
        // Goldcoin: 1.00 throughout the reference window, then 1.25 for the
        // whole current window; Solana constant. Rate moves 25.00% —
        // exactly the band, which is NOT a breach; one basis point more is.
        let book = LiveBook::new(config());
        feed_constant(&book, Chain::Solana, 0, 720, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 0, 359, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 360, 720, 1_250_000_000_000);
        let rate = book.route_rate(Route::GlcToSol, 720).unwrap();
        assert_eq!(rate.reference_source_price_e12, PRICE_SCALE);
        assert_eq!(rate.prices.source_price_e12, 1_250_000_000_000);
        assert_eq!(rate.movement_bps, 2_500);
        assert!(!rate.band_exceeded, "exactly at the band is admitted");
        // The reciprocal route sees the inverse move: 1/1.25 = 0.8 -> 20%.
        let inverse = book.route_rate(Route::SolToGlc, 720).unwrap();
        assert_eq!(inverse.movement_bps, 2_000);
        assert!(!inverse.band_exceeded);

        let book = LiveBook::new(config());
        feed_constant(&book, Chain::Solana, 0, 720, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 0, 359, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 360, 720, 1_250_100_000_000);
        let rate = book.route_rate(Route::GlcToSol, 720).unwrap();
        assert_eq!(rate.movement_bps, 2_501);
        assert!(rate.band_exceeded, "25.01% breaches");
        // Routes not touching Goldcoin are unaffected.
        feed_constant(&book, Chain::Robinhood, 0, 720, PRICE_SCALE);
        assert!(!book.route_rate(Route::SolToRhn, 720).unwrap().band_exceeded);
        // 24.9% is admitted.
        let book = LiveBook::new(config());
        feed_constant(&book, Chain::Solana, 0, 720, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 0, 359, PRICE_SCALE);
        feed_constant(&book, Chain::Goldcoin, 360, 720, 1_249_000_000_000);
        let rate = book.route_rate(Route::GlcToSol, 720).unwrap();
        assert_eq!(rate.movement_bps, 2_490);
        assert!(!rate.band_exceeded);
    }

    #[test]
    fn movement_is_exact_over_the_full_price_range() {
        assert_eq!(movement_bps(125, 100, 100, 100), 2_500);
        assert_eq!(movement_bps(80, 100, 100, 100), 2_000);
        assert_eq!(movement_bps(100, 125, 100, 100), 2_000);
        assert_eq!(movement_bps(3, 1, 1, 1), 20_000);
        assert_eq!(movement_bps(7, 7, 7, 7), 0);
        assert_eq!(movement_bps(u64::MAX, u64::MAX, u64::MAX, u64::MAX), 0);
        assert_eq!(movement_bps(u64::MAX, 1, u64::MAX, 1), 0);
        assert_eq!(movement_bps(u64::MAX, 1, 1, 1), u64::MAX);
        // 1/3 rate against a 1/2 reference: |1/3-1/2|/(1/2) = 33.33%
        assert_eq!(movement_bps(1, 3, 1, 2), 3_333);
    }

    #[test]
    fn every_route_is_priced_from_its_own_rail_pair_and_reciprocals_invert() {
        let book = LiveBook::new(config());
        feed_constant(&book, Chain::Goldcoin, 0, 720, 4 * PRICE_SCALE);
        feed_constant(&book, Chain::Solana, 0, 720, 2 * PRICE_SCALE);
        feed_constant(&book, Chain::Robinhood, 0, 720, PRICE_SCALE);
        let rate = |route| book.route_rate(route, 720).unwrap().rate_e12().unwrap();
        assert_eq!(rate(Route::GlcToSol), 2 * PRICE_SCALE);
        assert_eq!(rate(Route::SolToGlc), PRICE_SCALE / 2);
        assert_eq!(rate(Route::GlcToRhn), 4 * PRICE_SCALE);
        assert_eq!(rate(Route::RhnToGlc), PRICE_SCALE / 4);
        assert_eq!(rate(Route::SolToRhn), 2 * PRICE_SCALE);
        assert_eq!(rate(Route::RhnToSol), PRICE_SCALE / 2);
    }
}
