//! Per-route fee tests.
//!
//! The theme throughout: prove each route's rate is INDEPENDENT — that
//! setting one never moves another, and that a route with no rate is an
//! error rather than a number borrowed from somewhere else.

use super::*;
use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic, BRIDGE_FEE_BPS};

/// The four rates the migration's documented fallback produces, and the
/// shape a migrated `[fees]` table has.
fn launch_fees() -> RouteFees {
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, 300).unwrap();
    fees.insert(Route::SolToGlc, 300).unwrap();
    fees.insert(Route::GlcToRhn, 600).unwrap();
    fees.insert(Route::RhnToGlc, 600).unwrap();
    fees
}

#[test]
fn every_executable_route_resolves_exactly_one_rate() {
    let fees = launch_fees();
    // The four pre-Phase-H routes are always required; the two cross
    // routes only once enabled in config.
    fees.covers_required_routes(&crate::routes::RoutesConfig::default())
        .unwrap();
    assert_eq!(
        fees.covers_every_executable_route().unwrap_err(),
        FeeError::MissingFee { route: "SolToRhn" }
    );
    let cross_enabled =
        crate::routes::RoutesConfig::default().with_robinhood(false, false, true, false);
    assert_eq!(
        fees.covers_required_routes(&cross_enabled).unwrap_err(),
        FeeError::MissingFee { route: "SolToRhn" }
    );

    assert_eq!(fees.fee_bps(Route::GlcToSol).unwrap(), 300);
    assert_eq!(fees.fee_bps(Route::SolToGlc).unwrap(), 300);
    assert_eq!(fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(fees.fee_bps(Route::RhnToGlc).unwrap(), 600);
}

#[test]
fn executable_routes_are_derived_from_the_registry_not_listed() {
    // A future route that gains a Direction becomes one that MUST be
    // priced, with no second list to update. That is the whole reason
    // this is derived.
    let derived: Vec<Route> = executable_routes().collect();
    assert_eq!(
        derived,
        vec![
            Route::GlcToSol,
            Route::SolToGlc,
            Route::GlcToRhn,
            Route::RhnToGlc,
            Route::SolToRhn,
            Route::RhnToSol
        ]
    );
    for route in Route::ALL {
        assert_eq!(
            derived.contains(&route),
            route.as_direction().is_some(),
            "{} must be priceable exactly when it is settleable",
            route.as_str()
        );
    }
}

// ------------------------------------------------------- independence --

#[test]
fn glc_to_sol_and_sol_to_glc_are_independent() {
    // The two directions of the SAME pair. Changing one must not move the
    // other: they are separate commercial terms that merely happen to
    // share a chain.
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, 600).unwrap();
    fees.insert(Route::SolToGlc, 100).unwrap();

    assert_eq!(fees.fee_bps(Route::GlcToSol).unwrap(), 600);
    assert_eq!(fees.fee_bps(Route::SolToGlc).unwrap(), 100);
}

#[test]
fn glc_to_rhn_and_rhn_to_glc_are_independent() {
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToRhn, 600).unwrap();
    fees.insert(Route::RhnToGlc, 300).unwrap();

    assert_eq!(fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(fees.fee_bps(Route::RhnToGlc).unwrap(), 300);
}

#[test]
fn a_robinhood_rate_never_reaches_a_solana_route() {
    // The exact leak this module was written to close: before it, a
    // `GlcToRhn` transfer priced at the Solana rate because the pricing
    // call site did not know which route it was pricing.
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, 300).unwrap();
    fees.insert(Route::SolToGlc, 300).unwrap();
    fees.insert(Route::GlcToRhn, 600).unwrap();
    fees.insert(Route::RhnToGlc, 600).unwrap();

    for solana_route in [Route::GlcToSol, Route::SolToGlc] {
        assert_eq!(
            fees.fee_bps(solana_route).unwrap(),
            300,
            "{} must keep its own rate whatever Robinhood charges",
            solana_route.as_str()
        );
    }
    for robinhood_route in [Route::GlcToRhn, Route::RhnToGlc] {
        assert_eq!(
            fees.fee_bps(robinhood_route).unwrap(),
            600,
            "{} must keep its own rate whatever Solana charges",
            robinhood_route.as_str()
        );
    }
}

#[test]
fn changing_one_route_leaves_every_other_route_untouched() {
    // The property the admin flow depends on: "modify only that selected
    // route". Rebuilding the table with one entry changed must leave the
    // other three byte-identical.
    let before = launch_fees();
    let mut after = RouteFees::new();
    for (route, bps) in before.iter() {
        let new_bps = if route == Route::RhnToGlc { 300 } else { bps };
        after.insert(route, new_bps).unwrap();
    }

    assert_eq!(after.fee_bps(Route::RhnToGlc).unwrap(), 300);
    for untouched in [Route::GlcToSol, Route::SolToGlc, Route::GlcToRhn] {
        assert_eq!(
            after.fee_bps(untouched).unwrap(),
            before.fee_bps(untouched).unwrap(),
            "{} must not have moved",
            untouched.as_str()
        );
    }
}

// --------------------------------------------------------- fail closed --

#[test]
fn a_missing_fee_for_an_executable_route_fails_closed() {
    // NOT a fallback. Answering this with BRIDGE_FEE_BPS is the bug.
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, 300).unwrap();

    let err = fees.fee_bps(Route::GlcToRhn).unwrap_err();
    assert_eq!(err, FeeError::MissingFee { route: "GlcToRhn" });
    assert!(
        err.to_string().contains("[fees]"),
        "the refusal must name the remedy, got: {err}"
    );

    let err = fees.covers_every_executable_route().unwrap_err();
    assert!(matches!(err, FeeError::MissingFee { .. }));
}

#[test]
fn an_empty_table_prices_nothing_at_all() {
    let fees = RouteFees::new();
    assert!(fees.is_empty());
    for route in executable_routes() {
        assert!(
            fees.fee_bps(route).is_err(),
            "{} must not resolve a rate from an empty table",
            route.as_str()
        );
    }
}

#[test]
fn cross_routes_are_priced_by_route_and_never_borrow_a_rate() {
    // Since Phase H both cross routes are executable and priceable. A
    // table that does not name one resolves NO rate for it — never the
    // Solana rate, never the Robinhood rate, never the compiled-in
    // constant — so an unpriced cross route folds nothing.
    let mut fees = launch_fees();
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert_eq!(
            fees.fee_bps(route).unwrap_err(),
            FeeError::MissingFee {
                route: route.as_str()
            }
        );
        assert_eq!(fees.get(route), None);
    }
    fees.insert(Route::SolToRhn, 450).unwrap();
    assert_eq!(fees.fee_bps(Route::SolToRhn).unwrap(), 450);
    assert_eq!(fees.get(Route::RhnToSol), None);
    // Pricing one cross route changes nothing about any other rate.
    assert_eq!(fees.fee_bps(Route::GlcToRhn).unwrap(), 600);
    assert_eq!(fees.fee_bps(Route::SolToGlc).unwrap(), 300);
    assert_eq!(
        fees.insert(Route::SolToRhn, 450).unwrap_err(),
        FeeError::DuplicateFee { route: "SolToRhn" }
    );
}

#[test]
fn a_fee_entry_does_not_enable_a_route() {
    // Pricing is not enablement: a priced cross route still defaults to
    // disabled on every gate.
    let mut fees = launch_fees();
    fees.insert(Route::SolToRhn, 600).unwrap();
    fees.insert(Route::RhnToSol, 600).unwrap();
    assert!(!Route::SolToRhn.default_enabled());
    assert!(!Route::RhnToSol.default_enabled());
    assert!(!crate::routes::RoutesConfig::default().enabled(Route::SolToRhn));
    assert!(!crate::routes::RoutesConfig::default().enabled(Route::RhnToSol));
}

// ---------------------------------------------------------- validation --

#[test]
fn zero_is_a_configurable_rate_and_makes_the_route_free() {
    // Explicitly supported. `fee = 0`, `net = gross`; nothing downstream
    // treats it specially. It used to be refused for being "a rate the
    // protocol has never charged", which was the allowlist talking.
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, MIN_FEE_BPS).unwrap();
    assert_eq!(fees.fee_bps(Route::GlcToSol).unwrap(), 0);

    let breakdown = compute_fee_at_bps(CanonicalAtomic(1_000_000_000), 0).unwrap();
    assert_eq!(breakdown.fee.0, 0);
    assert_eq!(breakdown.net.0, 1_000_000_000);
    assert_eq!(breakdown.gross.0, breakdown.fee.0 + breakdown.net.0);
}

#[test]
fn the_maximum_configurable_rate_is_9999_bps() {
    // One basis point below 100%, and that one point is the whole reason:
    // at 10,000 the fee equals the gross and the route delivers nothing.
    assert_eq!(MAX_FEE_BPS, BPS_DENOMINATOR - 1);

    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, MAX_FEE_BPS).unwrap();
    assert_eq!(fees.fee_bps(Route::GlcToSol).unwrap(), 9_999);

    // It still delivers SOMETHING, which is what makes it the maximum.
    let breakdown = compute_fee_at_bps(CanonicalAtomic(1_000_000), MAX_FEE_BPS).unwrap();
    assert_eq!(breakdown.fee.0, 999_900);
    assert_eq!(breakdown.net.0, 100);
}

#[test]
fn a_rate_at_or_above_one_hundred_percent_is_refused() {
    let mut fees = RouteFees::new();
    for bps in [BPS_DENOMINATOR, BPS_DENOMINATOR + 1, u64::MAX] {
        let err = fees.insert(Route::GlcToSol, bps).unwrap_err();
        assert!(
            matches!(err, FeeError::FeeBpsOutOfRange { max: 9_999, .. }),
            "{bps} bps must be refused as out of range, got {err:?}"
        );
        let message = err.to_string();
        assert!(message.contains("9999"), "{message}");
        assert!(message.contains("deliver nothing"), "{message}");
    }
    assert!(fees.is_empty(), "a refused insert records nothing");
}

#[test]
fn four_percent_is_a_rate_like_any_other() {
    // THE regression this change exists for. 400 bps used to be refused
    // outright — not because it was unsafe, but because no release had
    // ever shipped it. Changing a configured fee must not require
    // rebuilding a binary.
    let mut fees = RouteFees::new();
    fees.insert(Route::RhnToGlc, 400).unwrap();
    assert_eq!(fees.fee_bps(Route::RhnToGlc).unwrap(), 400);

    let breakdown = compute_fee_at_bps(CanonicalAtomic(1_000_000_000), 400).unwrap();
    assert_eq!(breakdown.fee_bps, 400);
    assert_eq!(breakdown.fee.0, 40_000_000);
    assert_eq!(breakdown.net.0, 960_000_000);
}

#[test]
fn every_rate_in_range_is_configurable_for_every_executable_route() {
    // Exhaustive at the ends and sampled in between, because the rule is
    // now a RANGE and a range test should look like one. Nothing here
    // consults a list of rates anyone charged before.
    for route in executable_routes() {
        for bps in [
            MIN_FEE_BPS,
            1,
            7,
            100,
            137,
            300,
            400,
            450,
            600,
            1_234,
            5_000,
            9_998,
            MAX_FEE_BPS,
        ] {
            let mut fees = RouteFees::new();
            fees.insert(route, bps)
                .unwrap_or_else(|e| panic!("{} must accept {bps} bps: {e}", route.as_str()));
            assert_eq!(fees.fee_bps(route).unwrap(), bps);
        }
    }
}

#[test]
fn nothing_in_the_fee_path_consults_a_list_of_previously_charged_rates() {
    // A guard against the allowlist creeping back in under another name:
    // rates that have never been a default anywhere in this repository,
    // each required to configure AND to price.
    for bps in [17u64, 233, 401, 777, 3_141, 8_888] {
        let mut fees = RouteFees::new();
        fees.insert(Route::GlcToRhn, bps).unwrap();
        let breakdown = compute_fee_at_bps(CanonicalAtomic(10_000_000), bps).unwrap();
        assert_eq!(breakdown.fee_bps, bps);
        assert_eq!(breakdown.fee.0, 10_000_000 * bps / 10_000);
        assert_eq!(breakdown.net.0, 10_000_000 - breakdown.fee.0);
    }
}

#[test]
fn a_route_declared_twice_is_refused() {
    let mut fees = RouteFees::new();
    fees.insert(Route::GlcToSol, 300).unwrap();
    assert_eq!(
        fees.insert(Route::GlcToSol, 600).unwrap_err(),
        FeeError::DuplicateFee { route: "GlcToSol" }
    );
    assert_eq!(
        fees.fee_bps(Route::GlcToSol).unwrap(),
        300,
        "the refused second declaration must not have overwritten the first"
    );
}

// ---------------------------------------------------------- fee math --

#[test]
fn the_resolved_rate_drives_the_exact_same_fee_math_as_before() {
    // Per-route lookup changes WHICH rate is applied, never HOW. The
    // arithmetic is still `amount_conversion`'s, floored, checked, and
    // identical to what the global path produced at the same rate.
    let fees = launch_fees();
    let gross = CanonicalAtomic(1_000_000_000); // 10 GLC

    let solana = compute_fee_at_bps(gross, fees.fee_bps(Route::GlcToSol).unwrap()).unwrap();
    assert_eq!(solana.fee_bps, 300);
    assert_eq!(solana.fee.0, 30_000_000);
    assert_eq!(solana.net.0, 970_000_000);
    assert_eq!(solana.gross.0, solana.fee.0 + solana.net.0);

    let robinhood = compute_fee_at_bps(gross, fees.fee_bps(Route::GlcToRhn).unwrap()).unwrap();
    assert_eq!(robinhood.fee_bps, 600);
    assert_eq!(robinhood.fee.0, 60_000_000);
    assert_eq!(robinhood.net.0, 940_000_000);
    assert_eq!(robinhood.gross.0, robinhood.fee.0 + robinhood.net.0);

    // And the Solana leg is byte-for-byte what the old global path gave.
    let legacy = compute_fee_at_bps(gross, BRIDGE_FEE_BPS).unwrap();
    assert_eq!(legacy, solana);
}

// ------------------------------------------------------- future routes --

#[test]
fn a_future_executable_route_gets_its_own_rate_without_touching_the_others() {
    // Stands in for "add another executable Route". The pricing path
    // takes a Route and asks the table; nothing about adding one requires
    // an edit to quote logic, to another route's entry, or to this type.
    //
    // `RhnToGlc` plays the newcomer here — the point is the SHAPE: build
    // the table by iterating `executable_routes()`, give the new one its
    // own rate, and observe that every pre-existing rate is unchanged.
    let established = [
        (Route::GlcToSol, 300u64),
        (Route::SolToGlc, 300),
        (Route::GlcToRhn, 600),
        (Route::RhnToGlc, 600),
        (Route::SolToRhn, 600),
    ];
    let newcomer = (Route::RhnToSol, 100u64);

    let mut fees = RouteFees::new();
    for (route, bps) in established {
        fees.insert(route, bps).unwrap();
    }
    // Before the newcomer is priced, the table is INCOMPLETE and says so
    // rather than inventing a rate for it.
    assert_eq!(
        fees.covers_every_executable_route().unwrap_err(),
        FeeError::MissingFee {
            route: newcomer.0.as_str()
        }
    );

    fees.insert(newcomer.0, newcomer.1).unwrap();
    fees.covers_every_executable_route().unwrap();

    assert_eq!(fees.fee_bps(newcomer.0).unwrap(), newcomer.1);
    for (route, bps) in established {
        assert_eq!(
            fees.fee_bps(route).unwrap(),
            bps,
            "{} must be untouched by a new route's arrival",
            route.as_str()
        );
    }
}

#[test]
fn iteration_is_in_registry_order_not_insertion_order() {
    // Operator-facing listings must be stable across runs and match the
    // order every other route listing uses.
    let mut fees = RouteFees::new();
    fees.insert(Route::RhnToGlc, 600).unwrap();
    fees.insert(Route::GlcToSol, 300).unwrap();
    fees.insert(Route::GlcToRhn, 600).unwrap();
    fees.insert(Route::SolToGlc, 300).unwrap();

    let order: Vec<Route> = fees.iter().map(|(route, _)| route).collect();
    assert_eq!(
        order,
        vec![
            Route::GlcToSol,
            Route::SolToGlc,
            Route::GlcToRhn,
            Route::RhnToGlc
        ]
    );
}
