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

/// The legacy routes are served by both legacy chains, and the Robinhood
/// routes are not OPEN — but the reason has moved, and this test now says
/// where it lives.
///
/// Phase G made `GoldcoinAdapter` operational for `RhnToGlc`, because the
/// existing vault payout machinery genuinely serves that leg (see that
/// adapter's docs). The blanket "no legacy chain claims any Robinhood
/// route" assertion this test used to make was therefore both false and,
/// more importantly, protecting the wrong thing: what keeps a Robinhood
/// route closed is the ROBINHOOD leg refusing, plus the config and ledger
/// gates in front of it — never an unrelated chain's adapter declining a
/// leg it can in fact serve.
#[test]
fn legacy_chains_serve_the_legacy_routes_and_only_their_own_robinhood_legs() {
    let registry = ChainRegistry::phase1();
    for chain in [Chain::Goldcoin, Chain::Solana] {
        for route in [Route::GlcToSol, Route::SolToGlc] {
            assert!(
                registry.capability(chain, route).is_operational(),
                "{chain:?} must stay operational for {route:?}"
            );
        }
    }

    // Solana is a leg of NEITHER Goldcoin<->Robinhood route and claims
    // neither.
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        assert!(!registry.capability(Chain::Solana, route).is_operational());
    }

    // Goldcoin serves BOTH of its Robinhood legs: the payout machinery
    // for `RhnToGlc`, and the (now route-aware) deposit pipeline for
    // `GlcToRhn`.
    for route in [Route::RhnToGlc, Route::GlcToRhn] {
        assert!(registry.capability(Chain::Goldcoin, route).is_operational());
    }

    // And BOTH routes remain closed regardless, because the Robinhood leg
    // of each is unavailable in this registry. That is the guarantee this
    // test exists for.
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        assert!(
            !registry
                .capability(Chain::Robinhood, route)
                .is_operational(),
            "{route:?} must stay closed at its Robinhood leg"
        );
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
    // In the phase-1 registry every Robinhood route is a ONE-sided
    // closure: the Solana (or Goldcoin) leg serves it, the unverified
    // Robinhood leg alone holds it shut. `RouteGate::ensure_enabled`
    // consults both legs, so the closed one is what decides.
    let registry = ChainRegistry::phase1();
    assert!(registry
        .capability(Chain::Solana, Route::SolToRhn)
        .is_operational());
    assert!(!registry
        .capability(Chain::Robinhood, Route::SolToRhn)
        .is_operational());

    // GlcToRhn: source leg capable, destination leg refused — closed by
    // the Robinhood adapter on its own.
    assert!(registry
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

// =====================================================================
// The Goldcoin leg's capability matrix (Phase G, blocker H)
// =====================================================================

/// A `VerifiedDeployment` for the registry tests below. Only preflight
/// can produce one in production; this is the test fixture standing in
/// for it, and it grants nothing beyond an operational Robinhood adapter.
fn verified_deployment() -> crate::robinhood::preflight::VerifiedDeployment {
    use crate::evm::{EvmAddress, EvmChainId, TxEnvelope};
    use crate::robinhood::auth::ProtocolChainPair;
    crate::robinhood::preflight::VerifiedDeployment {
        chain_id: EvmChainId::new(4663).unwrap(),
        bridge_contract: EvmAddress::from_bytes([0xb1; 20]),
        token: EvmAddress::from_bytes([0x70; 20]),
        token_decimals: 18,
        signers: [
            EvmAddress::from_bytes([0xa1; 20]),
            EvmAddress::from_bytes([0xa2; 20]),
            EvmAddress::from_bytes([0xa3; 20]),
        ],
        domain_separator: [0x5a; 32],
        glc_to_rhn_chains: ProtocolChainPair {
            source: 1001,
            dest: 2001,
        },
        rhn_to_glc_chains: ProtocolChainPair {
            source: 2001,
            dest: 1001,
        },
        sol_to_rhn_chains: ProtocolChainPair {
            source: 3001,
            dest: 2001,
        },
        rhn_to_sol_chains: ProtocolChainPair {
            source: 2001,
            dest: 3001,
        },
        tx_envelope: TxEnvelope::Eip1559,
        chain_has_base_fee: true,
    }
}
//
// The distinction every test below turns on: **capability is not
// enablement**. `capability()` answers "can the Goldcoin machinery in
// this build serve this route's Goldcoin leg", and nothing more. Opening
// a route additionally requires config, the ledger's `bridge_routes`
// state, the contract's own `routeEnabled`, preflight, a signer quorum,
// reserve availability and the pause state — none of which lives here.

/// The full matrix, in one place, so a change to any arm is visible as a
/// change to this table.
#[test]
fn the_goldcoin_adapter_capability_matrix() {
    let adapter = GoldcoinAdapter;
    let expected: [(Route, bool); 6] = [
        // Legacy: unchanged.
        (Route::GlcToSol, true),
        (Route::SolToGlc, true),
        // Goldcoin is the DESTINATION — the existing vault payout
        // machinery already sweeps this direction.
        (Route::RhnToGlc, true),
        // Goldcoin is the SOURCE — the deposit pipeline that serves it
        // is the same one `GlcToSol` uses, keyed on
        // `Direction::source_is_goldcoin`.
        (Route::GlcToRhn, true),
        // Neither leg is Goldcoin.
        (Route::SolToRhn, false),
        (Route::RhnToSol, false),
    ];
    for (route, operational) in expected {
        assert_eq!(
            adapter.capability(route).is_operational(),
            operational,
            "{route:?}"
        );
    }
}

/// `RhnToGlc` settles by building and broadcasting a Goldcoin vault
/// payout, which `Orchestrator::tick_goldcoin_payouts` already does for
/// every direction whose destination is Goldcoin. The capability says so.
#[test]
fn the_goldcoin_adapter_serves_the_rhn_to_glc_destination_leg() {
    assert!(GoldcoinAdapter.capability(Route::RhnToGlc).is_operational());
    // The machinery claim is not prose: this is the predicate the
    // orchestrator's payout sweep actually selects on.
    assert!(crate::ledger::Direction::RhnToGlc.destination_is_goldcoin());
    assert_eq!(Route::RhnToGlc.destination_chain(), Chain::Goldcoin);
}

/// `GlcToRhn`'s Goldcoin SOURCE leg is now served — the deposit pipeline
/// is keyed on `Direction::source_is_goldcoin`, not on a single
/// direction.
///
/// The claim is deliberately not prose: `source_is_goldcoin` is the
/// predicate the ledger, the indexer and the coin-selection exclusion all
/// actually branch on, so asserting it is asserting the machinery. This
/// says nothing about the route being OPEN — `default_enabled` is still
/// `false`, and this adapter is only the third of three gates.
#[test]
fn the_goldcoin_adapter_serves_the_glc_to_rhn_source_leg() {
    assert!(GoldcoinAdapter.capability(Route::GlcToRhn).is_operational());
    assert_eq!(Route::GlcToRhn.source_chain(), Chain::Goldcoin);
    assert!(crate::ledger::Direction::GlcToRhn.source_is_goldcoin());
    // Capability is not enablement, and the two must never be confused.
    assert!(!Route::GlcToRhn.default_enabled());
}

/// The legacy routes are untouched by the widening — asserted directly
/// rather than left to the matrix, because "did not change GlcToSol" is
/// the single most important property of this change.
#[test]
fn the_legacy_routes_are_unaffected_by_the_robinhood_widening() {
    for route in [Route::GlcToSol, Route::SolToGlc] {
        assert_eq!(GoldcoinAdapter.capability(route), Capability::Operational);
        assert_eq!(SolanaAdapter.capability(route), Capability::Operational);
    }
}

/// Neither Solana<->Robinhood route is a Goldcoin route, and this adapter
/// says so specifically rather than by reusing the source-leg reason.
#[test]
fn the_goldcoin_adapter_refuses_both_solana_robinhood_routes() {
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert_eq!(
            GoldcoinAdapter.capability(route),
            Capability::unavailable(GoldcoinAdapter::NOT_A_GOLDCOIN_ROUTE_REASON),
            "{route:?}"
        );
        // Structurally, too: neither leg is Goldcoin, so the route gate
        // never even asks this adapter about them.
        assert_ne!(route.source_chain(), Chain::Goldcoin);
        assert_ne!(route.destination_chain(), Chain::Goldcoin);
    }
}

/// The registry a verified deployment builds now has BOTH legs of BOTH
/// Goldcoin<->Robinhood routes operational — which is exactly the
/// capability gate opening, and exactly not the route opening. Every
/// other gate is untouched.
#[test]
fn a_verified_registry_has_both_legs_of_the_goldcoin_robinhood_routes_but_still_opens_nothing() {
    let registry = ChainRegistry::with_verified_robinhood(verified_deployment());
    for chain in [
        Route::RhnToGlc.source_chain(),
        Route::RhnToGlc.destination_chain(),
    ] {
        assert!(
            registry.capability(chain, Route::RhnToGlc).is_operational(),
            "{chain:?} must serve RhnToGlc"
        );
    }
    // GlcToRhn now has BOTH legs capable in a verified registry — and
    // still opens nothing, which is the whole point of this test: this
    // registry holds no enable flag, no ledger handle and no contract
    // read.
    for chain in [
        Route::GlcToRhn.source_chain(),
        Route::GlcToRhn.destination_chain(),
    ] {
        assert!(
            registry.capability(chain, Route::GlcToRhn).is_operational(),
            "{chain:?} must serve GlcToRhn"
        );
    }
    assert!(!Route::GlcToRhn.default_enabled());
    // The registry is a capability lookup. It holds no enable flag, no
    // ledger handle and no contract read, so it cannot open anything.
    assert!(!registry
        .capability(Chain::Solana, Route::RhnToGlc)
        .is_operational());
}

/// The legacy-only registry — what an unmodified production deployment
/// resolves to — is unchanged: the Robinhood adapter refuses every route,
/// so `RhnToGlc`'s Robinhood leg is closed however capable Goldcoin is.
#[test]
fn the_legacy_registry_still_closes_the_robinhood_leg() {
    let registry = ChainRegistry::legacy_only();
    assert!(registry
        .capability(Chain::Goldcoin, Route::RhnToGlc)
        .is_operational());
    assert!(!registry
        .capability(Chain::Robinhood, Route::RhnToGlc)
        .is_operational());
}
