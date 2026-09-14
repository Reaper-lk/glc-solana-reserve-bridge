//! Bridge quote math and the bridge-rate book (docs/38-elastic-bridge-rate.md).
//!
//! # What a bridge quote is
//!
//! Until this module existed, a request's gross, fee and net were one asset
//! in one unit: `net = gross - fee`, with the only source→destination
//! transform being a decimal conversion (`amount_conversion`). A bridge
//! quote generalises that by one factor — the **bridge rate**, the ratio of
//! the source rail's price to the destination rail's price — and fixes the
//! order in which the three figures are derived:
//!
//! ```text
//! gross_out = floor(gross_in * source_price_e12 / destination_price_e12)
//! fee_out   = floor(gross_out * fee_bps / 10_000)      // the existing fee rule
//! net_out   = gross_out - fee_out
//! ```
//!
//! `gross_in` is the amount the depositor actually sent (anchored to the
//! real deposit exactly as before); `gross_out`, `fee_out` and `net_out` are
//! denominated in the DESTINATION asset, still in the canonical 8-decimal
//! accounting unit. Both prices are fixed-point integers scaled by
//! [`PRICE_SCALE`]; every intermediate product is `u128` and every step is
//! checked, so two processes given the same integers always produce the
//! same integers. No floating point appears anywhere in this module.
//!
//! # Phase 2A: the rate is exactly 1.0
//!
//! This phase ships the math, the persistence and the verification, with
//! both prices pinned to [`PRICE_SCALE`] ([`RateBook::fixed_unit`]). At a
//! unit rate `gross_out == gross_in`, so the fee and net a route produces
//! are bit-for-bit what it produced before — the signer messages, the
//! reserve accounting and the API amounts are unchanged. What changes is
//! that every new request now CARRIES its quote (`bridge_requests.quote_*`,
//! schema v37) and every settlement path re-derives its amounts from that
//! persisted quote ([`verify_quoted_breakdown`]) rather than from the fee
//! rule alone. Phase 2B replaces the fixed prices with live rail prices and
//! adds the halt/band/staleness gates; nothing in a settlement path has to
//! change for that, because the quote a request settles at is the one
//! persisted when its deposit was locked, never a live read.
//!
//! # When a quote is locked
//!
//! - Goldcoin-sourced routes (`GlcToSol`, `GlcToRhn`): the quote returned by
//!   `POST /transfers` is INDICATIVE and is stored on the row unlocked. The
//!   canonical settlement quote is locked when the deposit is first observed
//!   in a block (`Ledger::record_glc_deposit_observed_from`), and unlocked
//!   again if that block is orphaned, so a re-observation re-locks.
//! - Solana- and Robinhood-sourced routes: the fold IS the lock — the
//!   deposit is already final when the row is created.
//!
//! A quoted row whose quote is not locked cannot settle
//! ([`ConversionError::QuoteNotLocked`]).
//!
//! # Fee accounting under a quote (founder decision J-5, option a)
//!
//! `fee_out` is the bridge fee in destination-asset canonical units, and it
//! is the figure `bridge_requests.fee_amount_atomic` stores and
//! `reserve_ledger.accrued_fees_atomic` accrues. The fee is still physically
//! retained on the SOURCE reserve, exactly as before; what the accrued-fee
//! figure now reports is that retention valued in the destination asset at
//! the request's own quoted rate. At a unit rate the two are identical.
//! There is deliberately no second, source-denominated fee figure.

use crate::amount_conversion::{
    compute_fee_at_bps, verify_fee_breakdown, CanonicalAtomic, ConversionError, FeeBreakdown,
};
use crate::routes::Route;

/// Fixed-point scale of every rail price: a price of exactly 1.0 is
/// `PRICE_SCALE`. Twelve decimals is enough headroom for any real per-unit
/// price this bridge will see while keeping a BTC-quoted USD price (~1e5)
/// far inside `u64`.
pub const PRICE_SCALE: u64 = 1_000_000_000_000;

/// The default lifetime of a quote, in seconds, when the config's
/// `[bridge_rate]` section is absent. Metadata only in Phase 2A: nothing
/// reads `quote_expires_at` to make a decision.
pub const DEFAULT_QUOTE_LIFETIME_SECS: i64 = 60;

/// The two rail prices a quote is derived from, plus the timestamps of the
/// feed reads they came from (for audit; at a fixed rate these are simply
/// the quoting instant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RailPrices {
    pub source_price_e12: u64,
    pub destination_price_e12: u64,
    pub source_feed_at: i64,
    pub destination_feed_at: i64,
}

impl RailPrices {
    /// A unit rate (`1.0`) on both rails, read "now".
    pub const fn unit(now: i64) -> RailPrices {
        RailPrices {
            source_price_e12: PRICE_SCALE,
            destination_price_e12: PRICE_SCALE,
            source_feed_at: now,
            destination_feed_at: now,
        }
    }
}

/// One fully derived bridge quote: the prices it was struck at, the amounts
/// they produce for `gross_in`, and when it was struck. This is what a
/// pricing site persists onto a new request (`ledger::RequestAmounts::quote`)
/// and what a deposit observation locks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeQuote {
    pub source_price_e12: u64,
    pub destination_price_e12: u64,
    /// The amount the depositor sends (source asset, canonical units).
    pub gross_in: CanonicalAtomic,
    /// `gross_in` valued in the destination asset at the bridge rate.
    pub gross_out: CanonicalAtomic,
    pub fee_bps: u64,
    /// The bridge fee, destination asset, canonical units.
    pub fee_out: CanonicalAtomic,
    /// `gross_out - fee_out`: what the destination reserve owes.
    pub net_out: CanonicalAtomic,
    pub quoted_at: i64,
    pub quote_expires_at: i64,
    pub source_feed_at: i64,
    pub destination_feed_at: i64,
}

impl BridgeQuote {
    /// The quote's amounts in the shape every settlement path already
    /// consumes. `gross` here is `gross_out` — the destination-asset figure
    /// the fee and net were derived from — never `gross_in`.
    pub fn breakdown(&self) -> FeeBreakdown {
        FeeBreakdown {
            gross: self.gross_out,
            fee_bps: self.fee_bps,
            fee: self.fee_out,
            net: self.net_out,
        }
    }

    /// The bridge rate as a plain decimal string with twelve places
    /// (`"1.000000000000"`), derived from the two prices by integer
    /// arithmetic only. For display; never parsed back.
    pub fn rate_display(&self) -> String {
        format_rate_e12(self.source_price_e12, self.destination_price_e12)
    }

    /// Whether this quote is a unit rate — the only rate Phase 2A produces.
    pub fn is_unit_rate(&self) -> bool {
        self.source_price_e12 == self.destination_price_e12
    }
}

/// `floor(gross_in * source_price_e12 / destination_price_e12)`, the
/// destination-asset value of a source-asset amount at the bridge rate.
/// The one place the rate is applied; everything else derives from its
/// result through the unchanged fee rule.
pub fn gross_out_at_rate(
    gross_in: CanonicalAtomic,
    source_price_e12: u64,
    destination_price_e12: u64,
) -> Result<CanonicalAtomic, ConversionError> {
    if source_price_e12 == 0 || destination_price_e12 == 0 {
        return Err(ConversionError::InvalidBridgePrice {
            source_price_e12,
            destination_price_e12,
        });
    }
    // u64 * u64 < 2^128, and the divisor is nonzero, so neither the
    // multiplication nor the division can fail here; only the narrowing
    // back to u64 can.
    let scaled = u128::from(gross_in.0) * u128::from(source_price_e12);
    let out = scaled / u128::from(destination_price_e12);
    u64::try_from(out)
        .map(CanonicalAtomic)
        .map_err(|_| ConversionError::Overflow(gross_in.0))
}

/// The three-step quote derivation from `gross_in` at the given prices and
/// fee rate: `gross_out`, then the fee rule on `gross_out`, then the net.
/// Returned as a [`FeeBreakdown`] whose `gross` is `gross_out`.
pub fn quoted_breakdown(
    gross_in: CanonicalAtomic,
    source_price_e12: u64,
    destination_price_e12: u64,
    fee_bps: u64,
) -> Result<FeeBreakdown, ConversionError> {
    let gross_out = gross_out_at_rate(gross_in, source_price_e12, destination_price_e12)?;
    compute_fee_at_bps(gross_out, fee_bps)
}

/// Strikes a full [`BridgeQuote`] for `gross_in` at `prices`, fixing its
/// `quoted_at`/`quote_expires_at` from `now` and `quote_lifetime_secs`.
pub fn compute_bridge_quote(
    gross_in: CanonicalAtomic,
    prices: RailPrices,
    fee_bps: u64,
    now: i64,
    quote_lifetime_secs: i64,
) -> Result<BridgeQuote, ConversionError> {
    let breakdown = quoted_breakdown(
        gross_in,
        prices.source_price_e12,
        prices.destination_price_e12,
        fee_bps,
    )?;
    Ok(BridgeQuote {
        source_price_e12: prices.source_price_e12,
        destination_price_e12: prices.destination_price_e12,
        gross_in,
        gross_out: breakdown.gross,
        fee_bps,
        fee_out: breakdown.fee,
        net_out: breakdown.net,
        quoted_at: now,
        quote_expires_at: now.saturating_add(quote_lifetime_secs),
        source_feed_at: prices.source_feed_at,
        destination_feed_at: prices.destination_feed_at,
    })
}

/// The quoted twin of [`verify_fee_breakdown`], and the ONLY way a quoted
/// request's amounts reach a settlement: re-derives `gross_out`, fee and
/// net from the persisted `gross_in`, prices and `fee_bps`, and refuses
/// with [`ConversionError::QuoteMismatch`] unless all three stored figures
/// agree exactly. The returned breakdown is the freshly recomputed one;
/// the stored figures are only ever compared against, never used.
pub fn verify_quoted_breakdown(
    gross_in: u64,
    stored_source_price_e12: u64,
    stored_destination_price_e12: u64,
    stored_fee_bps: u64,
    stored_gross_out: u64,
    stored_fee_out: u64,
    stored_net_out: u64,
) -> Result<FeeBreakdown, ConversionError> {
    let fb = quoted_breakdown(
        CanonicalAtomic(gross_in),
        stored_source_price_e12,
        stored_destination_price_e12,
        stored_fee_bps,
    )?;
    if fb.gross.0 != stored_gross_out || fb.fee.0 != stored_fee_out || fb.net.0 != stored_net_out {
        return Err(ConversionError::QuoteMismatch {
            gross_in,
            source_price_e12: stored_source_price_e12,
            destination_price_e12: stored_destination_price_e12,
            stored_gross_out,
            recomputed_gross_out: fb.gross.0,
            stored_fee: stored_fee_out,
            recomputed_fee: fb.fee.0,
            stored_net: stored_net_out,
            recomputed_net: fb.net.0,
        });
    }
    Ok(fb)
}

/// The quote a request persisted, exactly as its row carries it
/// (`bridge_requests.quote_*`, schema v37). `None` on a `BridgeRequest`
/// means a LEGACY row — created before v37 — which verifies through
/// [`verify_fee_breakdown`] at an implicit unit rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedQuote {
    pub source_price_e12: u64,
    pub destination_price_e12: u64,
    pub gross_out_atomic: u64,
    pub quoted_at: i64,
    pub quote_expires_at: i64,
    pub source_feed_at: i64,
    pub destination_feed_at: i64,
    /// When the quote became the settlement quote. `None` = still the
    /// indicative quote a Goldcoin-sourced request was created with (its
    /// deposit has not been observed, or the observing block was orphaned).
    pub locked_at: Option<i64>,
}

impl PersistedQuote {
    pub fn is_locked(&self) -> bool {
        self.locked_at.is_some()
    }
}

/// THE canonical amount verification for a request row, quoted or legacy.
///
/// Every settlement, recovery and reconciliation path calls this (through
/// `BridgeRequest::verify_breakdown`) instead of choosing between the two
/// verifiers itself:
///
/// - a quoted row must be LOCKED and must reconcile under
///   [`verify_quoted_breakdown`];
/// - a legacy row (no quote) reconciles under [`verify_fee_breakdown`],
///   exactly as it did before v37.
///
/// The returned breakdown's `net` is what settles, in both cases.
pub fn verify_request_amounts(
    quote: Option<&PersistedQuote>,
    gross_amount_atomic: u64,
    fee_bps: u64,
    fee_amount_atomic: u64,
    net_amount_atomic: u64,
) -> Result<FeeBreakdown, ConversionError> {
    match quote {
        None => verify_fee_breakdown(
            gross_amount_atomic,
            fee_bps,
            fee_amount_atomic,
            net_amount_atomic,
        ),
        Some(q) => {
            if !q.is_locked() {
                return Err(ConversionError::QuoteNotLocked {
                    quoted_at: q.quoted_at,
                });
            }
            verify_quoted_breakdown(
                gross_amount_atomic,
                q.source_price_e12,
                q.destination_price_e12,
                fee_bps,
                q.gross_out_atomic,
                fee_amount_atomic,
                net_amount_atomic,
            )
        }
    }
}

/// The net a request WOULD owe for some independently observed gross —
/// the cross-check the Solana completion attestation runs against the
/// on-chain obligation amount. Quoted rows price the gross at their own
/// persisted rate; legacy rows at the fee rule alone.
pub fn expected_net_for_gross(
    quote: Option<&PersistedQuote>,
    gross_in: CanonicalAtomic,
    fee_bps: u64,
) -> Result<CanonicalAtomic, ConversionError> {
    let fb = match quote {
        None => compute_fee_at_bps(gross_in, fee_bps)?,
        Some(q) => quoted_breakdown(
            gross_in,
            q.source_price_e12,
            q.destination_price_e12,
            fee_bps,
        )?,
    };
    Ok(fb.net)
}

/// Where a pricing site gets its rail prices from.
///
/// Phase 2A has exactly one mode, [`RateBook::fixed_unit`]: every route,
/// every time, `1.0` on both rails. Phase 2B adds the live book behind the
/// same `quote` call, so the pricing sites (`POST /transfers`, the three
/// deposit folds, the Goldcoin deposit observation) do not change shape
/// when it lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateBook {
    quote_lifetime_secs: i64,
}

impl RateBook {
    /// A book that answers `1.0` for every route.
    pub fn fixed_unit(quote_lifetime_secs: i64) -> RateBook {
        RateBook {
            quote_lifetime_secs,
        }
    }

    pub fn quote_lifetime_secs(&self) -> i64 {
        self.quote_lifetime_secs
    }

    /// The prices for `route` as of `now`. Infallible in Phase 2A; the
    /// signature is `Result` so a live book can refuse (feed unavailable,
    /// stale, invalid, band exceeded) without changing its callers.
    pub fn prices(&self, _route: Route, now: i64) -> Result<RailPrices, ConversionError> {
        Ok(RailPrices::unit(now))
    }

    /// Strikes a quote for `gross_in` on `route` at `fee_bps`.
    pub fn quote(
        &self,
        route: Route,
        gross_in: CanonicalAtomic,
        fee_bps: u64,
        now: i64,
    ) -> Result<BridgeQuote, ConversionError> {
        let prices = self.prices(route, now)?;
        compute_bridge_quote(gross_in, prices, fee_bps, now, self.quote_lifetime_secs)
    }
}

/// `source / destination` rendered with twelve decimal places by integer
/// arithmetic. A zero destination price (which no quote can carry — see
/// [`gross_out_at_rate`]) renders as `"0.000000000000"` rather than
/// panicking, so a display path can never take a process down.
pub fn format_rate_e12(source_price_e12: u64, destination_price_e12: u64) -> String {
    if destination_price_e12 == 0 {
        return "0.000000000000".to_string();
    }
    let scaled =
        u128::from(source_price_e12) * u128::from(PRICE_SCALE) / u128::from(destination_price_e12);
    let whole = scaled / u128::from(PRICE_SCALE);
    let frac = scaled % u128::from(PRICE_SCALE);
    format!("{whole}.{frac:012}")
}

#[cfg(test)]
mod tests;
