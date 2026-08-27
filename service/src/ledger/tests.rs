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
fn available_vault_utxos_excludes_a_utxo_backing_a_not_yet_finalized_glc_to_sol_deposit() {
    // Regression: a concurrent SolToGlc payout's coin selection could pick
    // the vault UTXO backing a GlcToSol deposit before that deposit
    // reached SourceFinalized, permanently stranding the GlcToSol request
    // in Confirming once its own backing output turned up already spent.
    // Prevention: available_vault_utxos must never offer such a UTXO as a
    // payout candidate in the first place.
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
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Confirming
    );

    // Two vault UTXOs are now visible on-chain: the one backing the
    // still-Confirming GlcToSol deposit above, and an unrelated one (e.g.
    // vault change from an earlier settlement) with nothing pending
    // against it.
    let backing_deposit = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAA; 32],
        vout: 0,
        amount_atomic: 100_000,
        script_pubkey_hex: "51".to_string(),
    };
    let unrelated = crate::goldcoin::coin::VaultUtxo {
        txid: [0xCC; 32],
        vout: 1,
        amount_atomic: 250_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(
            &[
                (backing_deposit.clone(), 6, "51".to_string()),
                (unrelated.clone(), 6, "51".to_string()),
            ],
            1,
            1_150,
        )
        .unwrap();

    let available = ledger.available_vault_utxos().unwrap();
    assert!(
        !available
            .iter()
            .any(|u| u.txid == backing_deposit.txid && u.vout == backing_deposit.vout),
        "must exclude the UTXO backing a not-yet-SourceFinalized GlcToSol deposit: {available:?}"
    );
    assert!(
        available
            .iter()
            .any(|u| u.txid == unrelated.txid && u.vout == unrelated.vout),
        "must still offer an unrelated, unencumbered UTXO: {available:?}"
    );

    // Once the deposit reaches SourceFinalized, its UTXO becomes a
    // legitimate payout candidate again (nothing left to strand).
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();
    let available_after = ledger.available_vault_utxos().unwrap();
    assert!(
        available_after
            .iter()
            .any(|u| u.txid == backing_deposit.txid && u.vout == backing_deposit.vout),
        "must offer the UTXO once its backing deposit is SourceFinalized: {available_after:?}"
    );
}

#[test]
fn immature_vault_utxo_total_sums_only_unconfirmed_utxos() {
    let mut ledger = setup();

    let mature = crate::goldcoin::coin::VaultUtxo {
        txid: [0xAAu8; 32],
        vout: 0,
        amount_atomic: 250_000,
        script_pubkey_hex: "51".to_string(),
    };
    let immature = crate::goldcoin::coin::VaultUtxo {
        txid: [0xBBu8; 32],
        vout: 0,
        amount_atomic: 9_010_000,
        script_pubkey_hex: "51".to_string(),
    };

    // Only 9 confirmations against a required minimum of 20 — mirrors the
    // production incident's large, still-maturing change output.
    ledger
        .sync_vault_utxos(
            &[
                (mature.clone(), 20, "51".to_string()),
                (immature.clone(), 9, "51".to_string()),
            ],
            20,
            1_000,
        )
        .unwrap();

    assert_eq!(ledger.immature_vault_utxo_total().unwrap(), 9_010_000);

    // The mature UTXO is a normal payout candidate; the immature one is
    // invisible to coin selection until it matures.
    let available = ledger.available_vault_utxos().unwrap();
    assert!(available.iter().any(|u| u.txid == mature.txid));
    assert!(!available.iter().any(|u| u.txid == immature.txid));

    // Once it matures, it stops counting as immature and becomes a normal
    // candidate.
    ledger
        .sync_vault_utxos(
            &[
                (mature.clone(), 21, "51".to_string()),
                (immature.clone(), 21, "51".to_string()),
            ],
            20,
            1_100,
        )
        .unwrap();
    assert_eq!(ledger.immature_vault_utxo_total().unwrap(), 0);
    let available_after = ledger.available_vault_utxos().unwrap();
    assert!(available_after.iter().any(|u| u.txid == immature.txid));
}

#[test]
fn available_vault_utxos_excludes_a_utxo_already_reserved_for_another_payout() {
    // A UTXO `reserve_vault_utxos` has already claimed for one SolToGlc
    // payout must never be offered to coin selection for a second, distinct
    // payout — the reservation itself (not merely `state != 'Spent'`) is
    // what coin selection must respect, since the reserving payout has not
    // broadcast (or even necessarily been signed) yet.
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };

    let utxo = crate::goldcoin::coin::VaultUtxo {
        txid: [0xEEu8; 32],
        vout: 0,
        amount_atomic: 500_000,
        script_pubkey_hex: "51".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(utxo.clone(), 20, "51".to_string())], 1, 1_000)
        .unwrap();
    assert!(
        ledger
            .available_vault_utxos()
            .unwrap()
            .iter()
            .any(|u| u.txid == utxo.txid),
        "must be a normal candidate before anything reserves it"
    );

    ledger
        .reserve_vault_utxos(request_id, std::slice::from_ref(&utxo), 1_100)
        .unwrap();

    let available = ledger.available_vault_utxos().unwrap();
    assert!(
        !available.iter().any(|u| u.txid == utxo.txid),
        "a UTXO already reserved by another in-flight payout must never be offered again: {available:?}"
    );
}

#[test]
fn reserve_vault_utxos_is_safe_under_genuine_concurrent_writers() {
    // Real OS threads, each with its own connection to the SAME file-backed
    // ledger, racing to reserve the SAME single vault UTXO for two different
    // payout requests — not the sequential-call "concurrency" stand-in used
    // elsewhere in this crate's test suite (module docs on those tests
    // explain why sequential calls are an adequate substitute for capacity
    // accounting; the outpoint-level reservation guard below is exactly the
    // mechanism that substitution would fail to exercise). A busy timeout is
    // set on each connection so SQLite's writer serialization produces a
    // deterministic winner rather than a flaky `SQLITE_BUSY` on either side.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let (request_a, request_b, utxo) = {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                1_000_000,
                0,
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
                0,
                500_000,
                200_000,
                150_000,
                0,
            )
            .unwrap();
        let SolFoldOutcome::FoldedFinalized { request_id: a } = ledger
            .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 0)
            .unwrap()
        else {
            panic!()
        };
        let SolFoldOutcome::FoldedFinalized { request_id: b } = ledger
            .fold_sol_deposit(1, amounts(100_000), [3u8; 32], &[4u8; 32], 0)
            .unwrap()
        else {
            panic!()
        };
        let utxo = crate::goldcoin::coin::VaultUtxo {
            txid: [0xFFu8; 32],
            vout: 0,
            amount_atomic: 500_000,
            script_pubkey_hex: "51".to_string(),
        };
        ledger
            .sync_vault_utxos(&[(utxo.clone(), 20, "51".to_string())], 1, 0)
            .unwrap();
        (a, b, utxo)
    };

    let run = |request_id: i64| {
        let path = path.clone();
        let utxo = utxo.clone();
        std::thread::spawn(move || {
            let mut ledger = Ledger::open(&path).unwrap();
            ledger
                .raw()
                .busy_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            ledger.reserve_vault_utxos(request_id, &[utxo], 10)
        })
    };
    let ta = run(request_a);
    let tb = run(request_b);
    let ra = ta.join().unwrap();
    let rb = tb.join().unwrap();

    let outcomes = [ra.is_ok(), rb.is_ok()];
    assert_eq!(
        outcomes.iter().filter(|ok| **ok).count(),
        1,
        "exactly one of the two concurrent reservations for the same UTXO must win: {ra:?} / {rb:?}"
    );

    let ledger = Ledger::open(&path).unwrap();
    assert!(
        ledger.available_vault_utxos().unwrap().is_empty(),
        "the contested UTXO must be Reserved, not offered to a third payout"
    );
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
fn late_glc_deposit_after_expiry_auto_recreates_when_capacity_available() {
    // docs/04-state-machines.md "Open design item: late deposits after
    // expiry": a deposit that arrives after the reservation TTL elapsed
    // must not be treated the same as an uncorrelated payment when
    // capacity is still available — it should re-reserve and continue.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[1u8; 32],
            None,
            10,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(ledger.expire_reservations(1_020).unwrap(), 1);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        900_000,
        "expiry must have released the reservation"
    );

    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::LateDepositRecreated);

    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        RequestState::Confirming,
        "late deposit continues the flow normally from DepositObserved, same as an on-time one"
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000,
        "capacity must be re-reserved, not double-counted or left released"
    );

    let log = ledger.state_log(request_id).unwrap();
    let transitions: Vec<(Option<RequestState>, RequestState)> =
        log.iter().map(|e| (e.0, e.1)).collect();
    assert!(transitions.contains(&(Some(RequestState::Expired), RequestState::LiquidityReserved)));
    assert!(transitions.contains(&(
        Some(RequestState::LiquidityReserved),
        RequestState::AwaitingDeposit
    )));
    assert!(transitions.contains(&(
        Some(RequestState::AwaitingDeposit),
        RequestState::DepositObserved
    )));

    // Idempotent on replay, same as an on-time deposit.
    let replay = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_150)
        .unwrap();
    assert_eq!(replay, GlcObservationOutcome::AlreadyRecorded);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        800_000,
        "replay must not re-reserve capacity a second time"
    );
}

#[test]
fn late_glc_deposit_after_expiry_routes_to_manual_review_when_no_capacity() {
    // Same design item, other branch: if capacity is no longer available to
    // re-reserve, the real (irreversible) deposit must route to
    // ManualReview rather than being silently recorded as unmatched.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved {
        request_id: stale_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(900_000),
            &[1u8; 32],
            None,
            10,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(ledger.expire_reservations(1_020).unwrap(), 1);

    // A different request now consumes all the capacity the stale
    // reservation released.
    let CreateRequestOutcome::Reserved {
        request_id: other_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(900_000),
            &[2u8; 32],
            None,
            3600,
            1_020,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        0
    );

    let outcome = ledger
        .record_glc_deposit_observed(stale_id, [0xAA; 32], 0, 900_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::LateDepositNoCapacity);

    let req = ledger.get_request(stale_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    assert!(req.manual_review_note.as_deref() == Some("late_deposit_no_capacity"));

    // The other, unrelated request is untouched.
    let other = ledger.get_request(other_id).unwrap().unwrap();
    assert_eq!(other.state, RequestState::AwaitingDeposit);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        0,
        "no capacity was fabricated for the stale request"
    );
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

// ------------------------------------------------------- admission control --
//
// `admission_closed` (docs/09-runbook.md "Admission control
// (Solana->Goldcoin)") is a separate axis from `paused` — see
// `Ledger::set_admission`/`is_admission_closed`. It is checked ONLY by
// `fold_sol_deposit`'s capacity_ok computation; nothing else in this crate
// reads it, and nothing in this crate ever sets it automatically.

#[test]
fn closed_admission_routes_a_new_deposit_to_manual_review_even_with_capacity_and_no_pause() {
    let mut ledger = setup();
    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("operator note"),
        )
        .unwrap();
    // Proves the two flags are genuinely independent: pause is untouched
    // (still false), and there is ample capacity — admission_closed alone
    // must still be what blocks this.
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    let outcome = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("expected admission-closed to route to ManualReview, got {outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::ManualReview);
    assert_eq!(
        req.manual_review_note.as_deref(),
        Some("admission_closed_at_fold")
    );
    // The deposit is real and irreversible on Solana, but must not commit
    // reserve capacity it was never granted.
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        900_000
    );
}

#[test]
fn admission_is_open_by_default_and_folding_is_unaffected() {
    let ledger_open = setup();
    assert!(!ledger_open
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

#[test]
fn pause_still_blocks_admission_independent_of_the_new_admission_flag() {
    let mut ledger = setup();
    // Existing pause logic, unchanged: admission stays open (the new
    // flag), but the pre-existing `paused` gate alone must still be
    // enough to route a new deposit to ManualReview, exactly as before
    // this feature existed.
    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("reconciliation breach"),
        )
        .unwrap();
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    let outcome = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("expected paused to still route to ManualReview, got {outcome:?}")
    };
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.manual_review_note.as_deref(),
        Some("reserve_paused_at_fold")
    );
}

#[test]
fn closing_admission_never_touches_an_already_accepted_request() {
    let mut ledger = setup();
    // Accept a request BEFORE admission is ever closed.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    let before = ledger.get_request(request_id).unwrap().unwrap();
    let capacity_before = ledger
        .available_capacity(ReserveDirection::GoldcoinReserve)
        .unwrap();

    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("closing admission"),
        )
        .unwrap();

    // The already-accepted request and its committed capacity are
    // completely unaffected — closing admission only ever gates NEW folds.
    let after = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(after.state, before.state);
    assert_eq!(after.net_destination_atomic, before.net_destination_atomic);
    assert_eq!(after.state, RequestState::SourceFinalized);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        capacity_before
    );
}

#[test]
fn set_admission_never_reopens_automatically() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    // Nothing else in this crate ever calls `set_admission` — closing it
    // once must leave it closed indefinitely, with no code path that
    // implicitly reopens it. Simulate the passage of time/other ledger
    // activity and confirm it's still closed.
    ledger
        .fold_sol_deposit(9, amounts(1), [3u8; 32], &[4u8; 32], 2_000)
        .unwrap();
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

#[test]
fn check_invariant_fails_on_a_genuine_breach_the_same_check_open_admission_relies_on() {
    // `glc-admin open-admission` refuses unless `Ledger::check_invariant`
    // holds — this proves that check itself actually fails closed on a
    // real breach (balance below protected_minimum + reserved_liquidity),
    // not just that it passes on a healthy fixture.
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    // reserved_liquidity is now 100_000 against protected_minimum 100_000
    // -> the invariant requires balance >= 200_000. Drop the observed
    // balance below that via a live reconciliation-style refresh.
    ledger
        .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 150_000, 2_000)
        .unwrap();
    let err = ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap_err();
    assert!(matches!(err, LedgerError::InvariantViolated { .. }));
}

// ---------------------------------------------------- resume manual review --

#[test]
fn resumes_a_request_parked_by_admission_closed_and_reserves_capacity() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!("expected admission-closed to route to ManualReview")
    };
    let before = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(before.state, RequestState::ManualReview);
    assert_eq!(
        before.manual_review_note.as_deref(),
        Some("admission_closed_at_fold")
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        900_000,
        "a parked request must not have committed capacity"
    );

    // Admission may remain CLOSED — resuming never touches it.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "operator resuming after incident",
            "operator",
            2_000,
        )
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    let after = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(after.id, before.id, "same request id preserved");
    assert_eq!(after.state, RequestState::SourceFinalized);
    assert!(after.manual_review_note.is_none());
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        800_000,
        "resuming must reserve capacity, exactly as a successful fold would have"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .unwrap();
}

#[test]
fn resumes_a_request_parked_by_pause_even_while_still_paused() {
    let mut ledger = setup();
    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("reconciliation breach"),
        )
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("reserve_paused_at_fold")
    );

    // Resuming does not require unpausing first — processing has never
    // been gated by `paused` either.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "resuming while still paused",
            "operator",
            2_000,
        )
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
    assert!(ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

#[test]
fn resumes_a_request_parked_by_insufficient_capacity_once_capacity_recovers() {
    let mut ledger = setup();
    // available is 900_000 -> this exceeds it.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("insufficient_capacity_at_fold")
    );

    // Still insufficient -> refused.
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "trying too early", "operator", 1_500)
        .unwrap_err();
    assert!(matches!(err, LedgerError::InvariantViolated { .. }));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a refused resume attempt must not mutate the request"
    );

    // Capacity recovers (e.g. a rebalance deposit).
    ledger
        .refresh_reserve_balance(ReserveDirection::GoldcoinReserve, 2_000_000, 1_800)
        .unwrap();
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "capacity has recovered", "operator", 2_000)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn resume_is_idempotent_and_never_double_reserves() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    ledger
        .resume_manual_review_sol_to_glc(request_id, "first resume", "operator", 2_000)
        .unwrap();
    let capacity_after_first = ledger
        .available_capacity(ReserveDirection::GoldcoinReserve)
        .unwrap();

    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "second resume attempt", "operator", 3_000)
        .unwrap();
    assert_eq!(
        outcome,
        ResumeManualReviewOutcome::AlreadyResumed {
            state: RequestState::SourceFinalized
        }
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        capacity_after_first,
        "a repeat call must never reserve capacity a second time"
    );
}

#[test]
fn refuses_a_request_that_reached_source_finalized_without_ever_being_in_manual_review() {
    let mut ledger = setup();
    // Capacity is available and admission is open -> folds directly to
    // SourceFinalized, never touching ManualReview at all.
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "mistaken call", "operator", 2_000)
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::ManualReviewNotRecoverable { .. }),
        "a request never in ManualReview must be refused, not reported as already-resumed: {err}"
    );
}

#[test]
fn refuses_a_glc_to_sol_request() {
    let mut ledger = setup();
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(100_000),
            &[2u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = outcome else {
        panic!("{outcome:?}")
    };
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "wrong direction", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::NotASolToGlcRequest { id, .. } if id == request_id
    ));
}

#[test]
fn refuses_an_unknown_manual_review_reason() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    // Simulate a ManualReview row parked for some unrelated reason (e.g. a
    // future code path this command was never meant to touch).
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'some_future_unrelated_reason' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "should be refused", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::ManualReviewNotRecoverable { .. }
    ));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
}

#[test]
fn refuses_a_request_that_already_has_a_goldcoin_payout() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    // A payout row existing at all for a ManualReview request should never
    // happen in practice, but this command must fail closed rather than
    // assume it can't.
    ledger
        .conn
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at)
             VALUES (?1, X'ab', 1, 0, 0, X'cd', 'Built', 1000)",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "should be refused", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::ManualReviewNotRecoverable { .. }
    ));
}

#[test]
fn refuses_an_unknown_request_id() {
    let mut ledger = setup();
    let err = ledger
        .resume_manual_review_sol_to_glc(999_999, "does not exist", "operator", 2_000)
        .unwrap_err();
    assert!(matches!(err, LedgerError::RequestNotFound(id) if id == 999_999));
}

#[test]
fn resume_writes_the_operator_note_to_the_audit_trail() {
    let mut ledger = setup();
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!()
    };
    ledger
        .resume_manual_review_sol_to_glc(
            request_id,
            "verified with ops, safe to resume",
            "operator",
            2_000,
        )
        .unwrap();
    let log = ledger.state_log(request_id).unwrap();
    let resumed_entry = log
        .iter()
        .find(|(from, to, _, _)| {
            *from == Some(RequestState::ManualReview) && *to == RequestState::SourceFinalized
        })
        .expect("expected a ManualReview -> SourceFinalized log entry");
    assert_eq!(
        resumed_entry.3.as_deref(),
        Some("verified with ops, safe to resume")
    );
}

// ---- SolToGlc recipient rate limit (docs/09-runbook.md) ----

/// Directly sets a request's `state`, bypassing every ledger safety check —
/// legitimate ONLY in tests, to reach states
/// (`DestinationSubmitted`/`Settled`/etc.) the public API has no single-call
/// path to for a bare `SolToGlc` fold without also standing up the full
/// Goldcoin payout-signing pipeline. `tests` is a descendant module of
/// `ledger`, so it may access the private `conn` field directly.
fn force_state(ledger: &mut Ledger, request_id: i64, state: RequestState) {
    ledger
        .conn
        .execute(
            "UPDATE bridge_requests SET state = ?1 WHERE id = ?2",
            rusqlite::params![state, request_id],
        )
        .unwrap();
}

#[test]
fn second_deposit_to_the_same_recipient_inside_24h_is_parked_recipient_rate_limited() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!("first deposit to a fresh recipient must fold straight through")
    };

    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 3_600)
        .unwrap()
    else {
        panic!("a second deposit to the SAME recipient inside the window must be parked")
    };
    let parked = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        parked.manual_review_note.as_deref(),
        Some("recipient_rate_limited")
    );
    assert_eq!(
        parked.state,
        RequestState::ManualReview,
        "rate-limited fold must never reserve capacity"
    );
}

#[test]
fn deposit_to_the_same_recipient_after_the_window_ages_out_is_accepted_normally() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap();

    // created_at(1_000) + 86_400 == 87_400: the window has fully elapsed by
    // this exact instant (strictly-greater-than in the query), so this must
    // fold straight through, not park.
    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 87_400)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "expected a normal fold once the 24h window has aged out, got {outcome:?}"
    );
}

#[test]
fn different_recipients_are_completely_independent() {
    let mut ledger = setup();
    let outcome_a = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &[1u8; 32], 1_000)
        .unwrap();
    let outcome_b = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &[2u8; 32], 1_000)
        .unwrap();
    assert!(matches!(outcome_a, SolFoldOutcome::FoldedFinalized { .. }));
    assert!(matches!(outcome_b, SolFoldOutcome::FoldedFinalized { .. }));
}

#[test]
fn replaying_the_same_obligation_after_restart_is_not_treated_as_rate_limited() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // Simulated restart: the exact same obligation is observed again. The
    // pre-existing `source_obligation_index` idempotency check must win
    // BEFORE the rate-limit check ever runs — this must never be
    // reinterpreted as "this recipient hit its own limit."
    let outcome2 = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 2_000)
        .unwrap();
    assert_eq!(outcome2, SolFoldOutcome::AlreadyFolded { request_id });
}

#[test]
fn an_in_flight_manual_review_obligation_still_counts_against_its_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // Parked for an UNRELATED reason (insufficient capacity), never
    // resumed — still a live obligation that can result in a payout, so it
    // must still count against this recipient.
    let SolFoldOutcome::FoldedManualReview { .. } = ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };

    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 10)
        .unwrap()
    else {
        panic!("a second obligation to a recipient with a live ManualReview obligation must also be parked")
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("recipient_rate_limited")
    );
}

#[test]
fn a_settled_obligation_still_counts_against_its_recipient_until_the_window_elapses() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::Settled);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 10)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }),
        "a fully Settled payout to this recipient is still inside the window \
         and must still count, got {outcome:?}"
    );
}

#[test]
fn a_destination_submitted_obligation_counts_against_its_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::DestinationSubmitted);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 10)
        .unwrap();
    assert!(matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }));
}

#[test]
fn a_cancelled_or_failed_obligation_never_counts_against_its_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };
    // `Failed` is defined but never set anywhere in production code today
    // (docs/09-runbook.md) — forced directly here purely to exercise the
    // exclude-list, defensively, in case that ever changes.
    force_state(&mut ledger, request_id, RequestState::Failed);

    let outcome = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 10)
        .unwrap();
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedFinalized { .. }),
        "a Failed request must never count against its recipient, got {outcome:?}"
    );
}

#[test]
fn manual_resume_refuses_while_the_recipient_is_still_inside_the_window() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // First obligation: settles the window.
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap();
    // Second obligation to the same recipient: parked recipient_rate_limited.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };

    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "trying too early", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::RecipientRateLimited { request_id: rid, .. } if rid == request_id
    ));
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::ManualReview,
        "a refused resume attempt must not mutate the request"
    );

    // Once the FIRST request's window has aged out, the resume succeeds
    // normally — self-clearing, no operator override needed.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "window has elapsed", "operator", 87_401)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn manual_resume_checks_the_window_unconditionally_even_when_parked_for_a_different_reason() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // A live obligation to this recipient, still within its own window.
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap();

    // A second obligation to the SAME recipient, but parked for a
    // completely different, unrelated reason (admission closed).
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 10)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("admission_closed_at_fold")
    );

    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // Admission is open again, but the recipient's window (from the FIRST
    // request) has not elapsed yet — the resume must still be refused,
    // proving the window check is unconditional, not gated on this
    // request's own `manual_review_note`.
    let err = ledger
        .resume_manual_review_sol_to_glc(request_id, "admission reopened", "operator", 1_000 + 20)
        .unwrap_err();
    assert!(matches!(err, LedgerError::RecipientRateLimited { .. }));
}

#[test]
fn manual_resume_self_excludes_so_a_request_never_blocks_its_own_resume() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("closing"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!("parked for admission_closed, not rate limiting")
    };
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, false, Some("reopening"))
        .unwrap();
    // With no OTHER request to this recipient, the rate-limit re-check must
    // never treat this request's own row as a blocker of itself.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(request_id, "admission reopened", "operator", 1_500)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn glc_to_sol_is_completely_unaffected_by_the_recipient_rate_limit() {
    let mut ledger = setup();
    // Two GlcToSol requests to the same Solana recipient, back to back,
    // well inside what would be a 24h window for SolToGlc — the rate limit
    // is SolToGlc-only and must never touch `create_request`.
    let outcome_a = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[7u8; 32],
            None,
            3_600,
            1_000,
        )
        .unwrap();
    let outcome_b = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[7u8; 32],
            None,
            3_600,
            1_010,
        )
        .unwrap();
    let (
        CreateRequestOutcome::Reserved { request_id: id_a },
        CreateRequestOutcome::Reserved { request_id: id_b },
    ) = (outcome_a, outcome_b)
    else {
        panic!("both GlcToSol requests to the same recipient must be reserved normally, unaffected by the SolToGlc-only rate limit")
    };
    assert_ne!(id_a, id_b);
}

/// Configures a `setup()`-equivalent reserve on a file-backed `Ledger` at
/// `path` — needed wherever a test must simulate a restart (`setup()`
/// itself is in-memory and cannot survive being dropped and reopened).
fn setup_at(path: &std::path::Path) -> Ledger {
    let mut ledger = Ledger::open(path).unwrap();
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

/// Regression coverage for a real HIGH-severity finding: the resume-time
/// rate-limit check originally considered ANY other qualifying row to the
/// recipient as a potential blocker — including ones created AFTER the
/// candidate being resumed. For a recipient with 3+ queued rows, this let
/// a later-arriving (and itself still-parked) sibling shadow-block an
/// earlier, rightfully-next-in-line candidate, inverting oldest-first
/// draining. Fixed by restricting the blocker search to strict
/// predecessors — rows ordered `(created_at, id)` before the candidate's
/// own. These four tests exercise that fix directly.
///
/// Sets up A (accepted, anchors the window), B and C (parked, both
/// blocked at fold time). Returns their ids in creation order.
fn setup_three_requests_same_recipient(
    ledger: &mut Ledger,
    recipient: [u8; 32],
) -> (i64, i64, i64) {
    let SolFoldOutcome::FoldedFinalized { request_id: a } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!("A must fold straight through to establish the window")
    };
    let SolFoldOutcome::FoldedManualReview { request_id: b } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_050)
        .unwrap()
    else {
        panic!("B must park, blocked by A")
    };
    let SolFoldOutcome::FoldedManualReview { request_id: c } = ledger
        .fold_sol_deposit(2, amounts(50_000), [3u8; 32], &recipient, 1_100)
        .unwrap()
    else {
        panic!("C must park too")
    };
    (a, b, c)
}

#[test]
fn oldest_first_ordering_holds_for_three_queued_requests_to_the_same_recipient() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let (_a, b, c) = setup_three_requests_same_recipient(&mut ledger, recipient);

    // now = 87_401: just past A's window (1_000 + 86_400 = 87_400).
    // B's only possible blocker is A (C is a later sibling and must be
    // structurally ineligible to block B at all) — B must resume.
    let now = 87_401;
    let b_outcome = ledger
        .resume_manual_review_sol_to_glc(b, "b turn", "operator", now)
        .unwrap();
    assert_eq!(
        b_outcome,
        ResumeManualReviewOutcome::Resumed,
        "B must resume once A's window clears"
    );

    // C's only possible blocker is B. B's own window (1_050 + 86_400 =
    // 87_450) has not elapsed yet at now = 87_401, so C must still be
    // refused — even though B itself JUST resumed in this same instant.
    let c_err = ledger
        .resume_manual_review_sol_to_glc(c, "c too early", "operator", now)
        .unwrap_err();
    assert!(
        matches!(c_err, LedgerError::RecipientRateLimited { .. }),
        "C must remain blocked by B until B's OWN window elapses, got {c_err:?}"
    );
    assert_eq!(
        ledger.get_request(c).unwrap().unwrap().state,
        RequestState::ManualReview
    );
}

#[test]
fn c_remains_blocked_until_bs_own_24h_window_expires_then_resumes() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let (_a, b, c) = setup_three_requests_same_recipient(&mut ledger, recipient);

    ledger
        .resume_manual_review_sol_to_glc(b, "b turn", "operator", 87_401)
        .unwrap();

    // Still inside B's window (1_050 + 86_400 = 87_450 is the exact
    // instant it elapses — one second before, it must still block).
    let err = ledger
        .resume_manual_review_sol_to_glc(c, "still too early", "operator", 87_449)
        .unwrap_err();
    assert!(matches!(err, LedgerError::RecipientRateLimited { .. }));

    // B's window has now fully elapsed (strictly-greater-than semantics:
    // at exactly created_at + 86_400 the window has already elapsed, same
    // boundary convention as every other rate-limit check in this file).
    let outcome = ledger
        .resume_manual_review_sol_to_glc(c, "b's window elapsed", "operator", 87_450)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
}

#[test]
fn continuous_newer_arrivals_can_never_starve_the_oldest_parked_request() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };
    let SolFoldOutcome::FoldedManualReview {
        request_id: oldest_parked,
    } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_010)
        .unwrap()
    else {
        panic!()
    };

    // A steady trickle of NEW obligations to the SAME recipient, each
    // arriving shortly after the last — every one of them necessarily
    // parks too (the recipient is still within its own rolling window at
    // each arrival), continuously "renewing" admission-time rate limiting
    // for brand-new deposits. None of this may ever affect
    // `oldest_parked`'s own eligibility.
    for i in 2..30u64 {
        let outcome = ledger
            .fold_sol_deposit(
                i,
                amounts(50_000),
                [3u8; 32],
                &recipient,
                1_010 + (i as i64) * 10,
            )
            .unwrap();
        assert!(
            matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }),
            "obligation {i}: expected a park (still inside the rolling window), got {outcome:?}"
        );
    }

    // `oldest_parked`'s only possible blocker is the very first
    // (accepted) request at created_at=1_000 — none of the 28 later
    // arrivals above may count, no matter how many piled up behind it.
    let outcome = ledger
        .resume_manual_review_sol_to_glc(
            oldest_parked,
            "unblocked by predecessor alone",
            "operator",
            87_401,
        )
        .unwrap();
    assert_eq!(
        outcome,
        ResumeManualReviewOutcome::Resumed,
        "a flood of newer same-recipient arrivals must never starve the oldest parked request"
    );
}

#[test]
fn restart_preserves_oldest_first_ordering_for_the_same_recipient() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let recipient = [9u8; 32];
    let (b, c) = {
        let mut ledger = setup_at(&path);
        let (_a, b, c) = setup_three_requests_same_recipient(&mut ledger, recipient);
        ledger
            .resume_manual_review_sol_to_glc(b, "b turn", "operator", 87_401)
            .unwrap();
        (b, c)
    };
    // Simulated restart: a brand-new `Ledger` handle over the same
    // on-disk database.
    let mut restarted = Ledger::open(&path).unwrap();
    assert_eq!(
        restarted.get_request(b).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "B's resume must have survived the restart"
    );

    // C must still be exactly as blocked by B (created_at=1_050) as it
    // was before the restart — ordering is a pure function of persisted
    // `created_at`/`id` values, not in-memory state.
    let err = restarted
        .resume_manual_review_sol_to_glc(c, "too early, post-restart", "operator", 87_449)
        .unwrap_err();
    assert!(matches!(err, LedgerError::RecipientRateLimited { .. }));

    let outcome = restarted
        .resume_manual_review_sol_to_glc(c, "b's window elapsed, post-restart", "operator", 87_450)
        .unwrap();
    assert_eq!(outcome, ResumeManualReviewOutcome::Resumed);
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

// The read-only view (`sol_to_glc_recipient_rate_limited_until`) the API's
// eligibility endpoint serves: it must answer exactly what
// `fold_sol_deposit` would decide for the next obligation naming these
// bytes — same shared query, so these tests pin the pairing from the
// read side.

#[test]
fn eligibility_view_reports_an_unused_recipient_as_not_rate_limited() {
    let ledger = setup();
    assert_eq!(
        ledger
            .sol_to_glc_recipient_rate_limited_until(&[9u8; 32], 1_000)
            .unwrap(),
        None
    );
}

#[test]
fn eligibility_view_reports_a_recently_paid_recipient_with_the_exact_reopen_time() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .sol_to_glc_recipient_rate_limited_until(&recipient, 1_000 + 3_600)
            .unwrap(),
        Some(1_000 + 86_400),
        "retry_after must be the blocking fold's created_at plus the 24h window"
    );
    // And a fold attempted now really would be parked — the view and the
    // authoritative admission check must agree.
    let SolFoldOutcome::FoldedManualReview { .. } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 3_600)
        .unwrap()
    else {
        panic!("fold must park exactly when the view says rate-limited")
    };
}

#[test]
fn eligibility_view_clears_once_the_24h_window_has_elapsed() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap();
    // One second before the boundary: still blocked (`created_at > now -
    // window` — strictly-inside comparison).
    assert!(ledger
        .sol_to_glc_recipient_rate_limited_until(&recipient, 1_000 + 86_399)
        .unwrap()
        .is_some());
    // At exactly `created_at + window` — the very `retry_after` instant
    // reported above — the row no longer qualifies: retry_after is the
    // FIRST eligible second, not the last blocked one.
    assert_eq!(
        ledger
            .sol_to_glc_recipient_rate_limited_until(&recipient, 1_000 + 86_400)
            .unwrap(),
        None
    );
    // And the authoritative fold agrees: accepted normally.
    let SolFoldOutcome::FoldedFinalized { .. } = ledger
        .fold_sol_deposit(1, amounts(50_000), [2u8; 32], &recipient, 1_000 + 86_400)
        .unwrap()
    else {
        panic!("fold must admit exactly when the view says eligible")
    };
}

#[test]
fn eligibility_view_is_per_recipient_a_different_address_is_unaffected() {
    let mut ledger = setup();
    ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &[9u8; 32], 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .sol_to_glc_recipient_rate_limited_until(&[10u8; 32], 1_000 + 10)
            .unwrap(),
        None,
        "another recipient's payout must never rate-limit this one"
    );
}

#[test]
fn eligibility_view_counts_a_parked_manual_review_obligation_like_fold_does() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    // Oversized -> parked ManualReview, never paid — but it still counts
    // against the recipient, exactly as fold_sol_deposit counts it.
    ledger
        .fold_sol_deposit(0, amounts(950_000), [1u8; 32], &recipient, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .sol_to_glc_recipient_rate_limited_until(&recipient, 1_000 + 10)
            .unwrap(),
        Some(1_000 + 86_400)
    );
}

#[test]
fn eligibility_view_ignores_terminal_never_paid_states_like_fold_does() {
    let mut ledger = setup();
    let recipient = [9u8; 32];
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(50_000), [1u8; 32], &recipient, 1_000)
        .unwrap()
    else {
        panic!()
    };
    force_state(&mut ledger, request_id, RequestState::Failed);
    assert_eq!(
        ledger
            .sol_to_glc_recipient_rate_limited_until(&recipient, 1_000 + 10)
            .unwrap(),
        None,
        "a Failed request produced no payout and must not block the recipient"
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

// -------------------------------------------------------------- rebalancing --

#[test]
fn rebalance_full_lifecycle_deposit_increases_balance_only_after_confirmed() {
    let mut ledger = setup();
    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;

    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            50_000,
            "quarterly top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    let rb = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rb.state, RebalanceState::Proposed);
    assert_eq!(rb.amount_atomic, 50_000);

    // Below threshold: still Proposed.
    let outcome = ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    assert_eq!(
        outcome,
        RebalanceApprovalOutcome::Recorded {
            approvals: 1,
            required: 2
        }
    );
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Proposed
    );

    // Threshold reached: Approved.
    let outcome = ledger.approve_rebalance(id, "ops-bob", 1_002).unwrap();
    assert_eq!(outcome, RebalanceApprovalOutcome::ThresholdReached);
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Approved
    );

    // Balance untouched by proposal/approval alone.
    let mid = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(
        mid, before,
        "no balance change before execution+confirmation"
    );

    ledger
        .record_rebalance_executed(id, "solana-sig-abc123", "ops-alice", 1_003)
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Executed
    );
    // Still untouched: executed only records evidence, not the effect.
    let still_mid = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(still_mid, before);

    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 1_004)
        .unwrap();
    let rb = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(rb.state, RebalanceState::Confirmed);
    assert_eq!(rb.observed_amount_atomic, Some(50_000));

    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(
        after,
        before + 50_000,
        "a confirmed Deposit increases total_reserve_balance"
    );
}

#[test]
fn rebalance_withdraw_decreases_balance_only_after_confirmed() {
    let mut ledger = setup();
    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Withdraw,
            10_000,
            "sweep surplus to cold storage",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "solana-sig-withdraw-1", "ops-alice", 1_002)
        .unwrap();
    ledger
        .confirm_rebalance(id, 10_000, "ops-alice", 1_003)
        .unwrap();
    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(after, before - 10_000);
}

#[test]
fn rebalance_never_touches_reserved_liquidity_pending_obligations_or_bridge_requests() {
    let mut ledger = setup();
    let (_, _, reserved_before, pending_before) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    let requests_before = ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap()
        .len();

    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            50_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "solana-sig-structural", "ops-alice", 1_002)
        .unwrap();
    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 1_003)
        .unwrap();

    let (_, _, reserved_after, pending_after) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(reserved_before, reserved_after);
    assert_eq!(pending_before, pending_after);
    let requests_after = ledger
        .requests_by_state(Direction::GlcToSol, RequestState::AwaitingDeposit)
        .unwrap()
        .len();
    assert_eq!(requests_before, requests_after);
}

#[test]
fn rebalance_approving_twice_from_the_same_identity_does_not_double_count() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            50_000,
            "top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    let o1 = ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    let o2 = ledger.approve_rebalance(id, "ops-alice", 1_002).unwrap();
    assert_eq!(
        o1, o2,
        "the same approver approving twice must not move the count"
    );
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Proposed,
        "still short of the real second distinct approver"
    );
}

#[test]
fn rebalance_duplicate_tx_reference_is_rejected_structurally() {
    let mut ledger = setup();
    let id1 = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up 1",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id1, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id1, "solana-sig-replay-target", "ops-alice", 1_002)
        .unwrap();

    let id2 = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            20_000,
            "top-up 2",
            "ops-alice",
            1,
            1_003,
        )
        .unwrap();
    ledger.approve_rebalance(id2, "ops-alice", 1_004).unwrap();
    let result =
        ledger.record_rebalance_executed(id2, "solana-sig-replay-target", "ops-alice", 1_005);
    assert!(
        result.is_err(),
        "the same real tx_reference must never be recorded against two rebalance requests"
    );
    // The first request is untouched by the rejected second attempt.
    assert_eq!(
        ledger.get_rebalance(id1).unwrap().unwrap().state,
        RebalanceState::Executed
    );
    assert_eq!(
        ledger.get_rebalance(id2).unwrap().unwrap().state,
        RebalanceState::Approved,
        "the second request must not be left partially executed by the rejected attempt"
    );
}

#[test]
fn rebalance_wrong_state_transitions_are_rejected() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();

    // Cannot execute before Approved.
    let result = ledger.record_rebalance_executed(id, "sig-1", "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::RebalanceWrongState { .. })
    ));

    // Cannot confirm before Executed.
    let result = ledger.confirm_rebalance(id, 10_000, "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::RebalanceWrongState { .. })
    ));

    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    // Cannot approve again once Approved.
    let result = ledger.approve_rebalance(id, "ops-bob", 1_002);
    assert!(matches!(
        result,
        Err(LedgerError::RebalanceWrongState { .. })
    ));
}

#[test]
fn rebalance_reject_and_cancel_require_a_note_and_are_terminal() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    assert!(ledger.reject_rebalance(id, "", "ops-bob", 1_001).is_err());
    ledger
        .reject_rebalance(id, "not needed right now", "ops-bob", 1_001)
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Rejected
    );
    // Terminal: cannot approve a rejected request.
    assert!(ledger.approve_rebalance(id, "ops-alice", 1_002).is_err());

    let id2 = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id2, "ops-alice", 1_001).unwrap();
    ledger
        .cancel_rebalance(id2, "plans changed", "ops-alice", 1_002)
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id2).unwrap().unwrap().state,
        RebalanceState::Cancelled
    );
}

#[test]
fn rebalance_fail_routes_to_manual_resolution_without_touching_balance() {
    let mut ledger = setup();
    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "sig-never-confirmed", "ops-alice", 1_002)
        .unwrap();
    ledger
        .fail_rebalance(
            id,
            "transaction never confirmed on-chain",
            "ops-alice",
            1_003,
        )
        .unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Failed
    );
    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(
        after, before,
        "a Failed rebalance must never adjust the cached balance"
    );
}

#[test]
fn rebalance_list_filters_by_direction_and_open_state() {
    let mut ledger = setup();
    let goldcoin_id = ledger
        .propose_rebalance(
            ReserveDirection::GoldcoinReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up goldcoin",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    let solana_id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            20_000,
            "top-up solana",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .reject_rebalance(solana_id, "not needed", "ops-bob", 1_001)
        .unwrap();

    let goldcoin_only = ledger
        .list_rebalances(Some(ReserveDirection::GoldcoinReserve), false)
        .unwrap();
    assert_eq!(goldcoin_only.len(), 1);
    assert_eq!(goldcoin_only[0].id, goldcoin_id);

    let all_open = ledger.list_rebalances(None, true).unwrap();
    assert_eq!(
        all_open.len(),
        1,
        "the rejected Solana request must not appear as open"
    );
    assert_eq!(all_open[0].id, goldcoin_id);

    let all = ledger.list_rebalances(None, false).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn rebalance_state_log_records_every_transition_in_order() {
    let mut ledger = setup();
    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            RebalanceKind::Deposit,
            10_000,
            "top-up",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
    ledger
        .record_rebalance_executed(id, "sig-log-order", "ops-alice", 1_002)
        .unwrap();
    ledger
        .confirm_rebalance(id, 10_000, "ops-alice", 1_003)
        .unwrap();
    let log = ledger.rebalance_state_log(id).unwrap();
    let to_states: Vec<RebalanceState> = log.iter().map(|(_, to, _, _, _)| *to).collect();
    assert_eq!(
        to_states,
        vec![
            RebalanceState::Proposed,
            RebalanceState::Approved,
            RebalanceState::Executed,
            RebalanceState::Confirmed,
        ]
    );
}

// -------------------------------------------------- post-finality reorg --

#[test]
fn detect_post_finality_reorg_finds_only_finalized_requests_above_the_fork_height() {
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved {
        request_id: finalized_id,
    } = ledger
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
        .record_glc_deposit_observed(finalized_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger
        .mark_glc_source_finalized(finalized_id, 1_200)
        .unwrap();

    let CreateRequestOutcome::Reserved {
        request_id: pre_finality_id,
    } = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(50_000),
            &[2u8; 32],
            None,
            3600,
            1_000,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(
            pre_finality_id,
            [0xCC; 32],
            0,
            50_000,
            20,
            [0xDD; 32],
            1_100,
        )
        .unwrap();
    // Still Confirming — never finalized.

    // A rollback to height 5 orphans the finalized request's block (10)
    // but not the pre-finality one specifically — either way, only the
    // FINALIZED request must ever be returned here, since
    // `goldcoin_rollback_reorg` already handles pre-finality rows
    // correctly on its own.
    let affected = ledger.detect_post_finality_reorg(5).unwrap();
    assert_eq!(affected, vec![finalized_id]);

    // A rollback to height 15 (above the finalized request's block 10)
    // finds nothing — routine, no post-finality impact.
    let affected = ledger.detect_post_finality_reorg(15).unwrap();
    assert!(affected.is_empty());
}

#[test]
fn record_post_finality_reorg_pauses_both_reserves_and_writes_a_distinct_audit_event() {
    let mut ledger = setup();
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());

    let id = ledger
        .record_post_finality_reorg(5, 12, &[42, 43], 1_000)
        .unwrap();
    assert!(id > 0);

    assert!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "post-finality reorg must pause the Goldcoin reserve"
    );
    assert!(
        ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
        "post-finality reorg must pause the Solana reserve too (global, docs/10-threat-model.md)"
    );
    assert_eq!(ledger.post_finality_reorg_event_count().unwrap(), 1);

    let (fork_height, old_tip_height, ids_json): (i64, i64, String) = ledger
        .raw()
        .query_row(
            "SELECT fork_height, old_tip_height, affected_request_ids FROM \
             post_finality_reorg_events WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(fork_height, 5);
    assert_eq!(old_tip_height, 12);
    let ids: Vec<i64> = serde_json::from_str(&ids_json).unwrap();
    assert_eq!(ids, vec![42, 43]);

    // Never auto-cleared — same discipline as every other pause in this
    // codebase.
    let report = crate::reconciliation::reconcile(
        &mut ledger,
        ReserveDirection::GoldcoinReserve,
        1_000_000,
        1_000_000,
        2_000,
    )
    .unwrap();
    assert_eq!(
        report.classification,
        crate::reconciliation::Classification::WithinTolerance
    );
    assert!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        "a WithinTolerance reconciliation cycle must never clear an existing pause"
    );
}

// ------------------------------------------------- custody transitions --

#[test]
fn custody_transition_vault_sweep_full_lifecycle_requires_only_goldcoin_paused() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-pubkey-1".to_string(), "old-pubkey-2".to_string()],
            &["new-pubkey-1".to_string(), "new-pubkey-2".to_string()],
            Some(2),
            "scheduled vault rotation",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    let ct = ledger.get_custody_transition(id).unwrap().unwrap();
    assert_eq!(ct.state, CustodyTransitionState::Proposed);
    assert_eq!(ct.new_threshold, Some(2));

    // Cannot approve before the new identity is verified.
    let result = ledger.approve_custody_transition(id, "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionWrongState { .. })
    ));

    ledger
        .verify_new_identity(id, "ops-verifier", 1_002)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::IdentityVerified
    );

    let outcome = ledger
        .approve_custody_transition(id, "ops-alice", 1_003)
        .unwrap();
    assert_eq!(outcome, CustodyApprovalOutcome::ThresholdReached);
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Approved
    );

    // Cannot execute while the Goldcoin reserve is still unpaused.
    let result =
        ledger.record_custody_transition_executed(id, "glc-sweep-txid-1", "ops-alice", 1_004);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionRequiresPause { .. })
    ));

    ledger
        .set_paused(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("vault sweep in progress"),
        )
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "glc-sweep-txid-1", "ops-alice", 1_005)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Executed
    );

    ledger
        .confirm_custody_transition(id, "ops-alice", 1_006)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Confirmed
    );
}

#[test]
fn custody_transition_attestation_rotation_requires_both_reserves_paused() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::AttestationKeyRotation,
            &["old-signer-a".to_string()],
            &["new-signer-a".to_string(), "new-signer-b".to_string()],
            None,
            "scheduled attestation key rotation",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();

    // Only Goldcoin paused: still not enough for an attestation rotation,
    // which authorizes BOTH bridge directions.
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("rotation"))
        .unwrap();
    let result =
        ledger.record_custody_transition_executed(id, "attn-rotate-txid-1", "ops-alice", 1_003);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionRequiresPause {
            direction: ReserveDirection::SolanaReserve,
            ..
        })
    ));

    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("rotation"))
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "attn-rotate-txid-1", "ops-alice", 1_004)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Executed
    );
}

#[test]
fn custody_transition_new_threshold_is_rejected_for_attestation_rotation() {
    let mut ledger = setup();
    let result = ledger.propose_custody_transition(
        CustodyTransitionKind::AttestationKeyRotation,
        &["old-signer-a".to_string()],
        &["new-signer-a".to_string()],
        Some(2),
        "bad request",
        "ops-alice",
        1,
        1_000,
    );
    assert!(matches!(
        result,
        Err(LedgerError::InvalidCustodyTransition(_))
    ));
}

#[test]
fn custody_transition_approving_twice_from_the_same_identity_does_not_double_count() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "top-up",
            "ops-alice",
            2,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    let o1 = ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();
    let o2 = ledger
        .approve_custody_transition(id, "ops-alice", 1_003)
        .unwrap();
    assert_eq!(
        o1, o2,
        "the same approver approving twice must not move the count"
    );
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::IdentityVerified,
        "still short of the real second distinct approver"
    );
}

#[test]
fn custody_transition_duplicate_tx_reference_is_rejected_structurally() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("sweep"))
        .unwrap();

    let id1 = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep 1",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id1, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id1, "ops-alice", 1_002)
        .unwrap();
    ledger
        .record_custody_transition_executed(id1, "glc-sweep-replay-target", "ops-alice", 1_003)
        .unwrap();

    let id2 = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-2".to_string()],
            &["new-2".to_string()],
            Some(1),
            "sweep 2",
            "ops-alice",
            1,
            1_004,
        )
        .unwrap();
    ledger
        .verify_new_identity(id2, "ops-verifier", 1_005)
        .unwrap();
    ledger
        .approve_custody_transition(id2, "ops-alice", 1_006)
        .unwrap();
    let result = ledger.record_custody_transition_executed(
        id2,
        "glc-sweep-replay-target",
        "ops-alice",
        1_007,
    );
    assert!(
        result.is_err(),
        "the same real tx_reference must never be recorded against two custody transitions"
    );
    assert_eq!(
        ledger.get_custody_transition(id1).unwrap().unwrap().state,
        CustodyTransitionState::Executed
    );
    assert_eq!(
        ledger.get_custody_transition(id2).unwrap().unwrap().state,
        CustodyTransitionState::Approved,
        "the second transition must not be left partially executed by the rejected attempt"
    );
}

#[test]
fn custody_transition_reject_and_cancel_require_a_note_and_are_terminal() {
    let mut ledger = setup();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();

    let result = ledger.reject_custody_transition(id, "", "ops-alice", 1_001);
    assert!(matches!(
        result,
        Err(LedgerError::InvalidCustodyTransition(_))
    ));

    ledger
        .reject_custody_transition(id, "identity could not be verified", "ops-alice", 1_002)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id).unwrap().unwrap().state,
        CustodyTransitionState::Rejected
    );

    let id2 = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-2".to_string()],
            &["new-2".to_string()],
            Some(1),
            "sweep 2",
            "ops-alice",
            1,
            1_003,
        )
        .unwrap();
    ledger
        .cancel_custody_transition(id2, "no longer needed", "ops-alice", 1_004)
        .unwrap();
    assert_eq!(
        ledger.get_custody_transition(id2).unwrap().unwrap().state,
        CustodyTransitionState::Cancelled
    );
}

#[test]
fn custody_transition_fail_then_rollback_records_evidence_without_touching_pause_state() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("sweep"))
        .unwrap();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "glc-sweep-fail-1", "ops-alice", 1_003)
        .unwrap();

    ledger
        .fail_custody_transition(id, "new vault never observed active", "ops-alice", 1_004)
        .unwrap();
    let ct = ledger.get_custody_transition(id).unwrap().unwrap();
    assert_eq!(ct.state, CustodyTransitionState::Failed);
    assert_eq!(
        ct.failure_reason.as_deref(),
        Some("new vault never observed active")
    );

    // A rollback is only ever an audit marker of a real, out-of-band
    // revert — it never touches reserve pause state itself.
    let paused_before = ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap();
    ledger
        .rollback_custody_transition(id, "reverted to old vault out of band", "ops-alice", 1_005)
        .unwrap();
    let ct = ledger.get_custody_transition(id).unwrap().unwrap();
    assert_eq!(ct.state, CustodyTransitionState::RolledBack);
    assert_eq!(
        ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap(),
        paused_before,
        "rollback must never itself change pause state"
    );

    // Cannot roll back a second time.
    let result = ledger.rollback_custody_transition(id, "again", "ops-alice", 1_006);
    assert!(matches!(
        result,
        Err(LedgerError::CustodyTransitionWrongState { .. })
    ));
}

#[test]
fn custody_transition_list_filters_by_kind_and_open_state() {
    let mut ledger = setup();
    let sweep_id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    let rotation_id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::AttestationKeyRotation,
            &["old-signer".to_string()],
            &["new-signer".to_string()],
            None,
            "rotation",
            "ops-alice",
            1,
            1_001,
        )
        .unwrap();
    ledger
        .reject_custody_transition(sweep_id, "closed out", "ops-alice", 1_002)
        .unwrap();

    let sweeps = ledger
        .list_custody_transitions(Some(CustodyTransitionKind::GoldcoinVaultSweep), false)
        .unwrap();
    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps[0].id, sweep_id);

    let open = ledger.list_custody_transitions(None, true).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, rotation_id);

    let all = ledger.list_custody_transitions(None, false).unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn custody_transition_state_log_records_every_transition_in_order() {
    let mut ledger = setup();
    ledger
        .set_paused(ReserveDirection::GoldcoinReserve, true, Some("sweep"))
        .unwrap();
    let id = ledger
        .propose_custody_transition(
            CustodyTransitionKind::GoldcoinVaultSweep,
            &["old-1".to_string()],
            &["new-1".to_string()],
            Some(1),
            "sweep",
            "ops-alice",
            1,
            1_000,
        )
        .unwrap();
    ledger
        .verify_new_identity(id, "ops-verifier", 1_001)
        .unwrap();
    ledger
        .approve_custody_transition(id, "ops-alice", 1_002)
        .unwrap();
    ledger
        .record_custody_transition_executed(id, "glc-sweep-log-1", "ops-alice", 1_003)
        .unwrap();
    ledger
        .confirm_custody_transition(id, "ops-alice", 1_004)
        .unwrap();

    let log = ledger.custody_transition_state_log(id).unwrap();
    let states: Vec<CustodyTransitionState> = log.iter().map(|e| e.1).collect();
    assert_eq!(
        states,
        vec![
            CustodyTransitionState::Proposed,
            CustodyTransitionState::IdentityVerified,
            CustodyTransitionState::Approved,
            CustodyTransitionState::Executed,
            CustodyTransitionState::Confirmed,
        ]
    );
}

// ------------------------------------------------- unique deposit addresses --

fn create_glc_to_sol_request(ledger: &mut Ledger) -> i64 {
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
        panic!("expected Reserved")
    };
    request_id
}

#[test]
fn set_glc_to_sol_deposit_address_round_trips() {
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);

    ledger
        .set_glc_to_sol_deposit_address(
            request_id,
            "Qsomeaddress",
            "76a914somehash88ac",
            "5221...53ae",
        )
        .unwrap();

    assert_eq!(
        ledger
            .find_glc_to_sol_request_by_deposit_script("76a914somehash88ac")
            .unwrap(),
        Some(request_id)
    );
    assert_eq!(
        ledger.all_glc_to_sol_deposit_script_pubkeys().unwrap(),
        vec!["76a914somehash88ac".to_string()]
    );
}

#[test]
fn set_glc_to_sol_deposit_address_is_idempotent_on_an_exact_repeat() {
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_glc_to_sol_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap();
    // Calling again with the SAME values must succeed, not error.
    ledger
        .set_glc_to_sol_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap();
}

#[test]
fn set_glc_to_sol_deposit_address_never_silently_overwrites_a_different_value() {
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_glc_to_sol_deposit_address(request_id, "Qfirst", "scripthex1", "redeemhex1")
        .unwrap();

    let err = ledger
        .set_glc_to_sol_deposit_address(request_id, "Qsecond", "scripthex2", "redeemhex2")
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::DepositAddressAlreadySet { id, .. } if id == request_id
    ));
    // The original assignment must still be the one in effect.
    assert_eq!(
        ledger
            .find_glc_to_sol_request_by_deposit_script("scripthex1")
            .unwrap(),
        Some(request_id)
    );
    assert_eq!(
        ledger
            .find_glc_to_sol_request_by_deposit_script("scripthex2")
            .unwrap(),
        None
    );
}

#[test]
fn set_glc_to_sol_deposit_address_rejects_a_sol_to_glc_request() {
    let mut ledger = setup();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit(0, amounts(100_000), [1u8; 32], &[2u8; 32], 1_000)
        .unwrap()
    else {
        panic!("expected FoldedFinalized")
    };

    let err = ledger
        .set_glc_to_sol_deposit_address(request_id, "Qaddr", "scripthex", "redeemhex")
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::NotAGlcToSolRequest { id, actual_direction: Direction::SolToGlc } if id == request_id
    ));
}

#[test]
fn set_glc_to_sol_deposit_address_rejects_an_unknown_request_id() {
    let mut ledger = setup();
    let err = ledger
        .set_glc_to_sol_deposit_address(999_999, "Qaddr", "scripthex", "redeemhex")
        .unwrap_err();
    assert!(matches!(err, LedgerError::RequestNotFound(999_999)));
}

#[test]
fn find_glc_to_sol_request_by_deposit_script_returns_none_for_unknown_script() {
    let ledger = setup();
    assert_eq!(
        ledger
            .find_glc_to_sol_request_by_deposit_script("never-assigned")
            .unwrap(),
        None
    );
}

#[test]
fn find_glc_to_sol_request_by_deposit_script_does_not_match_a_sol_to_glc_row() {
    // Defense in depth: even if a SolToGlc row somehow had a non-NULL
    // deposit_script_pubkey_hex (it never legitimately can, since
    // `set_glc_to_sol_deposit_address` refuses that direction outright),
    // the lookup itself is also direction-scoped.
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_glc_to_sol_deposit_address(request_id, "Qaddr", "shared-script", "redeemhex")
        .unwrap();
    assert_eq!(
        ledger
            .find_glc_to_sol_request_by_deposit_script("shared-script")
            .unwrap(),
        Some(request_id)
    );
}

#[test]
fn all_glc_to_sol_deposit_script_pubkeys_includes_settled_requests() {
    // A settled request's derived address can still hold an unswept UTXO
    // -- the enumeration must include it, not just currently-open
    // AwaitingDeposit requests.
    let mut ledger = setup();
    let request_id = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_glc_to_sol_deposit_address(request_id, "Qaddr", "settled-script", "redeemhex")
        .unwrap();
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 100_000, 10, [0xBB; 32], 1_100)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 1_200).unwrap();

    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
    assert!(ledger
        .all_glc_to_sol_deposit_script_pubkeys()
        .unwrap()
        .contains(&"settled-script".to_string()));
}

#[test]
fn all_glc_to_sol_deposit_script_pubkeys_excludes_requests_with_no_address_assigned() {
    let mut ledger = setup();
    let _request_id = create_glc_to_sol_request(&mut ledger); // never assigned an address
    assert!(ledger
        .all_glc_to_sol_deposit_script_pubkeys()
        .unwrap()
        .is_empty());
}

#[test]
fn two_requests_can_never_share_the_same_deposit_script_pubkey() {
    let mut ledger = setup();
    let a = create_glc_to_sol_request(&mut ledger);
    let b = create_glc_to_sol_request(&mut ledger);
    ledger
        .set_glc_to_sol_deposit_address(a, "Qaddr-a", "same-script", "redeem-a")
        .unwrap();
    // The database-level partial unique index (ux_bridge_requests_deposit_script)
    // is the actual, race-safe guarantee here -- not application logic.
    let err = ledger
        .set_glc_to_sol_deposit_address(b, "Qaddr-b", "same-script", "redeem-b")
        .unwrap_err();
    assert!(matches!(err, LedgerError::Sqlite(_)));
}

// -------------------------------------- unmatched deposit / vault split reconciliation --

fn broadcast_vault_split(
    ledger: &mut Ledger,
    split_txid: [u8; 32],
    source_amount_atomic: u64,
    fee_atomic: u64,
    output_amounts: Vec<u64>,
) -> i64 {
    let plan = crate::goldcoin::split::SplitPlan {
        source: crate::goldcoin::coin::VaultUtxo {
            txid: [0xEEu8; 32],
            vout: 0,
            amount_atomic: source_amount_atomic,
            script_pubkey_hex: "deadbeef".to_string(),
        },
        vault_script_pubkey: vec![0xAA],
        output_amounts,
        fee_atomic,
    };
    let id = ledger
        .record_vault_utxo_split_built(&plan, 1, "unsigned-hex", "test split", 0)
        .unwrap();
    ledger
        .record_vault_utxo_split_signed(id, "signed-hex", 0)
        .unwrap();
    ledger
        .record_vault_utxo_split_broadcast(id, split_txid, 0)
        .unwrap();
    id
}

#[test]
fn get_broadcast_vault_utxo_split_returns_the_persisted_figures() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    let split = ledger
        .get_broadcast_vault_utxo_split(split_txid)
        .unwrap()
        .unwrap();
    assert_eq!(split.source_amount_atomic, 1_000_000);
    assert_eq!(split.fee_atomic, 100);
    assert_eq!(split.chunk_count, 3);
}

#[test]
fn get_broadcast_vault_utxo_split_is_none_for_an_unknown_txid() {
    let ledger = setup();
    assert!(ledger
        .get_broadcast_vault_utxo_split([0x11u8; 32])
        .unwrap()
        .is_none());
}

#[test]
fn reconciles_an_unmatched_deposit_that_exactly_matches_a_split_output() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    ledger
        .record_unmatched_goldcoin_deposit(split_txid, 0, 333_300, 50, "no_request_binding", 1_000)
        .unwrap();

    let outcome = ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "reconciling", 2_000)
        .unwrap();
    assert_eq!(outcome, ReconcileUnmatchedDepositOutcome::Reconciled);

    let reconciled_at: Option<i64> = ledger
        .raw()
        .query_row(
            "SELECT reconciled_at FROM unmatched_goldcoin_deposits WHERE txid = ?1 AND vout = 0",
            [split_txid.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        reconciled_at,
        Some(2_000),
        "the row must be marked reconciled, never deleted"
    );
    let count: i64 = ledger
        .raw()
        .query_row(
            "SELECT count(*) FROM unmatched_goldcoin_deposits",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "reconciling must never delete the audit row");
}

#[test]
fn reconcile_is_idempotent_on_an_already_reconciled_row() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    ledger
        .record_unmatched_goldcoin_deposit(split_txid, 0, 333_300, 50, "no_request_binding", 1_000)
        .unwrap();
    ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "first reconcile", 2_000)
        .unwrap();

    let outcome = ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "second reconcile attempt", 3_000)
        .unwrap();
    assert_eq!(outcome, ReconcileUnmatchedDepositOutcome::AlreadyReconciled);
    let reconciled_at: Option<i64> = ledger
        .raw()
        .query_row(
            "SELECT reconciled_at FROM unmatched_goldcoin_deposits WHERE txid = ?1 AND vout = 0",
            [split_txid.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        reconciled_at,
        Some(2_000),
        "a repeat call must never overwrite the original reconciliation timestamp"
    );
}

#[test]
fn refuses_to_reconcile_a_row_that_does_not_match_any_split() {
    let mut ledger = setup();
    ledger
        .record_unmatched_goldcoin_deposit([0x99u8; 32], 0, 500, 50, "no_request_binding", 1_000)
        .unwrap();
    let err = ledger
        .reconcile_unmatched_goldcoin_deposit([0x99u8; 32], 0, "reconciling", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::UnmatchedDepositNotAKnownSplitOutput { .. }
    ));
}

#[test]
fn refuses_to_reconcile_a_row_with_a_wrong_amount_even_if_the_split_exists() {
    let mut ledger = setup();
    let split_txid = [0xCCu8; 32];
    broadcast_vault_split(
        &mut ledger,
        split_txid,
        1_000_000,
        100,
        vec![333_300, 333_300, 333_300],
    );
    // Recorded amount does not match the split's expected output at vout 0.
    ledger
        .record_unmatched_goldcoin_deposit(split_txid, 0, 999_999, 50, "no_request_binding", 1_000)
        .unwrap();
    let err = ledger
        .reconcile_unmatched_goldcoin_deposit(split_txid, 0, "reconciling", 2_000)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::UnmatchedDepositNotAKnownSplitOutput { .. }
    ));
}

#[test]
fn refuses_to_reconcile_an_unknown_row() {
    let mut ledger = setup();
    let err = ledger
        .reconcile_unmatched_goldcoin_deposit([0x77u8; 32], 0, "reconciling", 2_000)
        .unwrap_err();
    assert!(matches!(err, LedgerError::UnmatchedDepositNotFound { .. }));
}
