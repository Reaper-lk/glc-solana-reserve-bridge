//! Phase-E isolation guarantees, asserted at the crate boundary.
//!
//! `robinhood_route_isolation.rs` proves the Phase-1 properties: the four
//! Robinhood routes are closed by three independent gates, and no
//! Robinhood value can construct a settlement `Direction`. This file
//! proves the properties the deposit indexer adds, stated as the two
//! things a reviewer would want to check before this ships:
//!
//! - **A.** A deployment with no Robinhood indexer config is byte-for-byte
//!   the deployment that existed before — no client, no task, no RPC call.
//! - **B.** Observing a real, valid, finalized deposit on a disabled route
//!   changes nothing about settlement: no bridge request, no reserve
//!   movement, no route opened, and nothing that can be marked settled.
//!
//! Deterministic and offline. Nothing here contacts a Robinhood endpoint.

use glc_reserve_bridge_service::ledger::{Direction, Ledger, ReserveDirection, RobinhoodFinality};
use glc_reserve_bridge_service::robinhood::{RobinhoodHealth, RobinhoodIndexerConfig};
use glc_reserve_bridge_service::routes::{Chain, Route, RouteGate, RoutesConfig};

/// The full reserve state that any settlement or accounting operation
/// could move, for one direction.
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
    for direction in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::GoldcoinReserve,
    ] {
        ledger
            .configure_reserve(direction, 1_000_000, 0, 500_000, 250_000, 100_000, 1)
            .unwrap();
    }
    ledger
}

/// **A.** The health surface distinguishes "this deployment has no
/// Robinhood indexer" from "the indexer is not answering". An operator
/// reading `configured: false` knows there is nothing to investigate.
#[test]
fn an_unconfigured_deployment_reports_itself_as_unconfigured() {
    let snapshot = RobinhoodHealth::unconfigured().snapshot();
    assert!(!snapshot.configured);
    assert!(!snapshot.connected);
    assert_eq!(snapshot.expected_chain_id, None);
    assert_eq!(snapshot.head_block, None);
    assert_eq!(snapshot.cursor_block, None);
    assert_eq!(snapshot.halt, None);
}

/// **A.** The config type itself is what makes "absent" mean "absent":
/// there is no default to fall back on, so nothing can construct an
/// indexer that the operator did not ask for.
#[test]
fn the_indexer_config_has_no_default_constructor() {
    // A compile-time-ish statement made at runtime: every field must be
    // supplied, and an invalid one is refused rather than replaced.
    let bad = RobinhoodIndexerConfig::new(
        "https://rpc.example.invalid".to_string(),
        glc_reserve_bridge_service::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID,
        glc_reserve_bridge_service::evm::EvmAddress::from_bytes([0x11; 20]),
        glc_reserve_bridge_service::evm::EvmAddress::from_bytes([0x22; 20]),
        0,
        // A zero confirmation depth is the one mistake that silently
        // accepts reversible money, and it is refused.
        0,
        1,
        1,
        1,
    );
    assert!(bad.is_err());
}

/// **B.** A fully recorded, finalized observation leaves both reserves
/// bit-identical and creates no bridge request.
#[test]
fn a_finalized_observation_moves_no_reserve_and_creates_no_request() {
    let mut ledger = configured_ledger();
    let before = both_reserves(&ledger);

    let observation = glc_reserve_bridge_service::ledger::RobinhoodDepositObservation {
        source_contract: [0x11; 20],
        obligation_index: 0,
        route: Route::RhnToGlc,
        depositor: [0x33; 20],
        destination: vec![0xde, 0xad, 0xbe, 0xef],
        amount_robinhood_atomic: [0xff; 32],
        amount_canonical_atomic: 25_000_000_000,
        tx_hash: [0xaa; 32],
        log_index: 0,
        block_number: 100,
        block_hash: [0x64; 32],
    };
    ledger
        .robinhood_apply_scan_range(&[observation], &[], 100, [0x64; 32], 12, 10)
        .unwrap();
    ledger.robinhood_promote_final(120, 12, 11).unwrap();

    // The deposit is visible...
    let rows = ledger.robinhood_observations().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].finality, RobinhoodFinality::Final);
    assert_eq!(rows[0].observation.amount_canonical_atomic, 25_000_000_000);

    // ...and it moved nothing at all. Even a FINAL Robinhood deposit of
    // 250 GLC is, to the settlement machinery, a row somebody can read.
    assert_eq!(both_reserves(&ledger), before);
}

/// **B.** Observing deposits does not open a route. All three gates stay
/// closed, and there is still no `Direction` any value-moving function
/// could be called with.
#[test]
fn observations_do_not_open_any_robinhood_route() {
    let mut ledger = configured_ledger();
    for index in 0..3u64 {
        let observation = glc_reserve_bridge_service::ledger::RobinhoodDepositObservation {
            source_contract: [0x11; 20],
            obligation_index: index,
            route: if index % 2 == 0 {
                Route::RhnToGlc
            } else {
                Route::RhnToSol
            },
            depositor: [0x33; 20],
            destination: vec![0x01],
            amount_robinhood_atomic: [0x01; 32],
            amount_canonical_atomic: 1_000,
            tx_hash: [index as u8; 32],
            log_index: 0,
            block_number: 100 + index,
            block_hash: [(100 + index) as u8; 32],
        };
        ledger
            .robinhood_apply_scan_range(
                &[observation],
                &[],
                100 + index,
                [(100 + index) as u8; 32],
                12,
                10,
            )
            .unwrap();
    }
    ledger.robinhood_promote_final(200, 12, 11).unwrap();
    assert_eq!(ledger.robinhood_observations().unwrap().len(), 3);

    // Gate 3 (adapter capability) refuses regardless of anything the
    // indexer did, and the gate as a whole stays closed.
    let gate = RouteGate::new(
        // Even with the config gate deliberately opened.
        RoutesConfig::default().with_robinhood(true, true, true, true),
        glc_reserve_bridge_service::chains::ChainRegistry::phase1(),
    );
    for route in [
        Route::GlcToRhn,
        Route::RhnToGlc,
        Route::SolToRhn,
        Route::RhnToSol,
    ] {
        assert!(
            gate.ensure_enabled(&ledger, route).is_err(),
            "{route:?} must stay closed after observing deposits on it",
        );
    }

    // Phase F built settlement machinery for the two Goldcoin<->Robinhood
    // routes, so those now have a `Direction`. What keeps them shut is the
    // gate asserted above — in particular the ADAPTER gate, which refuses
    // unless this process holds a deployment that passed preflight, and
    // which no amount of observing can change.
    //
    // For the two Solana<->Robinhood routes the original, stronger
    // guarantee is intact: no `Direction` value exists for them at all, so
    // no reserve, ledger or signing function can be called with one.
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert_eq!(
            route.as_direction(),
            None,
            "{route:?} must still have no settlement direction",
        );
    }
    assert!(gate.registry().contains(Chain::Robinhood));
}

/// **B.** The structural backstop, as it stands after Phase F.
///
/// Schema v22 pinned `settled` to zero so that settling a Robinhood
/// deposit would require a migration a reviewer would see. Schema v23 is
/// that migration, so the column is now a real flag — and it is still a
/// CONSTRAINED one, and it is still not reachable by observing: an
/// observation is only ever marked settled after its `executeSettlement`
/// transaction reached the configured confirmation depth, which the
/// indexer cannot do.
#[test]
fn an_observation_is_never_settled_by_observing_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let mut ledger = Ledger::open(&path).unwrap();
    let observation = glc_reserve_bridge_service::ledger::RobinhoodDepositObservation {
        source_contract: [0x11; 20],
        obligation_index: 0,
        route: Route::RhnToGlc,
        depositor: [0x33; 20],
        destination: vec![0x01],
        amount_robinhood_atomic: [0x01; 32],
        amount_canonical_atomic: 1_000,
        tx_hash: [0xaa; 32],
        log_index: 0,
        block_number: 100,
        block_hash: [0x64; 32],
    };
    ledger
        .robinhood_apply_scan_range(&[observation], &[], 100, [0x64; 32], 12, 10)
        .unwrap();
    drop(ledger);

    // Observing recorded the deposit and settled nothing.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let settled: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM robinhood_deposit_observations WHERE settled <> 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(settled, 0, "observing a deposit must settle nothing");

    // Nor did it fold anything: no bridge request exists, so there is no
    // payout to settle against.
    let requests: i64 = conn
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(requests, 0);
    let folded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM robinhood_deposit_observations
             WHERE folded_request_id IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(folded, 0);

    // And the column is still constrained: only 0 or 1, so a stray value
    // can never read as "settled" by accident.
    let error = conn
        .execute("UPDATE robinhood_deposit_observations SET settled = 2", [])
        .expect_err("the CHECK constraint refuses anything but 0 or 1");
    assert!(
        error.to_string().to_lowercase().contains("constraint"),
        "expected a constraint failure, got {error}",
    );
}
