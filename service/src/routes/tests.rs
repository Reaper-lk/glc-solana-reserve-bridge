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
    let config = RoutesConfig::default().with_robinhood(true, true);
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
    let config = RoutesConfig::default().with_robinhood(true, true);
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

#[test]
fn all_three_gates_open_still_cannot_produce_a_settlement_direction() {
    // The end of the line. Even with every gate subverted — config on,
    // ledger row on, and a fabricated permissive adapter — the route still
    // yields no `Direction`, so no reserve, ledger or signing function can
    // be called with it. This is the property that makes the Phase-1
    // posture structural rather than procedural.
    let ledger = ledger();
    enable_route_in_ledger(&ledger, Route::GlcToRhn);
    let config = RoutesConfig::default().with_robinhood(true, true);
    let gate = RouteGate::new(config, permissive_registry());

    gate.ensure_enabled(&ledger, Route::GlcToRhn)
        .expect("this contrived deployment deliberately opens all three gates");
    assert_eq!(
        Route::GlcToRhn.as_direction(),
        None,
        "a Robinhood route must never yield a settlement Direction"
    );
    assert_eq!(Route::RhnToGlc.as_direction(), None);
}

/// Creates the Phase-2 `bridge_routes` table and switches `route` on, to
/// exercise the ledger gate's "table present, row present" branch. Phase 1
/// never creates this table itself (no schema-version bump — see
/// `Ledger::route_enabled`); this is a test fixture standing in for the
/// future migration.
fn enable_route_in_ledger(ledger: &Ledger, route: Route) {
    ledger
        .connection()
        .execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS bridge_routes (
                 route_id TEXT PRIMARY KEY,
                 enabled  INTEGER NOT NULL DEFAULT 0
             );
             INSERT OR REPLACE INTO bridge_routes (route_id, enabled) VALUES ('{}', 1);",
            route.as_str()
        ))
        .unwrap();
}

// ------------------------------------------------------ ledger gate rules --

#[test]
fn missing_bridge_routes_table_falls_back_to_per_route_defaults() {
    let ledger = ledger();
    // Legacy: absent table must not close production traffic.
    assert!(ledger.route_enabled("GlcToSol", true).unwrap());
    assert!(ledger.route_enabled("SolToGlc", true).unwrap());
    // New: absent table must mean disabled.
    assert!(!ledger.route_enabled("GlcToRhn", false).unwrap());
    assert!(!ledger.route_enabled("RhnToGlc", false).unwrap());
}

#[test]
fn present_table_with_no_row_falls_back_to_the_default() {
    let ledger = ledger();
    ledger
        .connection()
        .execute_batch(
            "CREATE TABLE bridge_routes (route_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL);",
        )
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
        .execute_batch(
            "CREATE TABLE bridge_routes (route_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL);
             INSERT INTO bridge_routes VALUES ('GlcToSol', 0);",
        )
        .unwrap();
    assert!(!ledger.route_enabled("GlcToSol", true).unwrap());
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
fn route_chain_endpoints_are_correct_and_never_solana_to_robinhood() {
    assert_eq!(Route::GlcToRhn.source_chain(), Chain::Goldcoin);
    assert_eq!(Route::GlcToRhn.destination_chain(), Chain::Robinhood);
    assert_eq!(Route::RhnToGlc.source_chain(), Chain::Robinhood);
    assert_eq!(Route::RhnToGlc.destination_chain(), Chain::Goldcoin);
    // Direct Solana<->Robinhood bridging is out of scope: no route may
    // name both as its two endpoints.
    for route in Route::ALL {
        let pair = (route.source_chain(), route.destination_chain());
        assert!(
            pair != (Chain::Solana, Chain::Robinhood) && pair != (Chain::Robinhood, Chain::Solana),
            "{route:?} bridges Solana and Robinhood directly, which is out of scope"
        );
    }
}

#[test]
fn every_route_is_covered_by_the_all_constant() {
    // Guards the listings in `GET /chains` and the daemon's startup log
    // against silently omitting a route when a variant is added.
    assert_eq!(Route::ALL.len(), 4);
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
