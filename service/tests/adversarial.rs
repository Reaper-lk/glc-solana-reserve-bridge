//! Adversarial and boundary tests: replay, duplicate release, insufficient
//! reserve, concurrent-race-shaped attempts, and reconciliation-triggered
//! containment. Complements `restart_recovery.rs` (crash/restart) and the
//! unit tests colocated with each module (mock-RPC-level chain scenarios).

use glc_reserve_bridge_service::ledger::{
    CreateRequestOutcome, Direction, GlcObservationOutcome, Ledger, RequestState, ReserveDirection,
    SolFoldOutcome,
};
use glc_reserve_bridge_service::reconciliation::{self, Classification};

fn configure(ledger: &mut Ledger) {
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
}

#[test]
fn replaying_a_glc_deposit_against_a_different_request_is_rejected_by_the_unique_constraint() {
    // An attacker (or a buggy caller) tries to bind the SAME source
    // (txid, vout) to a SECOND request after it was already claimed by the
    // first — the UNIQUE index on (source_txid, source_vout) must refuse
    // this outright; it must never result in two requests both pointing at
    // one deposit.
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let CreateRequestOutcome::Reserved { request_id: first } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[1u8; 32], None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    let CreateRequestOutcome::Reserved { request_id: second } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[2u8; 32], None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    let outcome1 = ledger
        .record_glc_deposit_observed(first, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1)
        .unwrap();
    assert_eq!(outcome1, GlcObservationOutcome::Recorded);

    // Attempt to bind the identical (txid, vout) to the second request —
    // must be rejected by the SQLite UNIQUE constraint, surfaced as an
    // error, never silently accepted.
    let result =
        ledger.record_glc_deposit_observed(second, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 2);
    assert!(
        result.is_err(),
        "binding one Goldcoin deposit to two requests must be structurally rejected"
    );

    // The first request's binding is untouched.
    assert_eq!(
        ledger.get_request(first).unwrap().unwrap().source_txid,
        Some([0xAA; 32])
    );
    assert_eq!(
        ledger.get_request(second).unwrap().unwrap().source_txid,
        None
    );
}

#[test]
fn replaying_a_solana_obligation_index_never_produces_a_second_settlement_path() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let SolFoldOutcome::FoldedFinalized { request_id: first } = ledger
        .fold_sol_deposit(7, 100_000, [1u8; 32], b"addr", 0)
        .unwrap()
    else {
        panic!()
    };
    // A compromised or buggy caller replays the identical obligation index.
    let outcome = ledger
        .fold_sol_deposit(7, 100_000, [1u8; 32], b"addr", 1)
        .unwrap();
    assert_eq!(outcome, SolFoldOutcome::AlreadyFolded { request_id: first });

    // Only ONE request exists for this obligation index, ever.
    let all = ledger
        .requests_by_state(Direction::SolToGlc, RequestState::SourceFinalized)
        .unwrap();
    assert_eq!(all.len(), 1);
}

#[test]
fn ten_concurrent_shaped_reservations_never_oversubscribe_capacity() {
    // Sequential stand-in for concurrency (module docs: SQLite serializes
    // writers DB-wide, so the safety property under test — total granted
    // reservations never exceed available capacity — holds regardless of
    // arrival order or true parallelism).
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let available = ledger
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap() as u64;
    let per_request = available / 5 + 1; // 10 of these must not all succeed
    let mut granted = 0u64;
    for i in 0..10u8 {
        let outcome = ledger
            .create_request(Direction::GlcToSol, per_request, &[i; 32], None, 3600, 0)
            .unwrap();
        if let CreateRequestOutcome::Reserved { .. } = outcome {
            granted += per_request;
        }
    }
    assert!(
        granted <= available,
        "granted {granted} must never exceed available {available}"
    );
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn insufficient_reserve_at_creation_time_never_creates_a_liability() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let before = ledger
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap();
    let outcome = ledger
        .create_request(Direction::GlcToSol, 10_000_000, &[1u8; 32], None, 3600, 0)
        .unwrap();
    assert!(matches!(
        outcome,
        CreateRequestOutcome::InsufficientLiquidity { .. }
    ));
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        before
    );
    assert!(ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap()
        .is_empty());
}

#[test]
fn reconciliation_breach_blocks_new_reservations_but_never_reverses_committed_ones() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[1u8; 32], None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 2).unwrap();

    // Reconciliation detects an unexplained drop and pauses.
    let report =
        reconciliation::reconcile(&mut ledger, ReserveDirection::SolanaReserve, 500_000, 10, 5)
            .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());

    // The already-finalized (irreversible) request is untouched — pausing
    // stops NEW exposure, it does not revert an already-made promise.
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );

    // But a brand new reservation attempt is refused while paused.
    let outcome = ledger
        .create_request(Direction::GlcToSol, 1_000, &[9u8; 32], None, 3600, 6)
        .unwrap();
    assert_eq!(outcome, CreateRequestOutcome::Paused);
}

#[test]
fn expired_reservation_that_deposits_late_is_never_silently_credited_to_a_dead_request() {
    // A user deposits after their reservation already expired. The
    // Goldcoin indexer's binding lookup only matches AwaitingDeposit
    // requests, so a late deposit against an Expired request is correctly
    // reported as unmatched — never silently misapplied to an expired
    // reservation's stale capacity accounting. (The compensating
    // "auto-recreate if capacity allows" path from docs/04-state-machines.md
    // is service/orchestrator-level policy for a later phase; this test
    // pins the ledger-level safety property the compensating path builds
    // on: an expired request can never be resurrected by a late
    // observation alone.)
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[1u8; 32], None, 10, 0)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(ledger.expire_reservations(100).unwrap(), 1);
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Expired
    );

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 150)
        .unwrap();
    assert_eq!(
        outcome,
        GlcObservationOutcome::NoMatchingRequest,
        "an Expired request must not silently reopen"
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Expired,
        "state must not change"
    );
}

#[test]
#[should_panic(expected = "caller bug")]
fn attempting_to_reorg_a_request_that_was_never_observed_is_a_caller_bug() {
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[1u8; 32], None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    // Still AwaitingDeposit — nothing to reorg yet.
    let _ = ledger.mark_glc_reorged(request_id, 1);
}

#[test]
fn amount_mismatch_deposit_can_never_advance_toward_settlement_automatically() {
    // Even after many ticks / restarts, a ManualReview'd amount-mismatch
    // request must stay parked — nothing in the ledger API can move it
    // forward without an explicit operator action (which does not exist
    // yet in this phase, by design — see IMPLEMENTATION_LOG.md).
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, 100_000, &[1u8; 32], None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 50_000, 10, [0xBB; 32], 1)
        .unwrap();
    assert!(matches!(
        outcome,
        GlcObservationOutcome::AmountMismatch { .. }
    ));
    for _ in 0..5 {
        // Repeated re-observation attempts (e.g. indexer retries) must not
        // change anything further.
        let repeat = ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 50_000, 10, [0xBB; 32], 2);
        assert!(repeat.is_ok());
        assert_eq!(
            ledger.get_request(request_id).unwrap().unwrap().state,
            RequestState::ManualReview
        );
    }
}
