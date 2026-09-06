//! Chain-registry and adapter-capability tests.

use super::*;

#[test]
fn phase1_registry_knows_all_three_chains() {
    let registry = ChainRegistry::phase1();
    for chain in Chain::ALL {
        assert!(
            registry.contains(chain),
            "the Phase-1 registry must know {chain:?} by name, even when it can serve nothing"
        );
    }
}

#[test]
fn legacy_chains_are_operational_only_for_legacy_routes() {
    let registry = ChainRegistry::phase1();
    for chain in [Chain::Goldcoin, Chain::Solana] {
        for route in [Route::GlcToSol, Route::SolToGlc] {
            assert!(
                registry.capability(chain, route).is_operational(),
                "{chain:?} must stay operational for {route:?}"
            );
        }
        for route in [Route::GlcToRhn, Route::RhnToGlc] {
            assert!(
                !registry.capability(chain, route).is_operational(),
                "{chain:?} must not claim it can serve {route:?}"
            );
        }
    }
}

#[test]
fn robinhood_chain_is_operational_for_nothing() {
    let registry = ChainRegistry::phase1();
    for route in Route::ALL {
        assert!(
            !registry
                .capability(Chain::Robinhood, route)
                .is_operational(),
            "the Robinhood adapter must refuse {route:?}"
        );
    }
}

#[test]
fn an_unregistered_chain_fails_closed_rather_than_defaulting_open() {
    // A registry built wrong must close routes, not open them. This is the
    // failure mode that a `HashMap::get(...).unwrap_or(Operational)` would
    // have introduced silently.
    let empty = ChainRegistry::new();
    for chain in Chain::ALL {
        for route in Route::ALL {
            assert!(
                !empty.capability(chain, route).is_operational(),
                "an unregistered {chain:?} must not be operational for {route:?}"
            );
        }
    }
}

#[test]
fn both_legs_of_a_route_are_consulted() {
    // GlcToRhn's source leg (Goldcoin) and destination leg (Robinhood) must
    // BOTH refuse, so removing either check from `RouteGate` would still
    // leave the route closed by the other. Defence in depth within the
    // adapter gate itself.
    let registry = ChainRegistry::phase1();
    assert!(!registry
        .capability(Chain::Goldcoin, Route::GlcToRhn)
        .is_operational());
    assert!(!registry
        .capability(Chain::Robinhood, Route::GlcToRhn)
        .is_operational());
}

#[test]
fn capability_reports_a_reason_when_unavailable() {
    match ChainRegistry::phase1().capability(Chain::Robinhood, Route::GlcToRhn) {
        Capability::Unavailable { reason } => assert!(
            !reason.is_empty(),
            "an unavailable capability must explain itself to an operator"
        ),
        Capability::Operational => panic!("Robinhood must not be operational"),
    }
}
