use super::*;
use crate::amount_conversion::{compute_fee_at_bps, BRIDGE_FEE_BPS};

const GLC: u64 = 100_000_000;

fn prices(source_price_e12: u64, destination_price_e12: u64) -> RailPrices {
    RailPrices {
        source_price_e12,
        destination_price_e12,
        source_feed_at: 1_000,
        destination_feed_at: 1_000,
    }
}

// ---------------------------------------------------------------------
// Rate 1.0 — the Phase 2A production rate — is the fee rule, exactly.
// ---------------------------------------------------------------------

#[test]
fn a_unit_rate_reproduces_the_fee_rule_bit_for_bit() {
    // The fee rule itself bounds gross at `u64::MAX / 10_000`; the last
    // value sits just inside it.
    for gross in [
        1u64,
        33,
        34,
        103,
        500_000,
        100 * GLC,
        20_000 * GLC,
        u64::MAX / 10_000,
    ] {
        for fee_bps in [0u64, 100, 300, 600, 9_999, 10_000] {
            let legacy = compute_fee_at_bps(CanonicalAtomic(gross), fee_bps).unwrap();
            let quoted =
                quoted_breakdown(CanonicalAtomic(gross), PRICE_SCALE, PRICE_SCALE, fee_bps)
                    .unwrap();
            assert_eq!(quoted, legacy, "gross {gross} at {fee_bps} bps");
        }
    }
}

#[test]
fn the_fixed_unit_book_quotes_every_route_at_one() {
    let book = RateBook::fixed_unit(60);
    for route in [
        Route::GlcToSol,
        Route::SolToGlc,
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        let q = book
            .quote(
                route,
                CanonicalAtomic(1_000 * GLC),
                BRIDGE_FEE_BPS,
                1_700_000_000,
            )
            .unwrap();
        assert!(q.is_unit_rate());
        assert_eq!(q.rate_display(), "1.000000000000");
        assert_eq!(q.gross_in, q.gross_out);
        assert_eq!(q.fee_out.0, 30 * GLC);
        assert_eq!(q.net_out.0, 970 * GLC);
        assert_eq!(q.quoted_at, 1_700_000_000);
        assert_eq!(q.quote_expires_at, 1_700_000_060);
        assert_eq!(q.source_feed_at, 1_700_000_000);
        assert_eq!(q.destination_feed_at, 1_700_000_000);
        assert_eq!(
            q.breakdown(),
            compute_fee_at_bps(CanonicalAtomic(1_000 * GLC), BRIDGE_FEE_BPS).unwrap()
        );
    }
}

// ---------------------------------------------------------------------
// Synthetic rates — the math Phase 2B will drive with live prices.
// ---------------------------------------------------------------------

#[test]
fn rate_0_8_values_the_deposit_at_four_fifths_before_the_fee() {
    // source 0.80, destination 1.00
    let q = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(800_000_000_000, PRICE_SCALE),
        300,
        0,
        60,
    )
    .unwrap();
    assert_eq!(q.rate_display(), "0.800000000000");
    assert_eq!(q.gross_out.0, 800 * GLC);
    assert_eq!(q.fee_out.0, 24 * GLC);
    assert_eq!(q.net_out.0, 776 * GLC);
    assert_eq!(q.gross_out.0, q.fee_out.0 + q.net_out.0);
}

#[test]
fn rate_1_25_values_the_deposit_at_five_quarters_before_the_fee() {
    // source 1.25, destination 1.00 — and the same rate spelled as
    // 1.00 / 0.80, which must be the identical quote.
    let a = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(1_250_000_000_000, PRICE_SCALE),
        300,
        0,
        60,
    )
    .unwrap();
    let b = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(PRICE_SCALE, 800_000_000_000),
        300,
        0,
        60,
    )
    .unwrap();
    assert_eq!(a.rate_display(), "1.250000000000");
    assert_eq!(b.rate_display(), "1.250000000000");
    assert_eq!(a.gross_out.0, 1_250 * GLC);
    assert_eq!(a.fee_out.0, 37 * GLC + 50_000_000);
    assert_eq!(a.net_out.0, 1_212 * GLC + 50_000_000);
    assert_eq!(
        (a.gross_out, a.fee_out, a.net_out),
        (b.gross_out, b.fee_out, b.net_out)
    );
}

#[test]
fn a_non_terminating_rate_floors_the_destination_gross_and_never_rounds_up() {
    // 1/3: source 1.00, destination 3.00.
    let q = compute_bridge_quote(
        CanonicalAtomic(1_000 * GLC),
        prices(PRICE_SCALE, 3 * PRICE_SCALE),
        300,
        0,
        60,
    )
    .unwrap();
    assert_eq!(q.rate_display(), "0.333333333333");
    // 100_000_000_000 / 3 = 33_333_333_333.33.. -> floored
    assert_eq!(q.gross_out.0, 33_333_333_333);
    assert_eq!(q.fee_out.0, 999_999_999); // floor(33_333_333_333 * 0.03)
    assert_eq!(q.net_out.0, 32_333_333_334);
    assert_eq!(q.gross_out.0, q.fee_out.0 + q.net_out.0);
    // One atomic unit of source at 1/3 is worth nothing at the
    // destination — floored to zero, never rounded to one.
    let dust = quoted_breakdown(CanonicalAtomic(1), PRICE_SCALE, 3 * PRICE_SCALE, 300).unwrap();
    assert_eq!(dust.gross.0, 0);
    assert_eq!(dust.net.0, 0);
}

#[test]
fn gross_out_is_derived_by_integer_arithmetic_only_and_is_deterministic() {
    // A small LCG over (amount, prices, fee): the same integers must give
    // the same integers, run after run, and every result must satisfy the
    // structural identities.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..10_000 {
        // Bounded so `gross_out * fee_bps` stays inside the fee rule's own
        // u64 headroom: gross_in < 1e14, rate <= 10 -> gross_out < 1e15.
        let gross_in = next() % (1_000_000 * GLC);
        let src = 1 + next() % (10 * PRICE_SCALE);
        let dst = PRICE_SCALE + next() % (10 * PRICE_SCALE);
        let fee_bps = next() % 10_001;
        let first = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps).unwrap();
        let second = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.gross.0, first.fee.0 + first.net.0);
        let expected_gross_out = (u128::from(gross_in) * u128::from(src) / u128::from(dst)) as u64;
        assert_eq!(first.gross.0, expected_gross_out);
        assert_eq!(
            u128::from(first.fee.0),
            u128::from(expected_gross_out) * u128::from(fee_bps) / 10_000
        );
        assert!(verify_quoted_breakdown(
            gross_in,
            src,
            dst,
            fee_bps,
            first.gross.0,
            first.fee.0,
            first.net.0
        )
        .is_ok());
    }
}

// ---------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------

#[test]
fn a_zero_price_on_either_rail_is_refused_before_any_amount_is_derived() {
    assert!(matches!(
        gross_out_at_rate(CanonicalAtomic(1), 0, PRICE_SCALE),
        Err(ConversionError::InvalidBridgePrice { .. })
    ));
    assert!(matches!(
        gross_out_at_rate(CanonicalAtomic(1), PRICE_SCALE, 0),
        Err(ConversionError::InvalidBridgePrice { .. })
    ));
    assert!(matches!(
        RateBook::fixed_unit(60).quote(Route::GlcToSol, CanonicalAtomic(5), 10_001, 0),
        Err(ConversionError::FeeBpsOutOfRange { .. })
    ));
}

#[test]
fn a_gross_out_that_does_not_fit_u64_is_an_overflow_not_a_wrap() {
    // u64::MAX at a rate of 2.0 overflows; at 1.0 it does not.
    assert!(matches!(
        gross_out_at_rate(CanonicalAtomic(u64::MAX), 2 * PRICE_SCALE, PRICE_SCALE),
        Err(ConversionError::Overflow(_))
    ));
    assert_eq!(
        gross_out_at_rate(CanonicalAtomic(u64::MAX), PRICE_SCALE, PRICE_SCALE).unwrap(),
        CanonicalAtomic(u64::MAX)
    );
    // The intermediate product of two u64s cannot itself overflow u128.
    assert_eq!(
        gross_out_at_rate(CanonicalAtomic(u64::MAX), u64::MAX, u64::MAX).unwrap(),
        CanonicalAtomic(u64::MAX)
    );
}

// ---------------------------------------------------------------------
// Verification: every stored figure must reproduce, or nothing settles.
// ---------------------------------------------------------------------

fn locked(gross_in: u64, src: u64, dst: u64, fee_bps: u64) -> (PersistedQuote, u64, u64) {
    let fb = quoted_breakdown(CanonicalAtomic(gross_in), src, dst, fee_bps).unwrap();
    (
        PersistedQuote {
            source_price_e12: src,
            destination_price_e12: dst,
            gross_out_atomic: fb.gross.0,
            quoted_at: 100,
            quote_expires_at: 160,
            source_feed_at: 100,
            destination_feed_at: 100,
            locked_at: Some(100),
        },
        fb.fee.0,
        fb.net.0,
    )
}

#[test]
fn a_consistent_quoted_row_verifies_and_the_recomputed_net_is_what_settles() {
    let (q, fee, net) = locked(1_000 * GLC, 800_000_000_000, PRICE_SCALE, 300);
    let fb = verify_request_amounts(Some(&q), 1_000 * GLC, 300, fee, net).unwrap();
    assert_eq!(fb.gross.0, 800 * GLC);
    assert_eq!(fb.net.0, 776 * GLC);
}

#[test]
fn tampering_with_any_persisted_quote_figure_is_refused() {
    let (q, fee, net) = locked(1_000 * GLC, PRICE_SCALE, PRICE_SCALE, 300);
    let ok = |q: &PersistedQuote, fee: u64, net: u64| {
        verify_request_amounts(Some(q), 1_000 * GLC, 300, fee, net)
    };
    assert!(ok(&q, fee, net).is_ok());

    let mismatch = |r: Result<FeeBreakdown, ConversionError>| {
        matches!(r, Err(ConversionError::QuoteMismatch { .. }))
    };
    // gross_out
    let mut t = q;
    t.gross_out_atomic += 1;
    assert!(mismatch(ok(&t, fee, net)));
    // fee (net untouched -> gross != fee + net)
    assert!(mismatch(ok(&q, fee - 1, net)));
    // net
    assert!(mismatch(ok(&q, fee, net + 1)));
    // a rewritten source price that no longer produces the stored gross_out
    let mut t = q;
    t.source_price_e12 = 1_250_000_000_000;
    assert!(mismatch(ok(&t, fee, net)));
    // a rewritten destination price, same
    let mut t = q;
    t.destination_price_e12 = 800_000_000_000;
    assert!(mismatch(ok(&t, fee, net)));
    // a consistent rewrite of EVERYTHING at a different rate still cannot
    // pass off the old fee/net: the stored fee/net were struck at 1.0
    let mut t = q;
    t.source_price_e12 = 1_250_000_000_000;
    t.gross_out_atomic = 1_250 * GLC;
    assert!(mismatch(ok(&t, fee, net)));
}

#[test]
fn an_unlocked_quote_cannot_settle() {
    let (mut q, fee, net) = locked(1_000 * GLC, PRICE_SCALE, PRICE_SCALE, 300);
    q.locked_at = None;
    assert!(matches!(
        verify_request_amounts(Some(&q), 1_000 * GLC, 300, fee, net),
        Err(ConversionError::QuoteNotLocked { quoted_at: 100 })
    ));
}

#[test]
fn a_legacy_row_verifies_exactly_as_before_v37() {
    let fb = compute_fee_at_bps(CanonicalAtomic(500_000), 600).unwrap();
    assert_eq!(
        verify_request_amounts(None, 500_000, 600, fb.fee.0, fb.net.0).unwrap(),
        fb
    );
    assert!(matches!(
        verify_request_amounts(None, 500_000, 600, fb.fee.0 + 1, fb.net.0 - 1),
        Err(ConversionError::AccountingMismatch { .. })
    ));
}

#[test]
fn the_expected_net_for_an_observed_gross_is_priced_at_the_persisted_rate() {
    let (q, _, _) = locked(1_000 * GLC, 800_000_000_000, PRICE_SCALE, 300);
    // Quoted: the on-chain gross is valued at 0.8 first.
    assert_eq!(
        expected_net_for_gross(Some(&q), CanonicalAtomic(1_000 * GLC), 300).unwrap(),
        CanonicalAtomic(776 * GLC)
    );
    // Legacy: the fee rule alone.
    assert_eq!(
        expected_net_for_gross(None, CanonicalAtomic(1_000 * GLC), 300).unwrap(),
        CanonicalAtomic(970 * GLC)
    );
}

#[test]
fn the_rate_renders_with_twelve_places_by_integer_arithmetic() {
    assert_eq!(format_rate_e12(PRICE_SCALE, PRICE_SCALE), "1.000000000000");
    assert_eq!(format_rate_e12(1, PRICE_SCALE), "0.000000000001");
    assert_eq!(
        format_rate_e12(123 * PRICE_SCALE, PRICE_SCALE),
        "123.000000000000"
    );
    assert_eq!(format_rate_e12(PRICE_SCALE, 0), "0.000000000000");
}

// ---------------------------------------------------------------------
// Phase 2A has no feed: the book is pure arithmetic over fixed prices.
// ---------------------------------------------------------------------

#[test]
fn phase_2a_has_no_live_feed_anywhere_in_the_bridge_rate_module() {
    let source = include_str!("../bridge_rate.rs");
    for forbidden in [
        "reqwest",
        "http://",
        "https://",
        "tokio::",
        "RpcClient",
        "eth_call",
    ] {
        assert!(
            !source.contains(forbidden),
            "bridge_rate.rs must not reach a network in Phase 2A (found {forbidden:?})"
        );
    }
    assert_eq!(
        RateBook::fixed_unit(60).prices(Route::RhnToSol, 7).unwrap(),
        RailPrices::unit(7)
    );
}
