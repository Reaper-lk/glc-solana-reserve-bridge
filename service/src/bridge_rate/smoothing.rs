//! The per-rail price history and its time-weighted average (docs/38-
//! elastic-bridge-rate.md, Phase 2B "Smoothing").
//!
//! # The algorithm, exactly
//!
//! A rail's history is a list of samples `(feed_at, price_e12)` in
//! non-decreasing `feed_at` order. Between two samples the price is taken
//! to be the OLDER sample's price — a step function, which is the only
//! interpolation that never invents a price the feed did not print. The
//! smoothed price over an interval `[a, b)` is the integral of that step
//! function divided by `b - a`:
//!
//! ```text
//! twap([a, b)) = floor( Σ_i price_i × |[t_i, t_{i+1}) ∩ [a, b)| / (b − a) )
//! ```
//!
//! where `t_i` is sample `i`'s `feed_at`, `t_{n}` (past the last sample)
//! is `+∞`, and the sample in force at `a` is the newest one with
//! `feed_at <= a`. Every product is `u128` (a `u64` price times an
//! interval of at most a few thousand seconds) and the final division
//! floors, so the result is an integer function of the integers it was
//! given — two processes holding the same samples compute the same
//! average, whatever the sample cadence.
//!
//! # What the average requires, and the verdicts when it cannot be had
//!
//! An average over `[a, b)` evaluated at `now` is only produced when the
//! history genuinely covers it:
//!
//! - the newest sample is no older than `staleness` at `now` — otherwise
//!   the feed is [`Coverage::Stale`];
//! - a sample is in force at `a` — otherwise there is not yet one full
//!   window of history: [`Coverage::WarmingUp`];
//! - no two consecutive samples that matter to the interval are more than
//!   `staleness` apart — a longer gap means the feed was dark for part of
//!   the window, and an average that papered over it would be exactly the
//!   last-known-price fallback this design forbids: [`Coverage::WarmingUp`]
//!   again, until the gap has aged out of the interval.
//!
//! Only [`Coverage::Ok`] carries a price. Nothing here ever returns the
//! last price it saw when the rules above are not met.
//!
//! # Bounded memory
//!
//! [`PriceHistory::prune`] drops every sample older than the oldest
//! instant any caller can ask about (`now − 2 × window − staleness`),
//! keeping the single sample in force at that instant so the step
//! function stays defined there; and [`MAX_SAMPLES`] caps the list
//! outright, dropping the oldest first, so a feed that answers faster than
//! expected cannot grow the history without bound.

/// Hard cap on retained samples per rail. At the slowest sensible poll
/// cadence (one sample a second) this holds more than the two windows
/// plus staleness the evaluator ever looks at.
pub const MAX_SAMPLES: usize = 4096;

/// One price observation from a feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// When the feed says the price was current (unix seconds).
    pub feed_at: i64,
    /// When this process received it (unix seconds).
    pub observed_at: i64,
    pub price_e12: u64,
}

/// Why a smoothed price could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// A smoothed price over the requested interval.
    Ok { price_e12: u64 },
    /// No sample at all.
    Empty,
    /// The newest sample is older than the staleness bound at `now`.
    Stale { newest_feed_at: i64, age_secs: i64 },
    /// The history does not yet cover the interval without a gap longer
    /// than the staleness bound. `covered_from` is the oldest instant the
    /// gap-free history reaches back to (or `None` if there is none).
    WarmingUp { covered_from: Option<i64> },
}

impl Coverage {
    pub fn price_e12(self) -> Option<u64> {
        match self {
            Coverage::Ok { price_e12 } => Some(price_e12),
            _ => None,
        }
    }
}

/// A rail's retained samples.
#[derive(Debug, Clone, Default)]
pub struct PriceHistory {
    samples: Vec<Sample>,
}

impl PriceHistory {
    pub fn new() -> PriceHistory {
        PriceHistory {
            samples: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn newest(&self) -> Option<Sample> {
        self.samples.last().copied()
    }

    pub fn oldest(&self) -> Option<Sample> {
        self.samples.first().copied()
    }

    /// Records a sample. A sample not newer than the newest one is
    /// dropped: a feed that re-serves an old timestamp has nothing new to
    /// say, and out-of-order data must not rewrite the past. Returns
    /// whether it was kept.
    pub fn push(&mut self, sample: Sample) -> bool {
        if let Some(newest) = self.samples.last() {
            if sample.feed_at <= newest.feed_at {
                return false;
            }
        }
        if self.samples.len() >= MAX_SAMPLES {
            self.samples.remove(0);
        }
        self.samples.push(sample);
        true
    }

    /// Drops everything older than `horizon` except the one sample in
    /// force at `horizon` itself.
    pub fn prune(&mut self, horizon: i64) {
        // Index of the newest sample with feed_at <= horizon; everything
        // before it is unreachable.
        let keep_from = match self.samples.iter().rposition(|s| s.feed_at <= horizon) {
            Some(i) => i,
            None => return,
        };
        if keep_from > 0 {
            self.samples.drain(..keep_from);
        }
    }

    /// The oldest instant from which the history is gap-free (no two
    /// consecutive samples more than `max_gap` apart, walking back from
    /// the newest), or `None` when there are no samples.
    pub fn gap_free_from(&self, max_gap: i64) -> Option<i64> {
        let newest_index = self.samples.len().checked_sub(1)?;
        let mut from = self.samples[newest_index].feed_at;
        for i in (0..newest_index).rev() {
            let older = self.samples[i].feed_at;
            if from - older > max_gap {
                break;
            }
            from = older;
        }
        Some(from)
    }

    /// The smoothed price over `[end - window, end)`, evaluated at `now`
    /// under a `staleness` bound — see the module docs for the exact
    /// rules and the algorithm.
    pub fn twap(&self, end: i64, window: i64, now: i64, staleness: i64) -> Coverage {
        let Some(newest) = self.samples.last() else {
            return Coverage::Empty;
        };
        let age = now - newest.feed_at;
        if age > staleness {
            return Coverage::Stale {
                newest_feed_at: newest.feed_at,
                age_secs: age,
            };
        }
        let start = end - window;
        // The gap-free history must reach back to (at least) `start`.
        let covered_from = self.gap_free_from(staleness);
        match covered_from {
            Some(from) if from <= start => {}
            other => {
                return Coverage::WarmingUp {
                    covered_from: other,
                }
            }
        }
        // Integrate the step function over [start, end).
        let mut weighted: u128 = 0;
        for (i, s) in self.samples.iter().enumerate() {
            let next = self
                .samples
                .get(i + 1)
                .map(|n| n.feed_at)
                .unwrap_or(i64::MAX);
            let lo = s.feed_at.max(start);
            let hi = next.min(end);
            if hi > lo {
                weighted += u128::from(s.price_e12) * (hi - lo) as u128;
            }
        }
        let span = (end - start) as u128;
        // span > 0 by the window bound the config enforces; weighted /
        // span <= max price, so the narrowing cannot fail.
        let price = u64::try_from(weighted / span).unwrap_or(u64::MAX);
        Coverage::Ok { price_e12: price }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(feed_at: i64, price_e12: u64) -> Sample {
        Sample {
            feed_at,
            observed_at: feed_at,
            price_e12,
        }
    }

    fn history(points: &[(i64, u64)]) -> PriceHistory {
        let mut h = PriceHistory::new();
        for (t, p) in points {
            assert!(h.push(s(*t, *p)));
        }
        h
    }

    #[test]
    fn a_constant_price_averages_to_itself_whatever_the_cadence() {
        // Irregular cadence, gaps inside the staleness bound (300).
        let h = history(&[(0, 700), (7, 700), (100, 700), (101, 700), (359, 700)]);
        assert_eq!(h.twap(360, 360, 360, 300), Coverage::Ok { price_e12: 700 });
    }

    #[test]
    fn rising_and_falling_prices_average_by_time_in_force() {
        // 100 for 180 s then 200 for 180 s -> 150.
        let h = history(&[(0, 100), (180, 200)]);
        assert_eq!(h.twap(360, 360, 360, 200), Coverage::Ok { price_e12: 150 });
        // 200 for 180 s then 100 for 180 s -> 150, too: symmetric.
        let h = history(&[(0, 200), (180, 100)]);
        assert_eq!(h.twap(360, 360, 360, 200), Coverage::Ok { price_e12: 150 });
        // A late spike in force for 36 of 360 seconds moves a 100 average
        // by a tenth of the spike.
        let h = history(&[(0, 100), (324, 1_100)]);
        assert_eq!(h.twap(360, 360, 360, 400), Coverage::Ok { price_e12: 200 });
    }

    #[test]
    fn irregular_intervals_weight_each_price_by_exactly_how_long_it_held() {
        // in force: 10 for [0,50), 30 for [50,60), 20 for [60,360)
        let h = history(&[(0, 10), (50, 30), (60, 20)]);
        let expected = (10 * 50 + 30 * 10 + 20 * 300) / 360;
        assert_eq!(
            h.twap(360, 360, 360, 400),
            Coverage::Ok {
                price_e12: expected
            }
        );
        // Floors, never rounds: (1*359 + 2*1)/360 = 1.0027 -> 1
        let h = history(&[(0, 1), (359, 2)]);
        assert_eq!(h.twap(360, 360, 360, 400), Coverage::Ok { price_e12: 1 });
    }

    #[test]
    fn the_sample_in_force_before_the_window_starts_counts_from_the_boundary() {
        // Struck at t=-100, still in force at the window start t=0.
        let h = history(&[(-100, 500), (180, 700)]);
        assert_eq!(h.twap(360, 360, 360, 400), Coverage::Ok { price_e12: 600 });
        // Exactly at the boundary is in force too (feed_at <= start).
        let h = history(&[(0, 500), (180, 700)]);
        assert_eq!(h.twap(360, 360, 360, 400), Coverage::Ok { price_e12: 600 });
        // One second after the boundary is not: the window is not covered.
        let h = history(&[(1, 500), (180, 700)]);
        assert_eq!(
            h.twap(360, 360, 360, 400),
            Coverage::WarmingUp {
                covered_from: Some(1)
            }
        );
    }

    #[test]
    fn a_stale_newest_sample_is_reported_not_averaged() {
        let h = history(&[(0, 500), (100, 500)]);
        assert_eq!(
            h.twap(360, 360, 360, 120),
            Coverage::Stale {
                newest_feed_at: 100,
                age_secs: 260
            }
        );
        assert_eq!(h.twap(220, 120, 220, 120), Coverage::Ok { price_e12: 500 });
        assert_eq!(
            PriceHistory::new().twap(360, 360, 360, 120),
            Coverage::Empty
        );
    }

    #[test]
    fn a_gap_longer_than_the_staleness_bound_invalidates_the_history_behind_it() {
        // Fresh samples since t=300, but a 200-second hole before that: the
        // history is only good from 300, so a window starting at 0 is not
        // covered — even though a sample WAS in force at 0.
        let h = history(&[(0, 500), (100, 500), (300, 500), (350, 500)]);
        assert_eq!(
            h.twap(360, 360, 360, 120),
            Coverage::WarmingUp {
                covered_from: Some(300)
            }
        );
        // Once the window has moved past the hole, it is usable again.
        assert_eq!(h.twap(660, 360, 360, 120), Coverage::Ok { price_e12: 500 });
    }

    #[test]
    fn out_of_order_and_duplicate_samples_are_dropped() {
        let mut h = history(&[(10, 1), (20, 2)]);
        assert!(!h.push(s(20, 3)));
        assert!(!h.push(s(15, 3)));
        assert_eq!(h.len(), 2);
        assert_eq!(h.newest().unwrap().price_e12, 2);
    }

    #[test]
    fn pruning_keeps_the_sample_in_force_at_the_horizon_and_nothing_older() {
        let mut h = history(&[(0, 1), (100, 2), (200, 3), (300, 4)]);
        h.prune(250);
        assert_eq!(h.oldest().unwrap().feed_at, 200);
        assert_eq!(h.len(), 2);
        // Exactly at a sample: that sample is the one in force.
        h.prune(300);
        assert_eq!(h.oldest().unwrap().feed_at, 300);
        // A horizon older than everything prunes nothing.
        let mut h = history(&[(500, 1), (600, 2)]);
        h.prune(100);
        assert_eq!(h.len(), 2);
    }

    #[test]
    fn the_sample_cap_is_enforced_oldest_first() {
        let mut h = PriceHistory::new();
        for t in 0..(MAX_SAMPLES as i64 + 10) {
            h.push(s(t, 1));
        }
        assert_eq!(h.len(), MAX_SAMPLES);
        assert_eq!(h.oldest().unwrap().feed_at, 10);
    }

    #[test]
    fn the_average_is_deterministic_and_cannot_overflow_at_the_price_ceiling() {
        let h = history(&[(0, u64::MAX), (180, u64::MAX)]);
        assert_eq!(
            h.twap(360, 360, 360, 400),
            Coverage::Ok {
                price_e12: u64::MAX
            }
        );
        let a = history(&[(0, 3), (77, 9), (200, 4), (359, 8)]);
        let b = history(&[(0, 3), (77, 9), (200, 4), (359, 8)]);
        assert_eq!(a.twap(360, 360, 360, 400), b.twap(360, 360, 360, 400));
    }
}
