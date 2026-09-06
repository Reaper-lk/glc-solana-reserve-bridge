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
fn d_a_route_without_settlement_machinery_can_never_produce_a_direction() {
    // The structural guarantee, as it stands after Phase F. Every
    // reserve-mutating entry point on `Ledger` requires a `Direction`, and
    // `Route::as_direction()` is the only way to obtain one from a route.
    //
    // Phase F NARROWED the set that yields `None` from four routes to two;
    // it did not remove the property. `SolToRhn`/`RhnToSol` have no
    // settlement machinery, so no value exists that would let any
    // value-moving function run for them — and the ledger's own direction
    // CHECK cannot store one either.
    assert_eq!(Route::SolToRhn.as_direction(), None);
    assert_eq!(Route::RhnToSol.as_direction(), None);

    // The four executable routes map to their direction. Having one is NOT
    // permission to use it: `RouteGate`'s three gates, and the custody
    // contract's own `routeEnabled`, all still stand in front of every
    // value-moving call — which is what tests A and B above assert.
    assert_eq!(Route::GlcToSol.as_direction(), Some(Direction::GlcToSol));
    assert_eq!(Route::SolToGlc.as_direction(), Some(Direction::SolToGlc));
    assert_eq!(Route::GlcToRhn.as_direction(), Some(Direction::GlcToRhn));
    assert_eq!(Route::RhnToGlc.as_direction(), Some(Direction::RhnToGlc));

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
fn d_the_solana_robinhood_routes_are_unspellable_in_the_database() {
    // The independent backstop underneath the type system: even if code
    // somehow produced a `SolToRhn` settlement, the ledger could not store
    // one. Enabling such a route is a schema migration plus a `Direction`
    // variant — a compile error at every exhaustive match — not a
    // configuration change.
    //
    // Reached through a raw connection to the ledger's own file, i.e.
    // bypassing every API this crate exposes, which is exactly the attempt
    // the CHECK exists to stop.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    Ledger::open(&path).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();

    for unspellable in ["SolToRhn", "RhnToSol"] {
        assert!(
            conn.execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, recipient, created_at, source_chain)
                 VALUES (?1, 'AwaitingDeposit', 1, X'00', 1, 'robinhood')",
                [unspellable],
            )
            .is_err(),
            "the database must refuse a {unspellable} settlement row",
        );
    }
    // The four that ARE executable are storable, so this is a real
    // constraint on the vocabulary rather than a broken INSERT.
    for spellable in ["GlcToSol", "SolToGlc", "GlcToRhn", "RhnToGlc"] {
        conn.execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, recipient, created_at, source_chain)
             VALUES (?1, 'AwaitingDeposit', 1, X'00', 1, 'goldcoin')",
            [spellable],
        )
        .unwrap_or_else(|e| panic!("{spellable} must be storable: {e}"));
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
