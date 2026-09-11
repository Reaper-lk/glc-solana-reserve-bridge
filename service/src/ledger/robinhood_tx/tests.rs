//! Restart and idempotency tests for the Robinhood outbound lifecycle.
//!
//! # What these test, and why they are here rather than in `settlement`
//!
//! Phase F's requirement is that a restart at ANY point cannot produce a
//! duplicate payout, settlement, refund, or nonce. A restart is not
//! simulated by calling a function twice — it is simulated by DROPPING
//! the in-memory state entirely and re-opening the database, which is
//! what actually happens.
//!
//! So every test below writes to a real file-backed ledger, closes it,
//! re-opens it, and continues. What survives is exactly what a crashed
//! process would have left behind, and the assertions are about what the
//! next process can and cannot then do.

use super::*;
use crate::ledger::{Direction, RequestState, ReserveDirection};
use crate::routes::Route;

/// A file-backed ledger, so it can genuinely be closed and re-opened.
fn open(dir: &std::path::Path) -> Ledger {
    Ledger::open(&dir.join("ledger.sqlite3")).expect("opens the ledger")
}

fn seed_request(ledger: &Ledger, direction: &str, obligation: i64) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                 source_contract, source_obligation_index)
             VALUES (?1, 'SourceFinalized', 1000, 300, 30, 970, 970, X'ab', 100, 'robinhood',
                     X'1111111111111111111111111111111111111111', ?2)",
            rusqlite::params![direction, obligation],
        )
        .expect("seeds a request");
    ledger.conn_for_tests().last_insert_rowid()
}

fn new_tx(request_id: i64, kind: RobinhoodTxKind) -> NewRobinhoodTx {
    NewRobinhoodTx {
        kind,
        request_id: Some(request_id),
        rebalance_request_id: None,
        route: Some(match kind {
            RobinhoodTxKind::Payout => Route::GlcToRhn,
            _ => Route::RhnToGlc,
        }),
        bridge_contract: [0x11; 20],
        chain_id: 4663,
        contract_request_id: [0x22; 32],
        obligation_index: match kind {
            RobinhoodTxKind::Payout => None,
            _ => Some(7),
        },
        recipient: match kind {
            RobinhoodTxKind::Settlement => None,
            _ => Some([0xec; 20]),
        },
        amount_robinhood: match kind {
            RobinhoodTxKind::Settlement => None,
            _ => Some([3u8; 32]),
        },
        signer_epoch: 7,
        expiry: 1_800_000_000,
        auth_digest: [0x5a; 32],
    }
}

const SUBMITTER: [u8; 20] = [0x5b; 20];

fn id_of(outcome: BeginTxOutcome) -> i64 {
    match outcome {
        BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
    }
}

// =====================================================================
// The restart matrix
// =====================================================================

/// Restart BEFORE any signature was collected.
///
/// The operation row exists with its payload fixed — including its
/// `expiry`, a wall-clock deadline — so the next process resumes the SAME
/// authorization rather than minting a second, differently-expiring one.
#[test]
fn restart_before_signatures_resumes_the_same_authorization_payload() {
    let dir = tempfile::tempdir().unwrap();
    let (id, digest, expiry) = {
        let mut ledger = open(dir.path());
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
        (id, tx.auth_digest, tx.expiry)
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Authorizing);
    assert_eq!(
        tx.auth_digest, digest,
        "the payload is fixed, not re-minted"
    );
    assert_eq!(tx.expiry, expiry, "including its wall-clock deadline");
    assert!(tx.nonce.is_none());
    assert!(tx.raw_tx.is_none());

    // Re-beginning resumes rather than duplicating.
    assert_eq!(
        id_of(
            ledger
                .begin_robinhood_tx(
                    &new_tx(tx.request_id.unwrap(), RobinhoodTxKind::Payout),
                    9_999
                )
                .unwrap()
        ),
        id
    );
}

/// Restart AFTER the quorum was collected but before a nonce existed.
#[test]
fn restart_after_signatures_keeps_them_and_refuses_a_second_quorum() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let mut ledger = open(dir.path());
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        ledger
            .record_robinhood_authorization(
                id,
                &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
                100,
            )
            .unwrap();
        id
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Authorized);
    assert_eq!(ledger.robinhood_auth_signatures(id).unwrap().len(), 2);
    assert!(tx.nonce.is_none(), "no nonce was burned");

    // Re-recording the SAME quorum is a no-op; a DIFFERENT one is
    // refused, so a restart cannot produce two competing authorizations.
    ledger
        .record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
            200,
        )
        .unwrap();
    assert!(ledger
        .record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x99; 65]), ([0xa3; 20], [0x88; 65])],
            200
        )
        .is_err());
}

/// Restart AFTER the nonce was allocated but before anything was signed.
///
/// The nonce is durably owned by this operation, so no other operation
/// can take it — and re-allocating returns the same one.
#[test]
fn restart_after_nonce_allocation_keeps_the_nonce_and_blocks_every_other_claim() {
    let dir = tempfile::tempdir().unwrap();
    let (id, nonce) = {
        let mut ledger = open(dir.path());
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        ledger
            .record_robinhood_authorization(
                id,
                &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
                100,
            )
            .unwrap();
        let nonce = ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap();
        (id, nonce)
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    assert_eq!(
        ledger.get_robinhood_tx(id).unwrap().unwrap().nonce,
        Some(nonce)
    );
    assert_eq!(
        ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 999)
            .unwrap(),
        nonce,
        "re-allocation returns the same nonce, never a second one"
    );

    // A DIFFERENT operation gets the NEXT nonce, never this one. It
    // carries its own contract request id, because the ledger mirrors the
    // contract's `(action, requestId)` replay guard — two operations
    // claiming one id is refused there too.
    let other_request = seed_request(&ledger, "GlcToRhn", 2);
    let mut other_tx = new_tx(other_request, RobinhoodTxKind::Payout);
    other_tx.contract_request_id = [0x33; 32];
    let other = id_of(ledger.begin_robinhood_tx(&other_tx, 200).unwrap());
    ledger
        .record_robinhood_authorization(
            other,
            &[([0xa1; 20], [0x44; 65]), ([0xa2; 20], [0x55; 65])],
            200,
        )
        .unwrap();
    assert_eq!(
        ledger
            .allocate_robinhood_nonce(other, SUBMITTER, 4663, 200)
            .unwrap(),
        nonce + 1
    );
}

/// Restart AFTER the raw transaction was built and persisted, before it
/// was broadcast.
#[test]
fn restart_after_signing_re_broadcasts_the_identical_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (id, raw) = {
        let mut ledger = open(dir.path());
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        ledger
            .record_robinhood_authorization(
                id,
                &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
                100,
            )
            .unwrap();
        ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap();
        let raw = vec![0x02, 0xf8, 0x6b, 0x12];
        ledger
            .record_robinhood_signed(id, "eip1559", 150_000, "f", &raw, [0xaa; 32], 100)
            .unwrap();
        (id, raw)
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Signed);
    assert_eq!(
        tx.raw_tx.as_ref(),
        Some(&raw),
        "the exact bytes survive, so recovery is a re-send rather than a rebuild"
    );
    assert_eq!(tx.broadcast_attempts, 0, "nothing was sent yet");
    // And a rebuild that produced DIFFERENT bytes is refused.
    assert!(ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[9, 9], [0xbb; 32], 200)
        .is_err());
}

/// Restart with the broadcast result UNCERTAIN — the transport failed, so
/// the bytes may or may not have reached the node.
#[test]
fn restart_after_an_uncertain_broadcast_never_produces_a_second_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let (id, raw, nonce) = {
        let mut ledger = open(dir.path());
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        ledger
            .record_robinhood_authorization(
                id,
                &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
                100,
            )
            .unwrap();
        let nonce = ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap();
        let raw = vec![0x02, 0xf8];
        ledger
            .record_robinhood_signed(id, "eip1559", 150_000, "f", &raw, [0xaa; 32], 100)
            .unwrap();
        // The send happened; the answer did not.
        ledger.record_robinhood_broadcast(id, 100).unwrap();
        (id, raw, nonce)
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Broadcast);
    assert_eq!(tx.nonce, Some(nonce));
    assert_eq!(tx.raw_tx, Some(raw.clone()));
    assert_eq!(tx.first_broadcast_at, Some(100));

    // Every recovery step keeps the SAME nonce and the SAME bytes.
    ledger.record_robinhood_broadcast(id, 200).unwrap();
    let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(tx.nonce, Some(nonce));
    assert_eq!(tx.raw_tx, Some(raw));
    assert_eq!(tx.broadcast_attempts, 2, "the attempts are visible");
    assert_eq!(
        tx.first_broadcast_at,
        Some(100),
        "the first-broadcast time is never overwritten"
    );
}

/// Restart AFTER the transaction was included but before finality.
#[test]
fn restart_after_inclusion_resumes_confirmation_counting() {
    let dir = tempfile::tempdir().unwrap();
    let id = {
        let mut ledger = open(dir.path());
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        ledger
            .record_robinhood_authorization(
                id,
                &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
                100,
            )
            .unwrap();
        ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap();
        ledger
            .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
            .unwrap();
        ledger.record_robinhood_broadcast(id, 100).unwrap();
        ledger
            .record_robinhood_receipt(id, true, 500, [0xcc; 32], 200)
            .unwrap();
        id
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    let tx = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(tx.state, RobinhoodTxState::Included);
    assert_eq!(tx.receipt_block_number, Some(500));
    // Re-reading the SAME receipt is a no-op; the finality transition
    // still fires exactly once.
    ledger
        .record_robinhood_receipt(id, true, 500, [0xcc; 32], 300)
        .unwrap();
    assert!(ledger
        .update_robinhood_confirmations(id, 3, 3, 400)
        .unwrap());
    assert!(!ledger
        .update_robinhood_confirmations(id, 4, 3, 500)
        .unwrap());
}

/// Restart BETWEEN the on-chain finality and the ledger's own completion
/// update — the narrowest window in the whole flow.
#[test]
fn restart_before_the_completion_update_leaves_it_recoverable_and_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let request_id = {
        let mut ledger = open(dir.path());
        ledger
            .configure_reserve(
                ReserveDirection::RobinhoodReserve,
                970,
                0,
                970,
                500,
                250,
                100,
            )
            .unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE reserve_ledger SET total_reserve_balance = 970,
                    reserved_liquidity = 970, pending_obligations = 970
                 WHERE direction = 'RobinhoodReserve'",
                [],
            )
            .unwrap();
        let request_id = seed_request(&ledger, "GlcToRhn", 1);
        let id = id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
                .unwrap(),
        );
        ledger
            .record_robinhood_authorization(
                id,
                &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
                100,
            )
            .unwrap();
        ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap();
        ledger
            .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
            .unwrap();
        ledger.record_robinhood_broadcast(id, 100).unwrap();
        ledger
            .record_robinhood_receipt(id, true, 500, [0xcc; 32], 200)
            .unwrap();
        ledger
            .update_robinhood_confirmations(id, 10, 3, 300)
            .unwrap();
        // ... and the process dies before `mark_robinhood_payout_settled`.
        request_id
    };

    // --- crash ---
    let mut ledger = open(dir.path());
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::SourceFinalized,
        "the ledger has not yet recorded the completion"
    );
    // The transaction row IS finalized, so the next tick simply applies
    // the completion — and applying it twice is a no-op.
    ledger
        .mark_robinhood_payout_settled(request_id, 400)
        .unwrap();
    ledger
        .mark_robinhood_payout_settled(request_id, 500)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    // The reserve moved exactly once.
    assert_eq!(
        ledger
            .settled_liquidity(ReserveDirection::RobinhoodReserve)
            .unwrap(),
        970
    );
    let (balance, _, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!((balance, reserved, pending), (0, 0, 0));
}

// =====================================================================
// Cross-kind and cross-direction guarantees
// =====================================================================

#[test]
fn the_three_kinds_are_separate_operations_on_one_request() {
    // The contract keys its replay guard on `(action, requestId)`, so one
    // request legitimately has up to one operation of each kind — and
    // never two of any.
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let request_id = seed_request(&ledger, "RhnToGlc", 1);

    let settlement = id_of(
        ledger
            .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Settlement), 100)
            .unwrap(),
    );
    // A refund for the same request needs its own contract request id,
    // because the mirrored replay guard is per (chain, contract, action,
    // requestId) and the action differs — but the ledger's own
    // `(kind, request_id)` guard is what stops a SECOND settlement.
    let mut refund_tx = new_tx(request_id, RobinhoodTxKind::Refund);
    refund_tx.contract_request_id = [0x44; 32];
    let refund = id_of(ledger.begin_robinhood_tx(&refund_tx, 100).unwrap());
    assert_ne!(settlement, refund);

    assert_eq!(
        id_of(
            ledger
                .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Settlement), 200)
                .unwrap()
        ),
        settlement,
        "a second settlement resumes the first"
    );
}

#[test]
fn a_payout_row_cannot_name_an_obligation_and_a_settlement_cannot_name_an_amount() {
    // Stated as database CHECKs, so a bug produces a constraint violation
    // rather than a malformed authorization.
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let request_id = seed_request(&ledger, "GlcToRhn", 1);

    let mut bad = new_tx(request_id, RobinhoodTxKind::Payout);
    bad.obligation_index = Some(1);
    assert!(
        ledger.begin_robinhood_tx(&bad, 100).is_err(),
        "a payout settles a deposit on another chain and names no obligation"
    );

    let request_id = seed_request(&ledger, "RhnToGlc", 2);
    let mut bad = new_tx(request_id, RobinhoodTxKind::Settlement);
    bad.recipient = Some([0xec; 20]);
    bad.amount_robinhood = Some([1u8; 32]);
    assert!(
        ledger.begin_robinhood_tx(&bad, 100).is_err(),
        "a settlement moves nothing and names neither a recipient nor an amount"
    );
}

#[test]
fn the_action_byte_and_the_kind_can_never_disagree() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let request_id = seed_request(&ledger, "GlcToRhn", 1);
    let id = id_of(
        ledger
            .begin_robinhood_tx(&new_tx(request_id, RobinhoodTxKind::Payout), 100)
            .unwrap(),
    );
    // Two independent recordings of one fact; the schema keeps them
    // agreeing.
    assert!(ledger
        .conn_for_tests()
        .execute(
            "UPDATE robinhood_transactions SET action = 3 WHERE id = ?1",
            [id]
        )
        .is_err());
}

#[test]
fn a_settlement_cannot_be_recorded_against_a_glc_to_rhn_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let request_id = seed_request(&ledger, "GlcToRhn", 1);
    assert!(matches!(
        ledger.mark_robinhood_settlement_confirmed(request_id, 100),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
}

#[test]
fn a_payout_completion_cannot_be_recorded_against_an_rhn_to_glc_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let request_id = seed_request(&ledger, "RhnToGlc", 1);
    assert!(matches!(
        ledger.mark_robinhood_payout_settled(request_id, 100),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
}

#[test]
fn parking_a_request_releases_its_reservation_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    ledger
        .configure_reserve(
            ReserveDirection::RobinhoodReserve,
            970,
            0,
            970,
            500,
            250,
            100,
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE reserve_ledger SET total_reserve_balance = 970,
                reserved_liquidity = 970, pending_obligations = 970
             WHERE direction = 'RobinhoodReserve'",
            [],
        )
        .unwrap();
    let request_id = seed_request(&ledger, "GlcToRhn", 1);

    ledger
        .mark_robinhood_request_manual_review(request_id, "its payout reverted", 200)
        .unwrap();
    // Idempotent — a second park changes nothing.
    ledger
        .mark_robinhood_request_manual_review(request_id, "again", 300)
        .unwrap();

    let (_, _, reserved, pending) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(
        (reserved, pending),
        (0, 0),
        "the reservation is released once, not twice"
    );
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("its payout reverted"),
        "the FIRST reason is kept — it is the one that explains what happened"
    );
}

#[test]
fn a_settled_request_is_never_parked_by_a_later_disagreement() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let request_id = seed_request(&ledger, "GlcToRhn", 1);
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'Settled' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    ledger
        .mark_robinhood_request_manual_review(request_id, "too late", 200)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled,
        "a completed request cannot be un-completed by a later disagreement"
    );
}

#[test]
fn every_direction_reports_the_reserve_it_actually_draws_down() {
    assert_eq!(
        Direction::GlcToSol.destination_reserve(),
        ReserveDirection::SolanaReserve
    );
    assert_eq!(
        Direction::SolToGlc.destination_reserve(),
        ReserveDirection::GoldcoinReserve
    );
    assert_eq!(
        Direction::GlcToRhn.destination_reserve(),
        ReserveDirection::RobinhoodReserve
    );
    assert_eq!(
        Direction::RhnToGlc.destination_reserve(),
        ReserveDirection::GoldcoinReserve,
        "a Robinhood deposit is paid out of the GOLDCOIN vault"
    );
    // The three reserves are never conflated.
    let mut seen = std::collections::HashSet::new();
    for reserve in ReserveDirection::ALL {
        assert!(seen.insert(reserve.as_str()));
    }
    assert_eq!(seen.len(), 3);
}

#[test]
fn the_kind_and_its_contract_action_agree_for_every_kind() {
    for kind in RobinhoodTxKind::ALL {
        let expected = match kind {
            RobinhoodTxKind::Payout => crate::robinhood::auth::ACTION_PAYOUT,
            RobinhoodTxKind::Refund => crate::robinhood::auth::ACTION_REFUND,
            RobinhoodTxKind::Settlement => crate::robinhood::auth::ACTION_SETTLE,
            RobinhoodTxKind::TreasuryWithdraw => crate::robinhood::auth::ACTION_TREASURY_WITHDRAW,
        };
        assert_eq!(kind.action(), expected);
        assert_eq!(kind.as_str().parse::<RobinhoodTxKind>().unwrap(), kind);
    }
}

#[test]
fn every_transaction_state_round_trips_and_classifies_itself() {
    for state in [
        RobinhoodTxState::Authorizing,
        RobinhoodTxState::Authorized,
        RobinhoodTxState::Signed,
        RobinhoodTxState::Broadcast,
        RobinhoodTxState::Included,
        RobinhoodTxState::Finalized,
        RobinhoodTxState::Reverted,
        RobinhoodTxState::ManualReview,
    ] {
        assert_eq!(state.as_str().parse::<RobinhoodTxState>().unwrap(), state);
    }
    for terminal in [
        RobinhoodTxState::Finalized,
        RobinhoodTxState::Reverted,
        RobinhoodTxState::ManualReview,
    ] {
        assert!(terminal.is_terminal());
        assert!(!terminal.is_in_flight());
    }
    for in_flight in [RobinhoodTxState::Broadcast, RobinhoodTxState::Included] {
        assert!(in_flight.is_in_flight());
        assert!(!in_flight.is_terminal());
    }
}

#[test]
fn two_operations_can_never_claim_one_contract_request_id() {
    // The ledger MIRRORS the contract's own replay guard: it consumes
    // `(action, requestId)` exactly once per deployment, so recording two
    // local operations against one is a state the chain could never
    // reach.
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = open(dir.path());
    let a = seed_request(&ledger, "GlcToRhn", 1);
    let b = seed_request(&ledger, "GlcToRhn", 2);
    ledger
        .begin_robinhood_tx(&new_tx(a, RobinhoodTxKind::Payout), 100)
        .unwrap();
    // Same contract_request_id, same action, same deployment.
    assert!(
        ledger
            .begin_robinhood_tx(&new_tx(b, RobinhoodTxKind::Payout), 100)
            .is_err(),
        "one contract request id belongs to one operation"
    );

    // A DIFFERENT deployment address is a different namespace, so the
    // same id there is legitimately a different operation.
    let mut elsewhere = new_tx(b, RobinhoodTxKind::Payout);
    elsewhere.bridge_contract = [0x77; 20];
    ledger.begin_robinhood_tx(&elsewhere, 100).unwrap();
}
