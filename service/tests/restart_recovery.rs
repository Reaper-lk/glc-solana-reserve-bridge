//! Crash/restart recovery tests (priority 10, and priority 5's "idempotent
//! event processing and restart recovery" requirement).
//!
//! These use a FILE-backed [`Ledger`] (not `:memory:`) and simulate a crash
//! by simply dropping the `Ledger` value and re-opening a fresh one against
//! the same path — the same thing a process restart does. There is no
//! separate "recovery" code path to test: every mutating `Ledger` method
//! commits inside one SQLite transaction (module docs on
//! `service/src/ledger/mod.rs`), so the guarantee under test is that
//! reopening the same file always reflects the last fully-committed state,
//! with nothing partially applied and nothing lost.

use glc_reserve_bridge_service::ledger::{
    CreateRequestOutcome, Direction, GlcObservationOutcome, Ledger, RequestAmounts, RequestState,
    ReserveDirection, SolFoldOutcome,
};
use glc_reserve_bridge_service::reconciliation;

/// A fee-free `RequestAmounts` for tests that predate the bridge fee and
/// exercise restart/recovery safety properties orthogonal to fee math
/// (docs/20-bridge-fee.md) — the ledger itself never validates
/// `fee_bps`/computes a fee, so this is a legitimate input here.
fn amounts(gross: u64) -> RequestAmounts {
    RequestAmounts {
        gross_atomic: gross,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: gross,
        net_destination_atomic: gross,
    }
}

fn configure(ledger: &mut Ledger) {
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            10_000_000,
            100_000,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            10_000_000,
            100_000,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
}

#[test]
fn reservation_survives_restart_with_correct_capacity_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");

    let request_id = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                amounts(1_000_000),
                &[1u8; 32],
                None,
                3600,
                100,
            )
            .unwrap()
        else {
            panic!("reservation should succeed")
        };
        request_id
        // `ledger` dropped here — simulates process exit.
    };

    // "Restart": reopen against the same file.
    let ledger = Ledger::open(&path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::AwaitingDeposit);
    assert_eq!(req.gross_amount_atomic, 1_000_000);
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::SolanaReserve)
            .unwrap(),
        10_000_000 - 100_000 - 1_000_000,
        "reserved capacity must not be lost or double-counted across a restart"
    );
    ledger
        .check_invariant(ReserveDirection::SolanaReserve)
        .unwrap();
}

#[test]
fn deposit_observed_but_not_yet_finalized_survives_restart_and_finalization_still_proceeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");

    let request_id = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                amounts(1_000_000),
                &[1u8; 32],
                None,
                3600,
                100,
            )
            .unwrap()
        else {
            panic!()
        };
        let outcome = ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 1_000_000, 10, [0xBB; 32], 200)
            .unwrap();
        assert_eq!(outcome, GlcObservationOutcome::Recorded);
        request_id
        // Crash here: deposit seen, not yet finalized.
    };

    let mut ledger = Ledger::open(&path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Confirming);
    assert_eq!(
        req.source_txid,
        Some([0xAA; 32]),
        "the observed deposit must not be forgotten"
    );

    // Work continues normally after restart — finalization is not blocked
    // by having crashed mid-confirmation.
    ledger.mark_glc_source_finalized(request_id, 300).unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

#[test]
fn reprocessing_the_same_glc_block_after_restart_never_double_reserves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                amounts(1_000_000),
                &[1u8; 32],
                None,
                3600,
                100,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 1_000_000, 10, [0xBB; 32], 200)
            .unwrap();
        request_id
    };

    // Simulate: indexer crashed right after committing the observation but
    // before it could record its own "I've processed block 10" bookkeeping
    // (goldcoin_ingest_block), so on restart it re-scans block 10 and
    // re-observes the identical deposit.
    let mut ledger = Ledger::open(&path).unwrap();
    let outcome = ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 1_000_000, 10, [0xBB; 32], 250)
        .unwrap();
    assert_eq!(outcome, GlcObservationOutcome::AlreadyRecorded);
    assert_eq!(
        ledger.available_capacity(ReserveDirection::SolanaReserve).unwrap(),
        10_000_000 - 100_000 - 1_000_000,
        "capacity must be reserved exactly once regardless of how many times the same block is reprocessed"
    );
}

#[test]
fn sol_fold_cursor_and_idempotency_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");

    let request_id = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
        let SolFoldOutcome::FoldedFinalized { request_id } = ledger
            .fold_sol_deposit(
                3,
                amounts(500_000),
                [9u8; 32],
                b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
                100,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .set_last_synced_obligation_count(4, 12345, 100)
            .unwrap();
        request_id
    };

    let mut ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.last_synced_obligation_count().unwrap(),
        4,
        "sync cursor must persist"
    );
    let outcome = ledger
        .fold_sol_deposit(
            3,
            amounts(500_000),
            [9u8; 32],
            b"mzBc4XEFSdzCDcTxAgf6EZXgsZWpztRhef",
            200,
        )
        .unwrap();
    assert_eq!(
        outcome,
        SolFoldOutcome::AlreadyFolded { request_id },
        "re-folding the same obligation after restart must be a no-op"
    );
    assert_eq!(
        ledger
            .available_capacity(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        10_000_000 - 100_000 - 500_000,
        "no duplicate reservation from the replayed fold"
    );
}

#[test]
fn reconciliation_pause_persists_across_restart_and_requires_an_operator_to_clear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
        // Unexplained drop beyond tolerance -> auto-pause.
        reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            5_000_000,
            100,
            500,
        )
        .unwrap();
    }

    let mut ledger = Ledger::open(&path).unwrap();
    assert!(
        ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
        "pause must survive a restart"
    );

    // New reservations against the paused direction still fail closed after
    // restart — a restart must never silently clear an operator-relevant
    // pause.
    let outcome = ledger
        .create_request(
            Direction::GlcToSol,
            amounts(1_000),
            &[1u8; 32],
            None,
            3600,
            600,
        )
        .unwrap();
    assert_eq!(outcome, CreateRequestOutcome::Paused);

    // Only an explicit operator action clears it.
    ledger
        .set_paused(
            ReserveDirection::SolanaReserve,
            false,
            Some("operator cleared after investigation"),
        )
        .unwrap();
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn pre_finality_reorg_state_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let request_id = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
        let CreateRequestOutcome::Reserved { request_id } = ledger
            .create_request(
                Direction::GlcToSol,
                amounts(1_000_000),
                &[1u8; 32],
                None,
                3600,
                100,
            )
            .unwrap()
        else {
            panic!()
        };
        ledger
            .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 1_000_000, 10, [0xBB; 32], 200)
            .unwrap();
        ledger.mark_glc_reorged(request_id, 300).unwrap();
        request_id
    };

    let ledger = Ledger::open(&path).unwrap();
    let req = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        req.state,
        RequestState::AwaitingDeposit,
        "reorg recovery state must survive a restart"
    );
    assert_eq!(req.source_txid, None);
    // The audit trail (that a reorg happened) is preserved even though the
    // live state moved on.
    let log = ledger.state_log(request_id).unwrap();
    assert!(log.iter().any(|(_, to, _, _)| *to == RequestState::Reorged));
}

#[test]
fn goldcoin_chain_tip_and_reorg_events_survive_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        ledger
            .goldcoin_ingest_block(0, [0u8; 32], [0u8; 32], 0, 0)
            .unwrap();
        ledger
            .goldcoin_ingest_block(1, [1u8; 32], [0u8; 32], 0, 0)
            .unwrap();
        ledger
            .goldcoin_rollback_reorg(0, [0u8; 32], 1, [1u8; 32], 100)
            .unwrap();
    }

    let ledger = Ledger::open(&path).unwrap();
    let (tip, hash) = ledger.goldcoin_chain_tip().unwrap().unwrap();
    assert_eq!(tip, 0);
    assert_eq!(hash, [0u8; 32]);
    assert_eq!(
        ledger.goldcoin_reorg_event_count().unwrap(),
        1,
        "reorg event history must survive a restart for audit"
    );
}

#[test]
fn rebalance_state_survives_restart_at_every_stage() {
    use glc_reserve_bridge_service::ledger::{RebalanceKind, RebalanceState};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let id = {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger);
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
        ledger.approve_rebalance(id, "ops-alice", 1_001).unwrap();
        id
        // Crash here: one of two required approvals recorded.
    };

    let mut ledger = Ledger::open(&path).unwrap();
    let rb = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(
        rb.state,
        RebalanceState::Proposed,
        "partial approval must survive a restart"
    );
    assert_eq!(rb.approved_by, vec!["ops-alice".to_string()]);

    // Work continues normally after restart.
    let outcome = ledger.approve_rebalance(id, "ops-bob", 1_002).unwrap();
    assert_eq!(
        outcome,
        glc_reserve_bridge_service::ledger::RebalanceApprovalOutcome::ThresholdReached
    );
    ledger
        .record_rebalance_executed(id, "sig-restart-test", "ops-alice", 1_003)
        .unwrap();
    drop(ledger);

    // Second restart, after execution but before confirmation.
    let mut ledger = Ledger::open(&path).unwrap();
    let rb = ledger.get_rebalance(id).unwrap().unwrap();
    assert_eq!(
        rb.state,
        RebalanceState::Executed,
        "recorded execution must survive a restart"
    );
    assert_eq!(rb.tx_reference, Some("sig-restart-test".to_string()));

    let before = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 1_004)
        .unwrap();
    drop(ledger);

    // Third restart, after confirmation: the balance adjustment persisted too.
    let ledger = Ledger::open(&path).unwrap();
    assert_eq!(
        ledger.get_rebalance(id).unwrap().unwrap().state,
        RebalanceState::Confirmed
    );
    let after = ledger
        .reserve_thresholds(ReserveDirection::SolanaReserve)
        .unwrap()
        .0;
    assert_eq!(after, before + 50_000);
}
