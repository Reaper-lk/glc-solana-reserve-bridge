use super::*;

/// A fee-free `RequestAmounts` for structural/lifecycle tests that predate
/// the bridge fee and don't care about fee math (docs/20-bridge-fee.md) —
/// dedicated fee/accounting behavior is covered separately. The ledger
/// itself never validates `fee_bps`/computes a fee, so this is a legitimate
/// (if unrealistic) input from the ledger's point of view.
fn amounts(gross: u64) -> RequestAmounts {
    RequestAmounts {
        gross_atomic: gross,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: gross,
        net_destination_atomic: gross,
    }
}

fn setup() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            1_000_000,
            100_000,
            500_000,
            200_000,
            150_000,
            1_000,
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
            1_000,
        )
        .unwrap();
    ledger
}

#[test]
fn available_capacity_is_balance_minus_minimum_minus_reserved() {
    let ledger = setup();
    // 1_000_000 - 100_000 - 0
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
}

#[test]
fn create_request_reserves_capacity_and_never_exceeds_it() {
    let mut ledger = setup();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert!(matches!(outcome, CreateRequestOutcome::Reserved { .. }));
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn create_request_capacity_check_is_based_on_net_destination_not_gross_amount() {
    // available capacity for SolanaReserve is 900_000 (balance 1_000_000 -
    // protected_minimum 100_000, see `setup`). A gross far beyond that
    // must still succeed as long as the fee-adjusted NET destination
    // payout fits exactly — proving the capacity check is against
    // `net_destination_atomic`, not `gross_atomic` (docs/20-bridge-fee.md).
    let mut ledger = setup();
    let net_at_capacity = RequestAmounts {
        gross_atomic: 5_000_000, // far beyond 900_000 if checked against gross
        fee_bps: 100,
        fee_atomic: 4_100_000,
        net_atomic: 900_000,
        net_destination_atomic: 900_000, // exactly at available capacity
    };
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            net_at_capacity,
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert!(
        matches!(outcome, CreateRequestOutcome::Reserved { .. }),
        "a huge gross must still be accepted when its net destination payout fits capacity"
    );
}

#[test]
fn create_request_rejects_when_net_destination_exceeds_capacity_even_for_a_small_gross() {
    // Inverse of the test above: a small gross whose net_destination_atomic
    // exceeds capacity must be rejected — a small gross figure alone
    // guarantees nothing about whether the destination reserve can
    // actually cover the release (docs/20-bridge-fee.md).
    let mut ledger = setup();
    let net_over_capacity = RequestAmounts {
        gross_atomic: 1_000,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: 1_000,
        net_destination_atomic: 950_000, // exceeds the 900_000 available
    };
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            net_over_capacity,
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert_eq!(
        outcome,
        CreateRequestOutcome::InsufficientLiquidity {
            available_capacity: 900_000
        },
        "insufficient destination reserve must be judged on the net payout, not the gross amount"
    );
}

#[test]
fn create_request_rejects_when_capacity_insufficient_never_creates_a_row() {
    let mut ledger = setup();
    // available is 900_000; ask for more than that.
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(950_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert_eq!(
        outcome,
        CreateRequestOutcome::InsufficientLiquidity {
            available_capacity: 900_000
        }
    );
    // Never accept a transfer that cannot be fulfilled: no capacity touched.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    let none = ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn create_request_rejects_when_direction_is_paused() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("test"))
        .unwrap();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(1_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert_eq!(outcome, CreateRequestOutcome::Paused);
}

#[test]
fn concurrent_reservations_never_double_spend_the_same_capacity() {
    // Sequential calls stand in for "concurrent" here since sqlite
    // serializes writers DB-wide (module docs) — the property under test is
    // that two reservations summing to more than available capacity cannot
    // both succeed, regardless of arrival order.
    let mut ledger = setup();
    let available = ledger
        .available_capacity(ReserveDirection::SolanaReserve)
        .unwrap();
    let half = available / 2 + 1; // two of these exceed capacity
    let first = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(half as u64),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    let second = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(half as u64),
            &[2u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    assert!(matches!(first, CreateRequestOutcome::Reserved { .. }));
    assert!(matches!(
        second,
        CreateRequestOutcome::InsufficientLiquidity { .. }
    ));
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn expire_reservations_releases_capacity_and_is_idempotent() {
    let mut ledger = setup();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            10,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = outcome else {
        panic!()
    };

    // Not yet expired.
    assert_eq!(ledger.expire_reservations(1_005).unwrap(), 0);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );

    // Past expiry.
    let expired = ledger.expire_reservations(1_020).unwrap();
    assert_eq!(expired, 1);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Expired);

    // Idempotent: running again finds nothing more to expire.
    assert_eq!(ledger.expire_reservations(1_030).unwrap(), 0);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn cancel_request_releases_capacity() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .cancel_request(request_id, 1_001, "user requested")
        .unwrap();
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000
    );
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Cancelled
    );
}

// -------------------------------------------------------------- Goldcoin leg --

#[test]
fn glc_deposit_flows_from_awaiting_through_confirming_to_finalized() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::Recorded);
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert_eq!(req.source_txid, Some([0xAA; 32]));

    // pending_obligations now holds the committed amount.
    let pending: i64 = ledger
        .raw()
        .query_row(
            "SELECT pending_obligations FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 100_000);
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn mark_release_confirmed_decrements_total_reserve_balance_immediately() {
    // Regression: a real-node run against a real solana-test-validator
    // paused the reserve permanently right after a completely legitimate
    // settlement. Root cause: `total_reserve_balance` was only ever
    // refreshed by reconciliation's own periodic live read, never by the
    // settlement path itself. So the very next reconciliation after a
    // confirmed release compared a *stale* cached balance (pre-settlement)
    // against the real, already-lower on-chain balance, saw an
    // "unexplained" drop exactly equal to the amount this service itself
    // just released, and latched a one-way pause
    // (docs/05-reserve-accounting.md's never-auto-unpause design) even
    // though nothing anomalous had happened. `mark_release_confirmed` must
    // keep the cache self-consistent with settlements it causes, not leave
    // that to the next reconcile.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    ledger
        .record_release_submitted(request_id, [0xCC; 64], 1_300)
        .unwrap();

    let balance_before: i64 = ledger
        .raw()
        .query_row(
            "SELECT total_reserve_balance FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    ledger.mark_release_confirmed(request_id, 1_400).unwrap();

    let balance_after: i64 = ledger
        .raw()
        .query_row(
            "SELECT total_reserve_balance FROM reserve_ledger WHERE direction = 'SolanaReserve'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        balance_after,
        balance_before - 100_000,
        "confirming a release must immediately decrement the cached reserve balance by the \
         settled amount, so the very next reconciliation sees a matching (not stale) baseline"
    );
}

#[test]
fn replaying_the_same_glc_observation_after_restart_is_a_no_op() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    // Indexer restarts and re-processes the same block.
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_150)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::AlreadyRecorded);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000,
        "no double reservation"
    );
}

#[test]
fn glc_deposit_with_no_matching_request_is_never_silently_dropped() {
    let mut ledger = setup();
    let outcome = ledger
        .record_glc_deposit_observed(99999, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::NoMatchingRequest);
    ledger
        .record_unmatched_goldcoin_deposit([0xAA; 32], 0, 100_000, 10, "no_matching_request", 1_100)
        .unwrap();
    let count: i64 = ledger
        .raw()
        .query_row(
            "SELECT count(*) FROM unmatched_goldcoin_deposits",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn glc_deposit_amount_mismatch_routes_to_manual_review_not_silent_accept() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 50_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(
        outcome,
        GlcObservationOutcome::AmountMismatch {
            expected: 100_000,
            observed: 50_000
        }
    );
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    // Reserved capacity is untouched — no release, no advancement toward
    // settlement while under review.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );
}

#[test]
fn pre_finality_reorg_clears_source_binding_and_returns_to_awaiting_deposit() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_reorged(request_id, 1_150).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::AwaitingDeposit);
    assert_eq!(req.source_txid, None);
    // Reservation itself is untouched — still live, just unbound.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000
    );

    // A fresh observation (possibly a different mined block) can re-bind.
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 20, [0xCC; 32], 1_200)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::Recorded);
}

#[test]
#[should_panic(expected = "post-finality")]
fn reorg_after_finality_must_never_be_called_it_is_a_caller_bug() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    // This must panic rather than silently reverting an irreversible claim
    // (docs/10-threat-model.md's post-finality-reorg section).
    let _ = ledger.mark_glc_reorged(request_id, 1_300);
}

// ---------------------------------------------------------------- Solana leg --

#[test]
fn sol_deposit_folds_directly_to_source_finalized_when_capacity_available() {
    let mut ledger = setup();
    let outcome = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::SourceFinalized);
    assert_eq!(req.direction, Direction::SolToGlc);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        800_000
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[test]
fn sol_deposit_beyond_capacity_is_recorded_in_manual_review_never_dropped() {
    let mut ledger = setup();
    // available is 900_000
    let outcome = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    // Capacity untouched — the deposit is real (Solana-side, irreversible)
    // but does not commit reserve capacity it doesn't have.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        900_000
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[test]
fn replaying_the_same_obligation_index_after_restart_is_a_no_op() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(5, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    let outcome2 = ledger
        .fold_sol_deposit(5, amounts(100_000), [1u8; 32], &[2u8; 32], 1_050)
        .unwrap();
    assert_eq!(outcome2, SolFoldOutcome::AlreadyFolded { request_id });
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        800_000,
        "no double reservation"
    );
}

#[test]
fn sol_indexer_progress_cursor_persists() {
    let mut ledger = setup();
    assert_eq!(ledger.last_synced_obligation_count().unwrap(), 0);
    ledger
        .set_last_synced_obligation_count(7, 12345, 1_000)
        .unwrap();
    assert_eq!(ledger.last_synced_obligation_count().unwrap(), 7);
}

#[test]
fn state_log_records_every_transition_in_order() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    let log = ledger.state_log(request_id).unwrap();
    let to_states: Vec<RequestState> = log.iter().map(|(_, to, _, _)| *to).collect();
    assert_eq!(
        to_states,
        vec![
            RequestState::LiquidityReserved,
            RequestState::AwaitingDeposit,
            RequestState::DepositObserved,
            RequestState::Confirming,
            RequestState::SourceFinalized,
        ]
    );
}
