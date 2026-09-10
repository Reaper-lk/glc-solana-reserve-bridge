//! Route-gate tests.
//!
//! The theme throughout: prove the Robinhood routes are closed *by every
//! individual gate on its own*, not merely closed when all three agree.
//! A gate that only works in concert with the others is one refactor away
//! from being no gate at all.

use super::*;
use crate::chains::{Capability, ChainAdapter, ChainRegistry};
use crate::ledger::Ledger;

fn ledger() -> Ledger {
    Ledger::open_in_memory().unwrap()
}

/// An adapter that claims everything works — used to isolate the config and
/// ledger gates by removing the adapter gate's contribution.
struct AlwaysOperational(Chain);
impl ChainAdapter for AlwaysOperational {
    fn chain(&self) -> Chain {
        self.0
    }
    fn capability(&self, _route: Route) -> Capability {
        Capability::Operational
    }
}

fn permissive_registry() -> ChainRegistry {
    ChainRegistry::new()
        .with(Box::new(AlwaysOperational(Chain::Goldcoin)))
        .with(Box::new(AlwaysOperational(Chain::Solana)))
        .with(Box::new(AlwaysOperational(Chain::Robinhood)))
}

// ------------------------------------------------------ default posture --

#[test]
fn robinhood_routes_are_disabled_on_a_default_deployment() {
    let gate = RouteGate::legacy_only();
    let ledger = ledger();
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        let err = gate.ensure_enabled(&ledger, route).unwrap_err();
        assert!(
            matches!(err, RouteGateError::Disabled { .. }),
            "{route:?} must be refused on a default deployment, got {err:?}"
        );
    }
}

#[test]
fn legacy_routes_are_enabled_on_a_default_deployment() {
    // The Solana regression guard, at the gate level: an unmodified config
    // and an unmigrated ledger must admit exactly what production admits
    // today.
    let gate = RouteGate::legacy_only();
    let ledger = ledger();
    for route in [Route::GlcToSol, Route::SolToGlc] {
        gate.ensure_enabled(&ledger, route)
            .unwrap_or_else(|e| panic!("{route:?} must stay enabled, got {e:?}"));
    }
}

// ------------------------------------------- each gate closes on its own --

#[test]
fn config_gate_alone_closes_a_robinhood_route() {
    // Adapter permissive, ledger silent (default true would still not
    // apply — Robinhood's default is false, so force the ledger out of the
    // picture by asserting on the reported cause).
    let gate = RouteGate::new(RoutesConfig::default(), permissive_registry());
    let err = gate.ensure_enabled(&ledger(), Route::GlcToRhn).unwrap_err();
    match err {
        RouteGateError::Disabled { disabled_by, .. } => {
            assert_eq!(disabled_by, DisabledBy::Config)
        }
        other => panic!("expected a config refusal, got {other:?}"),
    }
}

#[test]
fn ledger_gate_alone_closes_a_robinhood_route_even_with_config_and_adapter_open() {
    // Config says yes, adapter says yes. The ledger's default for a
    // Robinhood route is `false`, so the route must still be refused — and
    // the reported cause must be the ledger, proving this gate is doing
    // real work rather than riding on the other two.
    let config = RoutesConfig::default().with_robinhood(true, true, true, true);
    let gate = RouteGate::new(config, permissive_registry());
    let err = gate.ensure_enabled(&ledger(), Route::GlcToRhn).unwrap_err();
    match err {
        RouteGateError::Disabled { disabled_by, .. } => {
            assert_eq!(disabled_by, DisabledBy::Ledger)
        }
        other => panic!("expected a ledger refusal, got {other:?}"),
    }
}

#[test]
fn adapter_gate_alone_closes_a_robinhood_route_when_config_and_ledger_are_open() {
    // Config on, and the ledger forced on by physically creating the
    // Phase-2 `bridge_routes` table and enabling the route in it. Only the
    // real Phase-1 adapter remains, and it must refuse.
    let ledger = ledger();
    enable_route_in_ledger(&ledger, Route::GlcToRhn);
    let config = RoutesConfig::default().with_robinhood(true, true, true, true);
    let gate = RouteGate::new(config, ChainRegistry::phase1());
    let err = gate.ensure_enabled(&ledger, Route::GlcToRhn).unwrap_err();
    match err {
        RouteGateError::Disabled { disabled_by, .. } => assert!(
            matches!(disabled_by, DisabledBy::Adapter { .. }),
            "expected an adapter refusal, got {disabled_by:?}"
        ),
        other => panic!("expected an adapter refusal, got {other:?}"),
    }
}

/// The end of the line for the two routes that have NO settlement
/// machinery.
///
/// Phase F built the machinery for `GlcToRhn`/`RhnToGlc`, so those two
/// now have a `Direction` and this property no longer applies to them —
/// what guards them is the route gate, exercised throughout this file.
///
/// For `SolToRhn`/`RhnToSol` the original, stronger guarantee is intact
/// and is tested here in its strongest form: even with every gate
/// deliberately subverted — config on, ledger row on, and a fabricated
/// permissive adapter — the route still yields no `Direction`, so no
/// reserve, ledger or signing function can be called with it at all.
#[test]
fn all_three_gates_open_still_cannot_produce_a_settlement_direction() {
    let ledger = ledger();
    let config = RoutesConfig::default().with_robinhood(true, true, true, true);
    let gate = RouteGate::new(config, permissive_registry());

    for route in [Route::SolToRhn, Route::RhnToSol] {
        enable_route_in_ledger(&ledger, route);
        gate.ensure_enabled(&ledger, route)
            .expect("this contrived deployment deliberately opens all three gates");
        assert_eq!(
            route.as_direction(),
            None,
            "{} must never yield a settlement Direction",
            route.as_str()
        );
    }
}

/// Forces `route`'s ledger row on by RAW SQL, to exercise the ledger
/// gate's "row present, enabled = 1" branch for any route at all —
/// including the two `Ledger::set_route_enabled` refuses. The supported
/// path is exercised separately, by
/// `set_route_enabled_opens_only_the_ledger_gate`.
///
/// The table itself is no longer created here: schema v24 creates and
/// seeds it, so every `Ledger` already has a row per route and this only
/// flips one.
fn enable_route_in_ledger(ledger: &Ledger, route: Route) {
    let n = ledger
        .connection()
        .execute(
            "UPDATE bridge_routes SET enabled = 1 WHERE route_id = ?1",
            [route.as_str()],
        )
        .unwrap();
    assert_eq!(n, 1, "v24 must have seeded a row for {}", route.as_str());
}

// ------------------------------------------------------ ledger gate rules --

#[test]
fn missing_bridge_routes_table_falls_back_to_per_route_defaults() {
    // Schema v24 creates the table, so it is dropped here on purpose: the
    // fallback is not dead code, it is the fail-closed floor underneath
    // the whole gate, and it must keep working for a ledger that predates
    // the migration or has lost the table.
    let ledger = ledger();
    ledger
        .connection()
        .execute_batch("DROP TABLE bridge_routes;")
        .unwrap();
    // Legacy: absent table must not close production traffic.
    assert!(ledger.route_enabled("GlcToSol", true).unwrap());
    assert!(ledger.route_enabled("SolToGlc", true).unwrap());
    // New: absent table must mean disabled.
    assert!(!ledger.route_enabled("GlcToRhn", false).unwrap());
    assert!(!ledger.route_enabled("RhnToGlc", false).unwrap());
}

#[test]
fn present_table_with_no_row_falls_back_to_the_default() {
    // v24 seeds a row for every route, so the rows are deleted here to
    // reach the middle branch deliberately — a route the table knows
    // nothing about must resolve to its default, not to "enabled".
    let ledger = ledger();
    ledger
        .connection()
        .execute_batch("DELETE FROM bridge_routes;")
        .unwrap();
    assert!(ledger.route_enabled("GlcToSol", true).unwrap());
    assert!(!ledger.route_enabled("GlcToRhn", false).unwrap());
}

#[test]
fn an_explicit_zero_row_disables_even_a_legacy_route() {
    // The ledger gate must be able to close a route an operator wants
    // closed, not only confirm defaults. This is the mechanism the Phase-2
    // migration and the admin route controls will use.
    let ledger = ledger();
    ledger
        .connection()
        .execute_batch("UPDATE bridge_routes SET enabled = 0 WHERE route_id = 'GlcToSol';")
        .unwrap();
    assert!(!ledger.route_enabled("GlcToSol", true).unwrap());
}

// ------------------------------------ the v24 seed and the operator write --

#[test]
fn the_v24_seed_is_exactly_every_routes_default_enabled() {
    // The migration seeds literal values; `Route::default_enabled` is
    // Rust. This is what keeps them from drifting — and it asks with the
    // WRONG default deliberately, so a row that failed to seed would
    // answer with the fallback and fail here instead of passing by
    // accident.
    let ledger = ledger();
    for route in Route::ALL {
        let opposite = !route.default_enabled();
        assert_eq!(
            ledger.route_enabled(route.as_str(), opposite).unwrap(),
            route.default_enabled(),
            "{}: the seeded row, not the fallback, must answer — and it must answer with \
             default_enabled",
            route.as_str()
        );
    }
}

#[test]
fn the_migration_alone_opens_nothing() {
    // The whole point of the seed being a no-op: running the migration is
    // not a launch. A migrated ledger with an otherwise permissive
    // deployment still refuses both Robinhood routes, at the ledger gate.
    let config = RoutesConfig::default().with_robinhood(true, true, true, true);
    let gate = RouteGate::new(config, permissive_registry());
    let ledger = ledger();
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        match gate.ensure_enabled(&ledger, route).unwrap_err() {
            RouteGateError::Disabled { disabled_by, .. } => assert_eq!(
                disabled_by,
                DisabledBy::Ledger,
                "{} must still be closed BY THE LEDGER after the migration",
                route.as_str()
            ),
            other => panic!("expected a ledger refusal, got {other:?}"),
        }
    }
}

#[test]
fn set_route_enabled_opens_the_ledger_gate_and_only_the_ledger_gate() {
    let mut ledger = ledger();
    ledger
        .set_route_enabled(Route::GlcToRhn, true, None)
        .unwrap();

    // The ledger gate is now open...
    assert!(ledger.route_enabled("GlcToRhn", false).unwrap());

    // ...and the route is still shut, by each of the other two on its
    // own. An operator write is necessary, never sufficient.
    let adapter_shut = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        ChainRegistry::phase1(),
    );
    match adapter_shut
        .ensure_enabled(&ledger, Route::GlcToRhn)
        .unwrap_err()
    {
        RouteGateError::Disabled { disabled_by, .. } => assert!(
            matches!(disabled_by, DisabledBy::Adapter { .. }),
            "expected an adapter refusal, got {disabled_by:?}"
        ),
        other => panic!("expected an adapter refusal, got {other:?}"),
    }
    let config_shut = RouteGate::new(RoutesConfig::default(), permissive_registry());
    match config_shut
        .ensure_enabled(&ledger, Route::GlcToRhn)
        .unwrap_err()
    {
        RouteGateError::Disabled { disabled_by, .. } => {
            assert_eq!(disabled_by, DisabledBy::Config)
        }
        other => panic!("expected a config refusal, got {other:?}"),
    }

    // With all three open — which is what a completed launch looks like —
    // the route opens. This is the supported path GET /chains reports on.
    let all_open = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        permissive_registry(),
    );
    all_open.ensure_enabled(&ledger, Route::GlcToRhn).unwrap();
}

#[test]
fn set_route_enabled_touches_exactly_the_route_it_was_given() {
    // Opening GlcToRhn must not open its twin, and must not disturb
    // either legacy route's state.
    let mut ledger = ledger();
    ledger
        .set_route_enabled(Route::GlcToRhn, true, None)
        .unwrap();
    assert!(!ledger.route_enabled("RhnToGlc", false).unwrap());
    assert!(ledger.route_enabled("GlcToSol", false).unwrap());
    assert!(ledger.route_enabled("SolToGlc", false).unwrap());
    assert!(!ledger.route_enabled("SolToRhn", false).unwrap());
    assert!(!ledger.route_enabled("RhnToSol", false).unwrap());
}

#[test]
fn set_route_enabled_can_close_a_route_it_opened() {
    // Reversibility is part of the control: an operator who opens a route
    // must be able to shut it again without touching the database by hand.
    let mut ledger = ledger();
    ledger
        .set_route_enabled(Route::RhnToGlc, true, None)
        .unwrap();
    assert!(ledger.route_enabled("RhnToGlc", false).unwrap());
    ledger
        .set_route_enabled(Route::RhnToGlc, false, Some("incident 7"))
        .unwrap();
    assert!(!ledger.route_enabled("RhnToGlc", false).unwrap());
}

#[test]
fn set_route_enabled_refuses_the_legacy_routes() {
    // Their control is the pause/admission machinery. A second, divergent
    // switch that no reserve invariant or liquidity check knows about is
    // exactly what must not exist — so this refuses, and the routes stay
    // exactly as they were.
    let mut ledger = ledger();
    for route in [Route::GlcToSol, Route::SolToGlc] {
        let err = ledger
            .set_route_enabled(route, false, Some("nope"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                crate::ledger::LedgerError::RouteNotOperatorSettable { .. }
            ),
            "{}: expected a refusal, got {err:?}",
            route.as_str()
        );
        assert!(
            ledger.route_enabled(route.as_str(), false).unwrap(),
            "{} must be untouched by the refused write",
            route.as_str()
        );
    }
}

#[test]
fn set_route_enabled_refuses_the_non_executable_routes() {
    // No `Direction` exists for either, so an `enabled = 1` row would be a
    // claim nothing else in the service could honour.
    let mut ledger = ledger();
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert_eq!(route.as_direction(), None);
        let err = ledger.set_route_enabled(route, true, None).unwrap_err();
        assert!(
            matches!(
                err,
                crate::ledger::LedgerError::RouteNotOperatorSettable { .. }
            ),
            "{}: expected a refusal, got {err:?}",
            route.as_str()
        );
        assert!(
            !ledger.route_enabled(route.as_str(), true).unwrap(),
            "{} must stay disabled in the ledger",
            route.as_str()
        );
    }
}

#[test]
fn only_the_two_executable_robinhood_routes_are_operator_settable() {
    for route in Route::ALL {
        assert_eq!(
            route.is_operator_settable(),
            matches!(route, Route::GlcToRhn | Route::RhnToGlc),
            "{route:?}: operator-settable must be exactly the two executable Robinhood routes"
        );
    }
}

#[test]
fn set_route_enabled_refuses_a_ledger_that_has_not_run_the_migration() {
    // Never an INSERT: writing route state into a database whose schema
    // this binary has not established is how a ledger ends up with rows
    // nothing else understands.
    let mut ledger = ledger();
    ledger
        .connection()
        .execute_batch("DELETE FROM bridge_routes;")
        .unwrap();
    let err = ledger
        .set_route_enabled(Route::GlcToRhn, true, None)
        .unwrap_err();
    assert!(
        matches!(err, crate::ledger::LedgerError::RouteStateNotInitialized(_)),
        "expected a not-migrated refusal, got {err:?}"
    );
}

// ------------------------------------------------------------- identity --

#[test]
fn route_names_are_the_agreed_spellings() {
    assert_eq!(Route::GlcToSol.as_str(), "GlcToSol");
    assert_eq!(Route::SolToGlc.as_str(), "SolToGlc");
    assert_eq!(Route::GlcToRhn.as_str(), "GlcToRhn");
    assert_eq!(Route::RhnToGlc.as_str(), "RhnToGlc");
    // The rejected alternative spellings must not parse, so a client or a
    // config file using them fails loudly instead of being silently
    // reinterpreted.
    assert!("L1ToRobinhood".parse::<Route>().is_err());
    assert!("RobinhoodToL1".parse::<Route>().is_err());
}

#[test]
fn route_round_trips_through_its_string_form() {
    for route in Route::ALL {
        assert_eq!(route.as_str().parse::<Route>().unwrap(), route);
    }
}

#[test]
fn legacy_direction_widens_to_the_matching_route_and_back() {
    for direction in [Direction::GlcToSol, Direction::SolToGlc] {
        let route = Route::from(direction);
        assert_eq!(route.as_direction(), Some(direction));
    }
}

#[test]
fn route_chain_endpoints_are_correct() {
    assert_eq!(Route::GlcToRhn.source_chain(), Chain::Goldcoin);
    assert_eq!(Route::GlcToRhn.destination_chain(), Chain::Robinhood);
    assert_eq!(Route::RhnToGlc.source_chain(), Chain::Robinhood);
    assert_eq!(Route::RhnToGlc.destination_chain(), Chain::Goldcoin);

    // This test previously asserted that NO route names Solana and
    // Robinhood as its two endpoints, on the grounds that direct
    // Solana<->Robinhood bridging was out of scope. That assumption is now
    // obsolete: the custody contract models `SolToRhn` and `RhnToSol`
    // structurally, so both pairs exist as named routes.
    //
    // The replacement invariant is stronger, not weaker. It is no longer
    // "the pair cannot be named" — which stopped being true — but "the
    // pair is named AND nothing can serve it": no settlement direction, no
    // adapter capability, and disabled by default. A route that cannot be
    // named cannot be audited; a route that is named and provably closed
    // can.
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert_eq!(route.as_direction(), None, "{route:?} must not settle");
        assert!(!route.default_enabled(), "{route:?} must default disabled");
    }
}

#[test]
fn every_route_is_covered_by_the_all_constant() {
    // Guards the listings in `GET /chains` and the daemon's startup log
    // against silently omitting a route when a variant is added. Six since
    // the Solana<->Robinhood routes were added structurally.
    assert_eq!(Route::ALL.len(), 6);
    for route in Route::ALL {
        assert!(Route::ALL.contains(&route));
    }
    assert_eq!(Chain::ALL.len(), 3);
}

#[test]
fn defaults_are_enabled_exactly_for_the_legacy_routes() {
    for route in Route::ALL {
        assert_eq!(
            route.default_enabled(),
            route.is_legacy(),
            "{route:?}: default_enabled must track is_legacy exactly"
        );
    }
}

#[test]
fn disabled_reason_is_cause_agnostic_and_reveals_no_gate() {
    let gate = RouteGate::legacy_only();
    let ledger = ledger();
    let reason = gate.disabled_reason(&ledger, Route::GlcToRhn).unwrap();
    assert_eq!(reason, RouteGateError::UNAVAILABLE_MESSAGE);
    for leak in ["config", "ledger", "adapter", "bridge_routes"] {
        assert!(
            !reason.contains(leak),
            "the public reason must not name the {leak} gate"
        );
    }
    assert_eq!(gate.disabled_reason(&ledger, Route::GlcToSol), None);
}

// ===================================================================== //
// Six-route model                                                       //
// ===================================================================== //
//
// The route set grew from four to six when the custody contract added
// structural support for Solana↔Robinhood. These pin the whole shape at
// once, because the properties that matter are about the SET, not about any
// one variant: exactly six routes exist, exactly two settle, and exactly
// four are Robinhood-side and disabled.

#[test]
fn there_are_exactly_six_routes_and_all_are_listed() {
    assert_eq!(Route::ALL.len(), 6);
    let mut names: Vec<&str> = Route::ALL.iter().map(|r| r.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["GlcToRhn", "GlcToSol", "RhnToGlc", "RhnToSol", "SolToGlc", "SolToRhn"]
    );

    // `ALL` must not silently drift out of sync with the enum: every entry
    // round-trips through its own name.
    for route in Route::ALL {
        assert_eq!(
            route.as_str().parse::<Route>().unwrap(),
            route,
            "{} does not round-trip",
            route.as_str()
        );
    }
}

/// The settlement firewall: exactly the two legacy routes have a
/// `Direction`, and all four Robinhood-side routes have none. `None` here
/// is a type-level guarantee, not a TODO.
#[test]
fn exactly_the_four_executable_routes_have_a_settlement_direction() {
    assert_eq!(Route::GlcToSol.as_direction(), Some(Direction::GlcToSol));
    assert_eq!(Route::SolToGlc.as_direction(), Some(Direction::SolToGlc));
    // Phase F. Having a `Direction` says the machinery EXISTS; whether it
    // may run is `RouteGate`'s decision, plus the contract's own
    // `routeEnabled`.
    assert_eq!(Route::GlcToRhn.as_direction(), Some(Direction::GlcToRhn));
    assert_eq!(Route::RhnToGlc.as_direction(), Some(Direction::RhnToGlc));

    // The two that remain unreachable BY CONSTRUCTION. `None` here is a
    // type-level guarantee, not a TODO: no `Direction` value exists for
    // them, so none of the reserve, ledger or signing functions that
    // require one can be called with them.
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert_eq!(
            route.as_direction(),
            None,
            "{} must have no settlement direction",
            route.as_str()
        );
    }

    // Stated as a set property too, so adding a seventh route that
    // settles cannot pass by only updating the list above.
    assert_eq!(
        Route::ALL
            .iter()
            .filter(|r| r.as_direction().is_some())
            .count(),
        4
    );
    // And the database says the same thing independently: its direction
    // CHECK admits exactly these four spellings (schema v23).
    assert_eq!(Direction::ALL.len(), 4);
}

/// Fail-closed, restated over the whole set: the two legacy routes keep
/// their existing default, every Robinhood route defaults off.
#[test]
fn every_robinhood_route_defaults_disabled_and_legacy_defaults_are_unchanged() {
    assert!(Route::GlcToSol.default_enabled());
    assert!(Route::SolToGlc.default_enabled());

    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        assert!(
            !route.default_enabled(),
            "{} must default to disabled",
            route.as_str()
        );
        assert!(!route.is_legacy(), "{} is not legacy", route.as_str());
    }

    // A default `RoutesConfig` — what an unmodified production config file
    // resolves to — agrees, route for route.
    let cfg = RoutesConfig::default();
    for route in Route::ALL {
        assert_eq!(
            cfg.enabled(route),
            route.default_enabled(),
            "{} config default disagrees with route default",
            route.as_str()
        );
    }
}

/// The contract discriminator mapping is a wire contract with deployed
/// bytecode. These exact bytes appear in `GlcRobinhoodBridge`'s `ROUTE_*`
/// constants; changing one here without changing the contract would
/// authorize the wrong route.
#[test]
fn contract_route_ids_match_the_custody_contract() {
    assert_eq!(Route::GlcToRhn.contract_route_id(), Some(0x01));
    assert_eq!(Route::RhnToGlc.contract_route_id(), Some(0x02));
    assert_eq!(Route::SolToRhn.contract_route_id(), Some(0x03));
    assert_eq!(Route::RhnToSol.contract_route_id(), Some(0x04));
}

/// The two Solana-only routes are not modelled by the custody contract at
/// all, so they have no discriminator. Returning `Some(0)` or a default
/// would hand contract-facing code a value the contract treats as
/// permanently invalid.
#[test]
fn solana_only_routes_have_no_contract_route_id() {
    assert_eq!(Route::GlcToSol.contract_route_id(), None);
    assert_eq!(Route::SolToGlc.contract_route_id(), None);
}

/// The four discriminators are distinct, non-zero, and cover exactly the
/// Robinhood routes.
#[test]
fn contract_route_ids_are_distinct_and_never_zero() {
    let ids: Vec<u8> = Route::ALL
        .iter()
        .filter_map(|r| r.contract_route_id())
        .collect();
    assert_eq!(ids.len(), 4, "exactly four routes are contract-modelled");
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "discriminators must be distinct");
    assert!(
        ids.iter().all(|id| *id != 0),
        "0x00 is permanently invalid in the contract"
    );

    // A route has a contract discriminator exactly when the CUSTODY
    // CONTRACT models it — i.e. exactly the routes with Robinhood on one
    // leg. That is a different axis from `as_direction`, which says
    // whether THIS SERVICE can settle it, and the two deliberately
    // disagree for `GlcToRhn`/`RhnToGlc`: the contract models them AND
    // this service settles them.
    for route in Route::ALL {
        let touches_robinhood = route.source_chain() == crate::routes::Chain::Robinhood
            || route.destination_chain() == crate::routes::Chain::Robinhood;
        assert_eq!(
            route.contract_route_id().is_some(),
            touches_robinhood,
            "{} disagrees between the two axes",
            route.as_str()
        );
    }
}

/// Chain legs for the two new routes.
#[test]
fn solana_robinhood_routes_have_the_expected_legs() {
    assert_eq!(Route::SolToRhn.source_chain(), Chain::Solana);
    assert_eq!(Route::SolToRhn.destination_chain(), Chain::Robinhood);
    assert_eq!(Route::RhnToSol.source_chain(), Chain::Robinhood);
    assert_eq!(Route::RhnToSol.destination_chain(), Chain::Solana);

    // No route is a self-loop.
    for route in Route::ALL {
        assert_ne!(
            route.source_chain(),
            route.destination_chain(),
            "{} is a self-loop",
            route.as_str()
        );
    }
}

// ------------------------------------------- the read side of the gate --
//
// `Ledger::route_ledger_rows` is what `glc-admin robinhood-routes` reports
// and what `scripts/bridge-admin.sh` shows an operator before they change
// anything. Its whole reason to exist is that it must NOT resolve the way
// `route_enabled` does, so that is what these pin.

#[test]
fn route_ledger_rows_reports_every_seeded_route_without_resolving_anything() {
    let ledger = ledger();
    let state = ledger
        .route_ledger_rows()
        .unwrap()
        .expect("a migrated ledger has the v24 table");

    assert_eq!(
        state.rows.len(),
        Route::ALL.len(),
        "v24 seeds one row per route; the read must surface all of them"
    );
    assert!(
        state.unknown_route_ids.is_empty(),
        "a freshly migrated ledger holds no route_id this build cannot parse"
    );
    // Registry order, so an operator reads this beside Route::ALL
    // everywhere else rather than in lexicographic order.
    let seen: Vec<Route> = state.rows.iter().map(|r| r.route).collect();
    assert_eq!(seen, Route::ALL.to_vec());

    for route in Route::ALL {
        let row = state.row(route).expect("seeded");
        assert_eq!(
            row.enabled,
            route.default_enabled(),
            "{} must be seeded at its compiled-in default",
            route.as_str()
        );
        assert!(row.disabled_reason.is_none());
        assert!(row.updated_at > 0, "the seed writes a real timestamp");
    }
}

#[test]
fn route_ledger_rows_carries_updated_at_and_the_disabled_reason_through() {
    // The two facts an operator needs that `route_enabled` throws away:
    // WHEN the flag was last written, and WHY it is off.
    let mut ledger = ledger();
    let before = ledger
        .route_ledger_rows()
        .unwrap()
        .unwrap()
        .row(Route::GlcToRhn)
        .unwrap()
        .updated_at;

    ledger
        .set_route_enabled(Route::GlcToRhn, true, None)
        .unwrap();
    let opened = ledger.route_ledger_rows().unwrap().unwrap();
    let row = opened.row(Route::GlcToRhn).unwrap();
    assert!(row.enabled);
    assert!(
        row.disabled_reason.is_none(),
        "a stale reason beside an OPEN route reads as an explanation of a state that is \
         no longer true"
    );
    assert!(row.updated_at >= before);

    ledger
        .set_route_enabled(Route::GlcToRhn, false, Some("incident OPS-1300"))
        .unwrap();
    let closed = ledger.route_ledger_rows().unwrap().unwrap();
    let row = closed.row(Route::GlcToRhn).unwrap();
    assert!(!row.enabled);
    assert_eq!(row.disabled_reason.as_deref(), Some("incident OPS-1300"));

    // And nothing else moved.
    for other in [Route::RhnToGlc, Route::SolToRhn, Route::RhnToSol] {
        assert!(!closed.row(other).unwrap().enabled);
    }
    for legacy in [Route::GlcToSol, Route::SolToGlc] {
        assert!(closed.row(legacy).unwrap().enabled);
    }
}

#[test]
fn route_ledger_rows_reports_an_absent_table_as_absent_not_as_defaults() {
    // The whole point of this read. `route_enabled` MUST resolve a missing
    // table to `default_enabled` because the admission gate has to return
    // a verdict; a display that did the same would tell an operator a
    // route is "disabled" when the truth is "this ledger has never run
    // v24", and those have completely different remedies.
    let ledger = ledger();
    ledger
        .conn_for_tests()
        .execute_batch("DROP TABLE bridge_routes;")
        .unwrap();

    assert_eq!(
        ledger.route_ledger_rows().unwrap(),
        None,
        "an absent table must be reported as absent"
    );
    // Meanwhile the admission gate still resolves, unchanged and fail-closed.
    assert!(ledger.route_enabled("GlcToSol", true).unwrap());
    assert!(!ledger.route_enabled("GlcToRhn", false).unwrap());
}

#[test]
fn route_ledger_rows_surfaces_a_route_id_this_build_does_not_model() {
    // A hand-written row, or a downgrade. Silently dropping it would hide
    // exactly the kind of database an operator needs to be told about.
    let ledger = ledger();
    ledger
        .conn_for_tests()
        .execute_batch(
            "INSERT INTO bridge_routes
                 (route_id, source_chain, destination_chain, enabled, disabled_reason, updated_at)
             VALUES ('GlcToMoon', 'goldcoin', 'moon', 1, NULL, 1757462400);",
        )
        .unwrap();

    let state = ledger.route_ledger_rows().unwrap().unwrap();
    assert_eq!(state.unknown_route_ids, vec!["GlcToMoon".to_string()]);
    assert_eq!(
        state.rows.len(),
        Route::ALL.len(),
        "the modelled routes are still all reported"
    );
    assert!(state.row(Route::GlcToRhn).is_some());
}

#[test]
fn route_ledger_rows_never_reports_the_non_executable_routes_as_enabled() {
    // Belt and braces against a database someone edited by hand: even if
    // SolToRhn/RhnToSol carried enabled = 1, nothing may present them as
    // usable — `set_route_enabled` refuses them, and the adapter reports
    // them Unavailable whatever the row says.
    let mut ledger = ledger();
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert!(
            ledger.set_route_enabled(route, true, None).is_err(),
            "{} must never be settable through the supported API",
            route.as_str()
        );
        assert!(
            !ledger
                .route_ledger_rows()
                .unwrap()
                .unwrap()
                .row(route)
                .unwrap()
                .enabled,
            "{} must still read as disabled after a refused write",
            route.as_str()
        );
    }
}
