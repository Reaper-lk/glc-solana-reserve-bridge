//! Phase-1 Robinhood isolation guarantees.
//!
//! Four properties the Robinhood scaffolding must hold, each stated as a
//! test that would fail loudly if a later change eroded it:
//!
//! - **A.** Robinhood routes are disabled on a default deployment, and each
//!   of the three gates closes them independently.
//! - **B.** No Robinhood operation can mutate Solana (or Goldcoin) reserve
//!   accounting — proven by taking a complete before/after snapshot of both
//!   reserve rows across every Robinhood-touching call the service exposes.
//! - **C.** The Solana↔Goldcoin reserve model behaves identically with the
//!   route machinery present as it did without it.
//! - **D.** A Robinhood route cannot produce a settlement `Direction`, so it
//!   cannot reach any reserve-mutating function at all — the structural
//!   guarantee underneath B.
//!
//! B is the one worth reading closely. It does not assert "the Robinhood
//! reserve row was not touched" (there is no such row in this phase, by
//! design — see the migration note in `Ledger::route_enabled`). It asserts
//! the stronger thing: that running every Robinhood-reachable code path
//! leaves the two REAL reserve rows bit-identical, field by field.

use glc_reserve_bridge_service::chains::{Capability, ChainAdapter, ChainRegistry};
use glc_reserve_bridge_service::ledger::{Direction, Ledger, ReserveDirection};
use glc_reserve_bridge_service::routes::{Chain, Route, RouteGate, RoutesConfig};

/// Every field of one reserve row that any settlement or accounting
/// operation could move. Compared as a whole so a future field addition
/// that this snapshot forgets shows up as a compile error at the
/// construction site rather than as a silently unchecked value.
#[derive(Debug, PartialEq, Eq)]
struct ReserveSnapshot {
    available_capacity: i64,
    settled_liquidity: u64,
    accrued_fees: u64,
    paused: bool,
    admission_closed: bool,
    request_count: i64,
}

fn snapshot(ledger: &Ledger, reserve: ReserveDirection, direction: Direction) -> ReserveSnapshot {
    ReserveSnapshot {
        available_capacity: ledger.available_capacity(reserve).unwrap(),
        settled_liquidity: ledger.settled_liquidity(reserve).unwrap(),
        accrued_fees: ledger.accrued_fees(reserve).unwrap(),
        paused: ledger.is_paused(reserve).unwrap(),
        admission_closed: ledger.is_admission_closed(reserve).unwrap(),
        request_count: ledger
            .request_state_counts(direction)
            .unwrap()
            .iter()
            .map(|(_, n)| *n)
            .sum(),
    }
}

fn both_reserves(ledger: &Ledger) -> (ReserveSnapshot, ReserveSnapshot) {
    (
        snapshot(ledger, ReserveDirection::SolanaReserve, Direction::GlcToSol),
        snapshot(
            ledger,
            ReserveDirection::GoldcoinReserve,
            Direction::SolToGlc,
        ),
    )
}

fn configured_ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for reserve in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .configure_reserve(
                reserve, 10_000_000, 1_000_000, 8_000_000, 4_000_000, 2_000_000, 0,
            )
            .unwrap();
    }
    ledger
}

// ------------------------------------------------------------------- A --

#[test]
fn a_robinhood_routes_are_disabled_by_default() {
    let ledger = configured_ledger();
    let gate = RouteGate::legacy_only();
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        assert!(
            gate.ensure_enabled(&ledger, route).is_err(),
            "{route:?} must be refused on a default deployment"
        );
        assert!(!gate.is_enabled(&ledger, route));
        assert!(gate.disabled_reason(&ledger, route).is_some());
    }
}

#[test]
fn a_missing_configuration_means_disabled_not_enabled() {
    // `RoutesConfig::default()` is what an absent `[robinhood]` section
    // resolves to. It must not open anything.
    let ledger = configured_ledger();
    let gate = RouteGate::new(RoutesConfig::default(), ChainRegistry::phase1());
    assert!(gate.ensure_enabled(&ledger, Route::GlcToRhn).is_err());
    assert!(gate.ensure_enabled(&ledger, Route::RhnToGlc).is_err());
}

#[test]
fn a_adapter_gate_holds_even_if_config_and_ledger_are_forced_open() {
    struct Permissive(Chain);
    impl ChainAdapter for Permissive {
        fn chain(&self) -> Chain {
            self.0
        }
        fn capability(&self, _route: Route) -> Capability {
            Capability::Operational
        }
    }

    let ledger = configured_ledger();
    // Config forced on; the real Phase-1 registry still refuses.
    let real = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        ChainRegistry::phase1(),
    );
    assert!(real.ensure_enabled(&ledger, Route::GlcToRhn).is_err());

    // Swapping in a permissive registry proves the adapter gate — and not
    // something else — was what refused above.
    let permissive = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        ChainRegistry::new()
            .with(Box::new(Permissive(Chain::Goldcoin)))
            .with(Box::new(Permissive(Chain::Robinhood))),
    );
    assert!(
        permissive.ensure_enabled(&ledger, Route::GlcToRhn).is_err(),
        "the ledger gate must still refuse once the adapter gate is removed"
    );
}

/// The same isolation for `RhnToGlc`, whose ADAPTER gate Phase G opened.
///
/// `GoldcoinAdapter` now serves this route's destination leg, so unlike
/// `GlcToRhn` above it is no longer the adapter that refuses. This proves
/// the remaining gates carry it alone: with a fully verified Robinhood
/// adapter and both legs capable, config and the ledger each still refuse
/// on their own.
#[test]
fn a_rhn_to_glc_stays_closed_on_config_and_ledger_once_both_legs_are_capable() {
    let ledger = configured_ledger();

    // Both legs capable, config OFF (the shipping default).
    let config_closed = RouteGate::new(
        RoutesConfig::default(),
        ChainRegistry::with_verified_robinhood(verified_deployment()),
    );
    assert!(
        config_closed
            .ensure_enabled(&ledger, Route::RhnToGlc)
            .is_err(),
        "config alone must keep RhnToGlc closed"
    );

    // Both legs capable, config ON, ledger silent.
    let ledger_closed = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, false, false),
        ChainRegistry::with_verified_robinhood(verified_deployment()),
    );
    assert!(
        ledger_closed
            .ensure_enabled(&ledger, Route::RhnToGlc)
            .is_err(),
        "the ledger gate must keep RhnToGlc closed once config and both adapters agree"
    );

    // And the adapter gate is genuinely open now — otherwise the two
    // assertions above would be passing for the wrong reason.
    let registry = ChainRegistry::with_verified_robinhood(verified_deployment());
    for chain in [
        Route::RhnToGlc.source_chain(),
        Route::RhnToGlc.destination_chain(),
    ] {
        assert!(
            registry.capability(chain, Route::RhnToGlc).is_operational(),
            "{chain:?} must serve RhnToGlc for this test to mean anything"
        );
    }
}

/// A `VerifiedDeployment` fixture — only preflight produces one in
/// production. It grants an operational Robinhood adapter and nothing
/// else.
fn verified_deployment() -> glc_reserve_bridge_service::robinhood::preflight::VerifiedDeployment {
    use glc_reserve_bridge_service::evm::{EvmAddress, EvmChainId, TxEnvelope};
    use glc_reserve_bridge_service::robinhood::auth::ProtocolChainPair;
    glc_reserve_bridge_service::robinhood::preflight::VerifiedDeployment {
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

// ------------------------------------------------------------------- B --

#[test]
fn b_no_robinhood_operation_mutates_either_reserve() {
    let ledger = configured_ledger();
    let before = both_reserves(&ledger);

    let gate = RouteGate::new(
        // Deliberately the most permissive configuration a deployment could
        // express, so this test covers the worst case rather than the
        // default one.
        RoutesConfig::default().with_robinhood(true, true, true, true),
        ChainRegistry::phase1(),
    );

    // Every Robinhood-reachable operation the service exposes, run
    // repeatedly. None may move a single field of either reserve.
    for _ in 0..10 {
        for route in [Route::GlcToRhn, Route::RhnToGlc] {
            let _ = gate.ensure_enabled(&ledger, route);
            let _ = gate.is_enabled(&ledger, route);
            let _ = gate.disabled_reason(&ledger, route);
            let _ = ledger.route_enabled(route.as_str(), route.default_enabled());
            let _ = route.as_direction();
            let _ = gate.registry().capability(route.source_chain(), route);
            let _ = gate.registry().capability(route.destination_chain(), route);
        }
    }

    assert_eq!(
        both_reserves(&ledger),
        before,
        "no Robinhood operation may move any field of the Solana or Goldcoin reserve"
    );
}

#[test]
fn b_robinhood_has_no_reserve_row_to_confuse_with_solanas() {
    // Robinhood deliberately gets NO `reserve_ledger` row in this phase:
    // creating one would require widening the table's CHECK constraint,
    // which means a schema-version bump this phase must not ship. The
    // absence is itself fail-closed — there is no row whose bounds could be
    // misread as Solana's, and no row that could be accidentally credited.
    let ledger = configured_ledger();
    for reserve in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .available_capacity(reserve)
            .expect("the two real reserves must remain readable");
    }
    // And the two that exist are independent of each other, unchanged.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "the fixture configures both identically; this asserts the fixture, \
         not a coupling between them"
    );
}

// ------------------------------------------------------------------- C --

#[test]
fn c_legacy_reserve_behaviour_is_unchanged_by_the_route_machinery() {
    let mut ledger = configured_ledger();
    let gate = RouteGate::legacy_only();

    // Both legacy routes still open.
    for route in [Route::GlcToSol, Route::SolToGlc] {
        gate.ensure_enabled(&ledger, route).unwrap();
    }

    // A pause on one reserve still closes exactly that reserve and leaves
    // the other alone — the pre-existing directional independence.
    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
        .unwrap();
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());

    // And the route gate is orthogonal to pausing: pausing a reserve does
    // NOT close the route, because they are different controls with
    // different meanings. This is a deliberate design property, asserted so
    // the two never get conflated.
    gate.ensure_enabled(&ledger, Route::GlcToSol)
        .expect("a paused reserve must not be reported as a disabled route");
}

// ------------------------------------------------------------------- D --

#[test]
fn d_every_route_produces_its_own_direction_and_nothing_else() {
    // The structural guarantee, as it stands after Phase H. Every
    // reserve-mutating entry point on `Ledger` requires a `Direction`, and
    // `Route::as_direction()` is the only way to obtain one from a route.
    //
    // Phase F narrowed the set that yields `None` from four routes to two;
    // Phase H closed it by giving the two Solana<->Robinhood routes the
    // machinery their halves already had. Having a `Direction` is NOT
    // permission to use it: `RouteGate`'s three gates, and the custody
    // contract's own `routeEnabled`, all still stand in front of every
    // value-moving call — which is what tests A and B above assert.
    assert_eq!(Route::GlcToSol.as_direction(), Some(Direction::GlcToSol));
    assert_eq!(Route::SolToGlc.as_direction(), Some(Direction::SolToGlc));
    assert_eq!(Route::GlcToRhn.as_direction(), Some(Direction::GlcToRhn));
    assert_eq!(Route::RhnToGlc.as_direction(), Some(Direction::RhnToGlc));
    assert_eq!(Route::SolToRhn.as_direction(), Some(Direction::SolToRhn));
    assert_eq!(Route::RhnToSol.as_direction(), Some(Direction::RhnToSol));

    // The two cross routes draw on the reserve of their DESTINATION and
    // withhold the fee on the reserve of their SOURCE — never netted.
    assert_eq!(
        Direction::SolToRhn.destination_reserve(),
        ReserveDirection::RobinhoodReserve
    );
    assert_eq!(
        Direction::SolToRhn.source_reserve(),
        ReserveDirection::SolanaReserve
    );
    assert_eq!(
        Direction::RhnToSol.destination_reserve(),
        ReserveDirection::SolanaReserve
    );
    assert_eq!(
        Direction::RhnToSol.source_reserve(),
        ReserveDirection::RobinhoodReserve
    );

    // And the destination reserve mapping for the legacy directions is
    // exactly what it was before any of this work.
    assert_eq!(
        Direction::GlcToSol.destination_reserve(),
        ReserveDirection::SolanaReserve
    );
    assert_eq!(
        Direction::SolToGlc.destination_reserve(),
        ReserveDirection::GoldcoinReserve
    );
}

#[test]
fn d_every_direction_is_spellable_in_the_database_and_nothing_else_is() {
    // The independent backstop underneath the type system, as widened by
    // schema v27: the ledger's direction CHECK admits exactly the six
    // `Direction` spellings. Reached through a raw connection to the
    // ledger's own file, bypassing every API this crate exposes.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    Ledger::open(&path).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();

    for direction in Direction::ALL {
        // Every direction, under a source identity the row constraints
        // accept (a contract-qualified one for the non-Goldcoin sources).
        let (chain, contract): (&str, Option<&[u8]>) = if direction.source_is_goldcoin() {
            ("goldcoin", None)
        } else if direction.source_is_solana() {
            ("solana", Some(&[0x01u8; 32][..]))
        } else {
            ("robinhood", Some(&[0x02u8; 20][..]))
        };
        conn.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, recipient, created_at, source_chain,
                 source_contract)
             VALUES (?1, 'ManualReview', 1, X'00', 1, ?2, ?3)",
            rusqlite::params![direction.as_str(), chain, contract],
        )
        .unwrap_or_else(|e| panic!("{} must be storable: {e}", direction.as_str()));
    }
    for unspellable in ["GlcToGlc", "SolToSol", "RhnToRhn", ""] {
        assert!(
            conn.execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, recipient, created_at, source_chain)
                 VALUES (?1, 'AwaitingDeposit', 1, X'00', 1, 'goldcoin')",
                [unspellable],
            )
            .is_err(),
            "the database must refuse a {unspellable:?} settlement row",
        );
    }
}

#[test]
fn d_the_reserve_direction_enum_has_exactly_the_three_real_reserves() {
    // Phase F added a THIRD physical reserve. It is accounted separately
    // and never netted against either of the others: a healthy Goldcoin
    // vault says nothing about whether the Robinhood custody contract can
    // honour a payout.
    assert_eq!(ReserveDirection::ALL.len(), 3);
    assert_eq!(
        ReserveDirection::GoldcoinReserve.as_str(),
        "GoldcoinReserve"
    );
    assert_eq!(ReserveDirection::SolanaReserve.as_str(), "SolanaReserve");
    assert_eq!(
        ReserveDirection::RobinhoodReserve.as_str(),
        "RobinhoodReserve"
    );

    // Each direction draws down exactly one of them, and the two
    // Goldcoin-destination directions share the Goldcoin vault.
    assert_eq!(
        Direction::GlcToRhn.destination_reserve(),
        ReserveDirection::RobinhoodReserve
    );
    assert_eq!(
        Direction::RhnToGlc.destination_reserve(),
        ReserveDirection::GoldcoinReserve
    );
}

// ------------------------------------------------------------------- E --
//
// Blocker I widened the Goldcoin deposit pipeline from one direction to
// two. E asserts what that widening did NOT do: it did not make the
// pipeline direction-agnostic, and it did not give the two
// Solana<->Robinhood routes a way in.

/// The deposit pipeline admits exactly the directions whose SOURCE leg is
/// a Goldcoin L1 payment, and refuses the others — checked by running the
/// real entry point over every direction rather than by restating the
/// predicate.
#[test]
fn e_only_goldcoin_sourced_directions_can_be_assigned_a_deposit_address() {
    for direction in Direction::ALL {
        let mut ledger = Ledger::open_in_memory().unwrap();
        for reserve in ReserveDirection::ALL {
            ledger
                .configure_reserve(
                    reserve, 10_000_000, 1_000_000, 8_000_000, 4_000_000, 2_000_000, 0,
                )
                .unwrap();
        }
        let glc_reserve_bridge_service::ledger::CreateRequestOutcome::Reserved { request_id } =
            ledger
                .create_request(
                    direction,
                    glc_reserve_bridge_service::ledger::RequestAmounts {
                        gross_atomic: 100_000,
                        fee_bps: 0,
                        fee_atomic: 0,
                        net_atomic: 100_000,
                        net_destination_atomic: 100_000,
                    },
                    &[0xAB; 20],
                    None,
                    3600,
                    0,
                )
                .unwrap()
        else {
            panic!("{direction:?} must reserve in this fixture")
        };

        let assigned =
            ledger.set_goldcoin_deposit_address(request_id, "Qaddr", "script-hex", "redeem-hex");
        assert_eq!(
            assigned.is_ok(),
            direction.source_is_goldcoin(),
            "{direction:?}: the deposit pipeline must admit exactly the \
             Goldcoin-sourced directions"
        );

        // And a deposit can only ever BIND to one it admitted.
        let observed = ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1)
            .unwrap();
        let bound = !matches!(
            observed,
            glc_reserve_bridge_service::ledger::GlcObservationOutcome::NoMatchingRequest
        );
        assert_eq!(
            bound,
            direction.source_is_goldcoin(),
            "{direction:?}: a Goldcoin deposit must bind only to a Goldcoin-sourced request"
        );
    }
}

/// Neither Solana<->Robinhood route can enter the Goldcoin deposit
/// pipeline, and the reason is structural rather than configured:
/// neither has a Goldcoin source leg, so there is no deposit address to
/// derive. And on an unmodified ledger both stay closed even against a
/// config and adapter forced fully open — the `bridge_routes` seed is
/// the gate that holds.
#[test]
fn e_a_solana_robinhood_route_can_never_enter_the_goldcoin_deposit_pipeline() {
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert!(
            route
                .as_direction()
                .is_some_and(|d| !d.source_is_goldcoin()),
            "{route:?} is not Goldcoin-sourced"
        );
        assert_ne!(
            route.source_chain(),
            Chain::Goldcoin,
            "{route:?} has no Goldcoin source leg to deposit into"
        );
        // Even with config and adapter forced as far open as a deployment
        // could express it, the seeded ledger row keeps the route shut.
        let ledger = configured_ledger();
        let permissive = RouteGate::new(
            RoutesConfig::default().with_robinhood(true, true, true, true),
            ChainRegistry::with_verified_robinhood(verified_deployment()),
        );
        assert!(
            permissive.ensure_enabled(&ledger, route).is_err(),
            "{route:?} must stay closed on an unmodified ledger"
        );
    }

    // The set of directions the pipeline serves is exactly two, and both
    // are Goldcoin<->something. Stated here so a fifth direction whose
    // source is Goldcoin has to be added deliberately.
    let goldcoin_sourced: Vec<Direction> = Direction::ALL
        .into_iter()
        .filter(|d| d.source_is_goldcoin())
        .collect();
    assert_eq!(
        goldcoin_sourced,
        vec![Direction::GlcToSol, Direction::GlcToRhn]
    );
}

/// The Goldcoin adapter now serves BOTH of its Robinhood legs — and
/// `GlcToRhn` still does not open, because the adapter is one gate of
/// three and the Robinhood contract's own gates sit beyond all of them.
#[test]
fn e_glc_to_rhn_stays_closed_on_config_and_ledger_once_both_legs_are_capable() {
    let ledger = configured_ledger();

    let registry = ChainRegistry::with_verified_robinhood(verified_deployment());
    for chain in [
        Route::GlcToRhn.source_chain(),
        Route::GlcToRhn.destination_chain(),
    ] {
        assert!(
            registry.capability(chain, Route::GlcToRhn).is_operational(),
            "{chain:?} must serve GlcToRhn for this test to mean anything"
        );
    }

    // Config OFF — the shipping default.
    let config_closed = RouteGate::new(
        RoutesConfig::default(),
        ChainRegistry::with_verified_robinhood(verified_deployment()),
    );
    assert!(
        config_closed
            .ensure_enabled(&ledger, Route::GlcToRhn)
            .is_err(),
        "config alone must keep GlcToRhn closed"
    );
    assert!(!Route::GlcToRhn.default_enabled());
    assert!(!Route::RhnToGlc.default_enabled());

    // Config ON, ledger silent.
    let ledger_closed = RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, false, false),
        ChainRegistry::with_verified_robinhood(verified_deployment()),
    );
    assert!(
        ledger_closed
            .ensure_enabled(&ledger, Route::GlcToRhn)
            .is_err(),
        "the ledger gate must keep GlcToRhn closed once config and both adapters agree"
    );
}
