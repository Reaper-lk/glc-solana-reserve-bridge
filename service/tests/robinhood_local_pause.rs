//! The LOCAL `RobinhoodReserve.paused` gate and its one operator
//! control, `glc-admin robinhood-local-pause`.
//!
//! # The production incident this closes
//!
//! `reserve_ledger.paused` on the `RobinhoodReserve` row has always been
//! READ: it is a term of `InboundAdmissionGates`, so it is a term of the
//! `available` verdict `GET /chains` publishes for `GlcToRhn`, and of
//! every `fold_robinhood_deposit`. It was not WRITABLE by any operator
//! surface — `glc-admin pause`/`unpause` and the admin API's
//! `POST /pause` both parse `goldcoin|solana` and reject everything else
//! — so a row left `paused=1` closed `GlcToRhn` with the contract
//! unpaused, both `routeEnabled` flags true, a healthy quorum, a holding
//! invariant and spare capacity, and no supported way to clear it.
//!
//! # What these tests pin
//!
//! That the flag is exactly the `GlcToRhn` local reserve gate: it closes
//! that route and no other, it is reachable, it is guarded on the way
//! back open, and it moves nothing else in the database. Every test
//! drives state through the same audited entry point the CLI uses
//! (`admin_api::audited_set_robinhood_local_pause`) or through the real
//! binary — never a direct SQLite write — so what is proven here is what
//! an operator can actually reach.

use std::path::Path;
use std::process::Command;

use glc_reserve_bridge_service::admin_api::audited_set_robinhood_local_pause;
use glc_reserve_bridge_service::chains::ChainRegistry;
use glc_reserve_bridge_service::ledger::{
    AdminAuditFilter, AdminAuditOutcome, Direction, InboundAdmissionBlocker, Ledger,
    ReserveDirection, RouteAdmissionState, RouteLedgerState,
};
use glc_reserve_bridge_service::ops::reserve_health;
use glc_reserve_bridge_service::routes::{Route, RouteGate, RoutesConfig};

const GLC: u64 = 100_000_000;
const NOW: i64 = 1_000;

/// A ledger with all THREE reserves funded and every gate open — a
/// healthy post-launch deployment, which is precisely the state the
/// incident occurred in.
fn setup() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    fund(&mut ledger);
    // The `bridge_routes` ENABLEMENT gate, opened the way an operator
    // opens it — a separate axis from everything under test, held open
    // so a route that is unavailable here is unavailable for the RESERVE
    // reason and not because it was never switched on.
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        ledger.set_route_enabled(route, true, None).unwrap();
    }
    ledger
}

fn fund(ledger: &mut Ledger) {
    for direction in ReserveDirection::ALL {
        ledger
            .configure_reserve(
                direction,
                10_000 * GLC,
                100 * GLC,
                5_000 * GLC,
                500 * GLC,
                200 * GLC,
                NOW,
            )
            .unwrap();
    }
}

/// The registry/config half of the route gate, forced fully open, so a
/// test asserting "GlcToRhn is available" is asserting about the RESERVE
/// gate rather than about enablement.
fn open_route_gate() -> RouteGate {
    RouteGate::new(
        RoutesConfig::default().with_robinhood(true, true, true, true),
        ChainRegistry::with_verified_robinhood(verified_deployment()),
    )
}

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

/// `GET /chains`'s own composition for one route: the three-place
/// enablement AND, then the SAME runtime evaluator `api::
/// route_availability` calls. Spelled out here rather than reaching for
/// a private function, but built only from the two public calls that
/// function itself makes, so it cannot answer differently.
fn available(gate: &RouteGate, ledger: &Ledger, route: Route) -> bool {
    let Some(direction) = route.as_direction() else {
        return false;
    };
    gate.is_enabled(ledger, route) && ledger.route_admission_blocker(direction).unwrap().is_none()
}

fn blocker(ledger: &Ledger, direction: Direction) -> Option<InboundAdmissionBlocker> {
    ledger.route_admission_blocker(direction).unwrap()
}

fn pause(ledger: &mut Ledger, paused: bool, note: &str) -> Result<(), String> {
    audited_set_robinhood_local_pause(ledger, paused, note, "cli:test")
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Everything in the database this command must never move, captured as
/// one comparable value: all three reserve rows in full, the
/// `bridge_routes` enablement table, and the `route_admission` table.
#[derive(Debug, PartialEq)]
struct UntouchedState {
    reserves: Vec<reserve_health::ReserveSnapshot>,
    routes: Option<RouteLedgerState>,
    route_admission: Option<RouteAdmissionState>,
}

fn untouched_state(ledger: &Ledger, skip: Option<ReserveDirection>) -> UntouchedState {
    UntouchedState {
        reserves: ReserveDirection::ALL
            .into_iter()
            .filter(|d| Some(*d) != skip)
            .map(|d| reserve_health::check(ledger, d, NOW).unwrap())
            .collect(),
        routes: ledger.route_ledger_rows().unwrap(),
        route_admission: ledger.route_admission_rows().unwrap(),
    }
}

// ===================================================================
// 1 & 2 — the flag IS GlcToRhn's local reserve gate
// ===================================================================

#[test]
fn pausing_the_robinhood_reserve_makes_glc_to_rhn_unavailable() {
    let mut ledger = setup();
    let gate = open_route_gate();

    assert!(
        available(&gate, &ledger, Route::GlcToRhn),
        "precondition: with every gate open GlcToRhn must be available"
    );

    pause(&mut ledger, true, "incident OPS-1300").unwrap();

    assert!(ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
    assert!(
        !available(&gate, &ledger, Route::GlcToRhn),
        "a paused RobinhoodReserve must close GlcToRhn"
    );
    assert_eq!(
        blocker(&ledger, Direction::GlcToRhn),
        Some(InboundAdmissionBlocker::ReservePaused),
        "and it must close it as ReservePaused — not as some other gate"
    );
    // Enablement is a different axis and stays open, which is the whole
    // point: the route is switched ON and still unavailable.
    assert!(
        gate.is_enabled(&ledger, Route::GlcToRhn),
        "the pause must not be reported as, or achieved through, enablement"
    );
}

#[test]
fn unpausing_makes_glc_to_rhn_available_again_when_every_other_gate_is_open() {
    let mut ledger = setup();
    let gate = open_route_gate();

    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    assert!(!available(&gate, &ledger, Route::GlcToRhn));

    pause(&mut ledger, false, "incident OPS-1300 resolved").unwrap();

    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
    assert_eq!(blocker(&ledger, Direction::GlcToRhn), None);
    assert!(
        available(&gate, &ledger, Route::GlcToRhn),
        "clearing the local gate with every other gate open must restore GlcToRhn"
    );
}

/// The exact production shape: the reserve arrives already paused and
/// nothing else is wrong. One command has to be enough.
#[test]
fn a_reserve_that_starts_paused_is_recoverable_with_one_command() {
    let mut ledger = setup();
    let gate = open_route_gate();
    pause(&mut ledger, true, "seeded paused").unwrap();

    assert!(ledger
        .check_invariant(ReserveDirection::RobinhoodReserve)
        .is_ok());
    assert!(
        ledger
            .available_capacity(ReserveDirection::RobinhoodReserve)
            .unwrap()
            > 0
    );
    assert!(!available(&gate, &ledger, Route::GlcToRhn));

    pause(&mut ledger, false, "OPS-1400 reopening GlcToRhn").unwrap();

    assert!(available(&gate, &ledger, Route::GlcToRhn));
}

// ===================================================================
// 3, 4, 5 — no other route moves
// ===================================================================

#[test]
fn rhn_to_glc_is_unaffected_in_both_directions() {
    let mut ledger = setup();
    let gate = open_route_gate();

    let before = blocker(&ledger, Direction::RhnToGlc);
    assert_eq!(before, None, "precondition: RhnToGlc starts open");

    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    assert_eq!(
        blocker(&ledger, Direction::RhnToGlc),
        None,
        "RhnToGlc settles out of GoldcoinReserve — the Robinhood reserve's pause cannot gate it"
    );
    assert!(available(&gate, &ledger, Route::RhnToGlc));
    assert!(
        !ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "GoldcoinReserve.paused — RhnToGlc's actual local gate — must not have moved"
    );

    pause(&mut ledger, false, "resolved").unwrap();
    assert_eq!(blocker(&ledger, Direction::RhnToGlc), None);
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
}

#[test]
fn sol_to_glc_is_unaffected_in_both_directions() {
    let mut ledger = setup();
    assert_eq!(blocker(&ledger, Direction::SolToGlc), None);

    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    assert_eq!(blocker(&ledger, Direction::SolToGlc), None);

    pause(&mut ledger, false, "resolved").unwrap();
    assert_eq!(blocker(&ledger, Direction::SolToGlc), None);
}

#[test]
fn glc_to_sol_is_unaffected_in_both_directions() {
    let mut ledger = setup();
    assert_eq!(blocker(&ledger, Direction::GlcToSol), None);

    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    assert_eq!(blocker(&ledger, Direction::GlcToSol), None);
    assert!(
        !ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
        "SolanaReserve.paused — GlcToSol's local gate — must not have moved"
    );

    pause(&mut ledger, false, "resolved").unwrap();
    assert_eq!(blocker(&ledger, Direction::GlcToSol), None);
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

// ===================================================================
// 6 & 7 — nothing but the one flag changes
// ===================================================================

/// The strong form of "it touches only `RobinhoodReserve.paused`": every
/// other reserve row, `bridge_routes` (route ENABLEMENT) and
/// `route_admission` are compared field by field across both a pause and
/// an unpause.
///
/// `bridge_routes` carries `updated_at` and `disabled_reason`, so this
/// also proves the rows were not rewritten with identical values.
#[test]
fn route_enablement_and_every_other_reserve_are_left_byte_identical() {
    let mut ledger = setup();
    // Give `bridge_routes` real, non-default content so an accidental
    // rewrite has something to destroy.
    ledger
        .set_route_enabled(Route::GlcToRhn, true, None)
        .unwrap();
    ledger
        .set_route_enabled(Route::RhnToGlc, false, Some("staged rollout"))
        .unwrap();

    let before = untouched_state(&ledger, Some(ReserveDirection::RobinhoodReserve));

    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    assert_eq!(
        untouched_state(&ledger, Some(ReserveDirection::RobinhoodReserve)),
        before,
        "pausing moved something outside the RobinhoodReserve row"
    );

    pause(&mut ledger, false, "resolved").unwrap();
    assert_eq!(
        untouched_state(&ledger, Some(ReserveDirection::RobinhoodReserve)),
        before,
        "unpausing moved something outside the RobinhoodReserve row"
    );

    // And within the Robinhood row itself, only `paused` moved.
    let after = reserve_health::check(&ledger, ReserveDirection::RobinhoodReserve, NOW).unwrap();
    let mut expected = after;
    expected.paused = true;
    pause(&mut ledger, true, "again").unwrap();
    assert_eq!(
        reserve_health::check(&ledger, ReserveDirection::RobinhoodReserve, NOW).unwrap(),
        expected,
        "pausing changed a RobinhoodReserve field other than `paused`"
    );

    // Enablement specifically, read back through its own accessor.
    assert!(ledger.route_enabled("GlcToRhn", false).unwrap());
    assert!(!ledger.route_enabled("RhnToGlc", true).unwrap());
}

/// The CONTRACT's `depositsPaused`/`payoutsPaused`/`routeEnabled` are
/// on-chain state this service neither stores nor caches, so the proof
/// that they are untouched is structural: the command has no surface
/// that could reach them.
///
/// It takes `--db` and nothing else — no `--config`, no `--rpc-url`, no
/// `--keypair`, no `--execute` — so it cannot construct an RPC client,
/// load an authorization key, or submit a transaction. This test runs the
/// REAL binary with no network, no config file and no keys available, and
/// requires it to succeed: a command that contacted a chain could not.
#[test]
fn the_command_cannot_reach_contract_flags_or_any_chain() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed_file_ledger(&db);

    let out = run_admin(&[
        "robinhood-local-pause",
        "--db",
        db.to_str().unwrap(),
        "--paused",
        "true",
        "--note",
        "OPS-1300 emergency stop",
    ]);
    assert!(out.status.success(), "command failed: {}", out.stderr);

    // Its own output states the scope, and names what it did not touch.
    assert!(
        out.stdout.contains("GlcToRhn local reserve gate only"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("RhnToGlc is NOT controlled by this flag"),
        "{}",
        out.stdout
    );
    assert!(out.stdout.contains("depositsPaused"), "{}", out.stdout);
    assert!(out.stdout.contains("routeEnabled"), "{}", out.stdout);
    assert!(out.stdout.contains("bridge_routes"), "{}", out.stdout);
    assert!(out.stdout.contains("config.toml"), "{}", out.stdout);

    // Before/after, printed.
    assert!(out.stdout.contains("before"), "{}", out.stdout);
    assert!(out.stdout.contains("paused=false"), "{}", out.stdout);
    assert!(out.stdout.contains("paused=true"), "{}", out.stdout);

    // The flag really moved, in the file the operator named.
    let ledger = Ledger::open(&db).unwrap();
    assert!(ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

/// `--note` is mandatory, and `--paused` is not a value to guess at.
#[test]
fn the_command_refuses_a_missing_note_and_a_non_boolean_paused() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed_file_ledger(&db);
    let db = db.to_str().unwrap().to_string();

    let out = run_admin(&["robinhood-local-pause", "--db", &db, "--paused", "true"]);
    assert!(!out.status.success());
    assert!(out.stderr.contains("--note is required"), "{}", out.stderr);

    let out = run_admin(&[
        "robinhood-local-pause",
        "--db",
        &db,
        "--paused",
        "yes",
        "--note",
        "n",
    ]);
    assert!(!out.status.success());
    assert!(
        out.stderr.contains("must be exactly `true` or `false`"),
        "{}",
        out.stderr
    );

    // Neither refusal moved the flag.
    let ledger = Ledger::open(Path::new(&db)).unwrap();
    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
}

/// `--help` must name the command, and must keep saying which of the
/// four Robinhood "pause"-shaped flags it is.
#[test]
fn help_documents_the_command_and_its_scope() {
    let out = run_admin(&["--help"]);
    assert!(out.status.success());
    assert!(
        out.stdout.contains("glc-admin robinhood-local-pause"),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("THE `GlcToRhn` LOCAL RESERVE GATE"),
        "--help must state the scope"
    );
    assert!(
        out.stdout
            .contains("RHNTOGLC IS NOT CONTROLLED BY THIS FLAG")
            || out
                .stdout
                .contains("RhnToGlc IS NOT CONTROLLED BY THIS FLAG"),
        "--help must state that RhnToGlc is not controlled by this flag"
    );
    assert!(
        out.stdout
            .contains("--direction\n      robinhood` is deliberately not accepted")
            || out
                .stdout
                .contains("robinhood` is deliberately not accepted"),
        "--help must say why `pause --direction robinhood` does not exist"
    );
}

/// Both read-only status surfaces must explain the flag, so an operator
/// reading `paused=true` is told which flag it is and how to clear it.
#[test]
fn robinhood_status_explains_which_pause_this_is() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed_file_ledger(&db);

    let out = run_admin(&["robinhood-status", "--db", db.to_str().unwrap()]);
    assert!(out.status.success(), "{}", out.stderr);
    assert!(
        out.stdout.contains("the LOCAL GlcToRhn reserve gate"),
        "robinhood-status must name the flag: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("glc-admin robinhood-local-pause"),
        "robinhood-status must name the remedy: {}",
        out.stdout
    );
    assert!(
        out.stdout.contains("depositsPaused/payoutsPaused"),
        "robinhood-status must name what the flag is NOT: {}",
        out.stdout
    );
}

// ===================================================================
// 8 — the audit trail
// ===================================================================

#[test]
fn every_change_and_every_refusal_writes_an_audit_row() {
    let mut ledger = setup();

    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    let row = rows.first().expect("a pause must be audited");
    assert_eq!(row.action, "pause");
    assert_eq!(row.target.as_deref(), Some("robinhood"));
    assert_eq!(row.old_value.as_deref(), Some("paused=false"));
    assert_eq!(row.new_value.as_deref(), Some("paused=true"));
    assert_eq!(row.note, "incident OPS-1300");
    assert_eq!(row.actor, "cli:test");
    assert!(matches!(row.outcome, AdminAuditOutcome::Success));

    pause(&mut ledger, false, "resolved").unwrap();
    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    let row = rows.first().expect("an unpause must be audited");
    assert_eq!(row.action, "unpause");
    assert_eq!(row.old_value.as_deref(), Some("paused=true"));
    assert_eq!(row.new_value.as_deref(), Some("paused=false"));
    assert!(matches!(row.outcome, AdminAuditOutcome::Success));

    // A REFUSED unpause is audit-relevant too: someone tried.
    pause(&mut ledger, true, "second stop").unwrap();
    break_the_invariant(&mut ledger);
    let err = pause(&mut ledger, false, "trying to reopen").unwrap_err();
    assert!(err.contains("reserve invariant does not hold"), "{err}");

    let rows = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap();
    let row = rows
        .first()
        .expect("a refused unpause must still be audited");
    assert_eq!(row.action, "unpause");
    assert_eq!(row.target.as_deref(), Some("robinhood"));
    assert_eq!(row.note, "trying to reopen");
    match &row.outcome {
        AdminAuditOutcome::Error(message) => {
            assert!(message.contains("refusing to unpause"), "{message}")
        }
        other => panic!("a refusal must be audited as an error, got {other:?}"),
    }
    assert!(
        ledger
            .is_paused(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        "a refused unpause must leave the flag exactly as it was"
    );
}

// ===================================================================
// 9 — idempotence
// ===================================================================

#[test]
fn repeated_pause_and_unpause_are_idempotent() {
    let mut ledger = setup();
    let gate = open_route_gate();

    for _ in 0..3 {
        pause(&mut ledger, true, "incident OPS-1300").unwrap();
        assert!(ledger
            .is_paused(ReserveDirection::RobinhoodReserve)
            .unwrap());
        assert!(!available(&gate, &ledger, Route::GlcToRhn));
    }
    for _ in 0..3 {
        pause(&mut ledger, false, "resolved").unwrap();
        assert!(!ledger
            .is_paused(ReserveDirection::RobinhoodReserve)
            .unwrap());
        assert!(available(&gate, &ledger, Route::GlcToRhn));
    }

    // Idempotent in EFFECT, never silent: each attempt is its own
    // audited action, and a repeat records the no-op honestly as
    // `paused=true -> paused=true` rather than inventing a transition.
    let repeats: Vec<_> = ledger
        .list_admin_audit(&AdminAuditFilter::default())
        .unwrap()
        .into_iter()
        .filter(|r| r.action == "pause" && r.old_value.as_deref() == Some("paused=true"))
        .collect();
    assert_eq!(
        repeats.len(),
        2,
        "each repeated pause must leave its own row"
    );
    for row in repeats {
        assert_eq!(row.new_value.as_deref(), Some("paused=true"));
        assert!(matches!(row.outcome, AdminAuditOutcome::Success));
    }
}

/// Through the real binary, twice, to prove the CLI path is idempotent
/// too and not merely the library call underneath it.
#[test]
fn the_command_is_idempotent_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    seed_file_ledger(&db);
    let db = db.to_str().unwrap().to_string();

    for _ in 0..2 {
        let out = run_admin(&[
            "robinhood-local-pause",
            "--db",
            &db,
            "--paused",
            "true",
            "--note",
            "OPS-1300",
        ]);
        assert!(out.status.success(), "{}", out.stderr);
        assert!(out.stdout.contains("paused=true"), "{}", out.stdout);
    }
    for _ in 0..2 {
        let out = run_admin(&[
            "robinhood-local-pause",
            "--db",
            &db,
            "--paused",
            "false",
            "--note",
            "OPS-1300 done",
        ]);
        assert!(out.status.success(), "{}", out.stderr);
        assert!(out.stdout.contains("paused=false"), "{}", out.stdout);
    }
    let ledger = Ledger::open(Path::new(&db)).unwrap();
    assert!(!ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
}

// ===================================================================
// 10 & 11 — the unpause guard
// ===================================================================

/// Raises `protected_minimum` above the balance, so
/// `total_reserve_balance < protected_minimum + reserved_liquidity`.
fn break_the_invariant(ledger: &mut Ledger) {
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            10_000 * GLC,
            20_000 * GLC,
            30_000 * GLC,
            25_000 * GLC,
            20_001 * GLC,
            NOW,
        )
        .unwrap();
    assert!(ledger
        .check_invariant(ReserveDirection::RobinhoodReserve)
        .is_err());
}

/// Leaves the hard invariant intact (`balance == protected_minimum`) but
/// with zero unreserved headroom — solvent, and unable to fund even the
/// smallest new transfer.
fn exhaust_capacity(ledger: &mut Ledger) {
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            10_000 * GLC,
            10_000 * GLC,
            30_000 * GLC,
            25_000 * GLC,
            10_001 * GLC,
            NOW,
        )
        .unwrap();
    assert!(
        ledger
            .check_invariant(ReserveDirection::RobinhoodReserve)
            .is_ok(),
        "this fixture must leave the hard invariant HOLDING — otherwise it \
         proves the invariant check, not the capacity check"
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        0
    );
}

#[test]
fn unpause_refuses_a_broken_reserve_invariant() {
    let mut ledger = setup();
    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    break_the_invariant(&mut ledger);

    let err = pause(&mut ledger, false, "reopening").unwrap_err();
    assert!(err.contains("refusing to unpause"), "{err}");
    assert!(err.contains("RobinhoodReserve"), "{err}");
    assert!(err.contains("reserve invariant does not hold"), "{err}");
    assert!(
        ledger
            .is_paused(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        "the refusal must leave the emergency stop engaged"
    );
}

#[test]
fn unpause_refuses_an_unsafe_capacity_state_even_with_the_invariant_intact() {
    let mut ledger = setup();
    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    exhaust_capacity(&mut ledger);

    let err = pause(&mut ledger, false, "reopening").unwrap_err();
    assert!(err.contains("refusing to unpause"), "{err}");
    assert!(
        err.contains(InboundAdmissionBlocker::InsufficientCapacity.as_str()),
        "the refusal must name the gate that would still refuse GlcToRhn: {err}"
    );
    assert!(ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
}

/// The guard is asymmetric on purpose: an emergency stop is never
/// refused, however bad the reserve looks.
#[test]
fn pause_is_always_allowed_even_on_a_broken_reserve() {
    let mut ledger = setup();
    break_the_invariant(&mut ledger);

    pause(&mut ledger, true, "stop everything, OPS-1300").unwrap();
    assert!(ledger
        .is_paused(ReserveDirection::RobinhoodReserve)
        .unwrap());
}

/// Once the reserve is healthy again the same command that was refused
/// succeeds — the guard is a condition, not a dead end.
#[test]
fn unpause_succeeds_once_the_reserve_recovers() {
    let mut ledger = setup();
    let gate = open_route_gate();
    pause(&mut ledger, true, "incident OPS-1300").unwrap();
    exhaust_capacity(&mut ledger);
    assert!(pause(&mut ledger, false, "too early").is_err());

    fund(&mut ledger); // protected_minimum back to a healthy 100 GLC
    pause(&mut ledger, false, "reserve topped up, OPS-1300 closed").unwrap();
    assert!(available(&gate, &ledger, Route::GlcToRhn));
}

/// The guard must not refuse on gates that are OTHER operators'
/// deliberate switches — those have their own audited commands, and
/// making this command's success depend on them would couple two
/// controls that the design keeps independent.
#[test]
fn unpause_is_not_blocked_by_an_unrelated_operator_switch() {
    let mut ledger = setup();
    let gate = open_route_gate();
    pause(&mut ledger, true, "incident OPS-1300").unwrap();

    // Close reserve-wide Goldcoin admission — the RhnToGlc/SolToGlc
    // axis, which has nothing to do with GlcToRhn.
    glc_reserve_bridge_service::admin_api::audited_set_admission(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        true,
        "unrelated OPS-1301",
        "cli:test",
    )
    .unwrap();

    pause(&mut ledger, false, "OPS-1300 resolved").unwrap();
    assert!(available(&gate, &ledger, Route::GlcToRhn));
    assert!(
        ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "and the unrelated switch stays exactly where its own operator left it"
    );
}

// ===================================================================
// Harness
// ===================================================================

fn seed_file_ledger(db: &Path) {
    let mut ledger = Ledger::open(db).unwrap();
    fund(&mut ledger);
}

struct Output {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run_admin(args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_glc-admin"))
        .args(args)
        .output()
        .expect("could not run glc-admin");
    Output {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}
