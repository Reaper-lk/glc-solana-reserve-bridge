//! Ledger-side manual Solana refund: eligibility refusals, the atomic
//! record+close, and idempotent re-import.

use super::*;
use crate::ledger::{ClosureDisposition, RequestState};

const GROSS: u64 = 5_000_000_000_000;
const FEE: u64 = 300_000_000_000;
const NET: u64 = 4_700_000_000_000;
const MINT_UNITS: u64 = 50_000_000_000;
const REQUESTER: [u8; 32] = [0x6b; 32];
const NOW: i64 = 1_800_000_000;

fn ledger() -> Ledger {
    Ledger::open_in_memory().unwrap()
}

/// A parked SolToGlc deposit, exactly as the fold leaves one.
fn seed_parked(ledger: &Ledger, direction: &str, obligation: i64) -> i64 {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
                 source_chain, source_contract, source_obligation_index, source_confirmations,
                 source_finalized_at, manual_review_note)
             VALUES (?1, 'ManualReview', ?2, 600, ?3, ?4, ?4, X'00', ?5, 100,
                     'solana', X'01', ?6, 1, 100, 'liquidity_buffer_low_at_fold')",
            rusqlite::params![
                direction,
                GROSS as i64,
                FEE as i64,
                NET as i64,
                &REQUESTER[..],
                obligation
            ],
        )
        .unwrap();
    ledger.conn_for_tests().last_insert_rowid()
}

fn inputs(sig: &str) -> ManualSolanaRefundInputs {
    ManualSolanaRefundInputs {
        batch_id: "mrb-test".to_string(),
        mint: [0xaa; 32],
        refund_wallet: [0x11; 32],
        recipient: REQUESTER,
        recipient_token_account: [0x22; 32],
        amount_atomic: MINT_UNITS,
        amount_canonical_atomic: GROSS,
        tx_signature: sig.to_string(),
        slot: 500,
        block_time: Some(NOW - 10),
        submitted_at: Some(NOW - 20),
        finalized_at: Some(NOW - 5),
    }
}

fn detail(e: LedgerError) -> String {
    match e {
        LedgerError::ManualReviewNotRecoverable { detail, .. } => detail,
        other => panic!("expected ManualReviewNotRecoverable, got {other:?}"),
    }
}

#[test]
fn a_parked_solana_sourced_request_is_eligible() {
    let l = ledger();
    let id = seed_parked(&l, "SolToGlc", 4001);
    let r = l.manual_solana_refund_eligibility(id, NOW).unwrap();
    assert_eq!(r.requester, Some(REQUESTER));
    assert_eq!(l.manual_review_request_ids().unwrap(), vec![id]);
    let id2 = seed_parked(&l, "SolToRhn", 4002);
    l.manual_solana_refund_eligibility(id2, NOW).unwrap();
}

#[test]
fn robinhood_and_goldcoin_sourced_requests_are_refused() {
    let l = ledger();
    for (direction, chain, obligation) in
        [("RhnToSol", "Robinhood", 7), ("RhnToGlc", "Robinhood", 8)]
    {
        l.conn_for_tests()
            .execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                     net_amount_atomic, net_destination_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index, source_confirmations)
                 VALUES (?1, 'ManualReview', 100, 300, 3, 97, 97, X'00', 100, 'robinhood',
                         ?2, ?3, 1)",
                rusqlite::params![direction, &[0xbau8; 20][..], obligation],
            )
            .unwrap();
        let id = l.conn_for_tests().last_insert_rowid();
        let d = detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err());
        assert!(d.contains("not Solana-sourced") && d.contains(chain), "{d}");
    }
    l.conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at,
                 source_chain, source_confirmations)
             VALUES ('GlcToSol', 'ManualReview', 100, 300, 3, 97, 97, X'00', 100, 'goldcoin', 1)",
            [],
        )
        .unwrap();
    let id = l.conn_for_tests().last_insert_rowid();
    let d = detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err());
    assert!(d.contains("Goldcoin"), "{d}");
}

#[test]
fn every_ledger_side_refusal_fires() {
    let l = ledger();
    // Not ManualReview.
    let id = seed_parked(&l, "SolToGlc", 1);
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'SourceFinalized' WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(
        detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err())
            .contains("not ManualReview")
    );
    // Missing requester.
    let id = seed_parked(&l, "SolToGlc", 2);
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET requester = NULL WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(
        detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err()).contains("no requester")
    );
    // Destination txid.
    let id = seed_parked(&l, "SolToGlc", 3);
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET destination_txid = X'aa' WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(
        detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err())
            .contains("destination transaction")
    );
    // A payout row of ANY state.
    let id = seed_parked(&l, "SolToGlc", 4);
    l.conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, confirmations)
             VALUES (?1, X'00', 1, 0, 0, X'00', 'Built', 100, 0)",
            [id],
        )
        .unwrap();
    assert!(
        detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err())
            .contains("Goldcoin payout row")
    );
    // An in-band refund lifecycle.
    let id = seed_parked(&l, "SolToGlc", 5);
    l.conn_for_tests()
        .execute(
            "INSERT INTO solana_refunds (request_id, obligation_index, nonce, amount_solana_atomic,
                requester, destination_token_account, reserve_mint, token_program,
                manual_review_reason, note, created_by, state, created_at)
             VALUES (?1, 5, 5, 1, X'00', X'00', X'00', X'00', 'x', 'n', 'c', 'Pending', 1)",
            [id],
        )
        .unwrap();
    assert!(
        detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err())
            .contains("refund lifecycle")
    );
    // PROCESS decision.
    let id = seed_parked(&l, "SolToGlc", 6);
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET operator_decision = 'process', operator_decision_at = 1,
                operator_note = 'pay' WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err()).contains("PROCESS"));
    // Rapid-burst hold before review_after.
    let id = seed_parked(&l, "SolToGlc", 7);
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET manual_review_disposition = 'rapid_burst_hold',
                hold_reason = 'rapid_burst:x', held_by = 'system', hold_started_at = ?2,
                review_after = ?3 WHERE id = ?1",
            rusqlite::params![id, NOW - 10, NOW + 100],
        )
        .unwrap();
    assert!(
        detail(l.manual_solana_refund_eligibility(id, NOW).unwrap_err())
            .contains("rapid-burst hold")
    );
    // ...and eligible once it has elapsed (an operator hold needs no decision).
    l.manual_solana_refund_eligibility(id, NOW + 101).unwrap();
    let id = seed_parked(&l, "SolToGlc", 8);
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET manual_review_disposition = 'operator_hold',
                hold_reason = 'operator_hold', held_by = 'cli:x', hold_started_at = 1,
                auto_resume_hold_note = 'INDEFINITE' WHERE id = ?1",
            [id],
        )
        .unwrap();
    l.manual_solana_refund_eligibility(id, NOW).unwrap();
}

#[test]
fn record_writes_row_and_closure_atomically_and_is_idempotent() {
    let mut l = ledger();
    let id = seed_parked(&l, "SolToGlc", 4001);
    let outcome = l
        .record_manual_solana_refund(id, &inputs("sig1"), "backlog batch", "cli:op", NOW)
        .unwrap();
    let (row, closure) = match outcome {
        ManualRefundRecordOutcome::Recorded(r, c) => (r, c),
        other => panic!("{other:?}"),
    };
    assert_eq!(row.request_id, id);
    assert_eq!(row.tx_signature, "sig1");
    assert_eq!(row.amount_atomic, MINT_UNITS);
    assert_eq!(row.imported_by, "cli:op");
    assert_eq!(closure.disposition, ClosureDisposition::RefundedOutOfBand);
    assert_eq!(closure.reference, "sig1");
    assert_eq!(closure.from_state, RequestState::ManualReview);
    let req = l.get_request(id).unwrap().unwrap();
    assert_eq!(req.state, RequestState::Closed);
    assert_eq!(l.get_manual_solana_refund(id).unwrap(), Some(row.clone()));
    assert_eq!(
        l.get_manual_solana_refund_by_signature("sig1").unwrap(),
        Some(row.clone())
    );
    assert_eq!(l.list_manual_solana_refunds(10).unwrap(), vec![row.clone()]);

    // Same signature again: no-op.
    let again = l
        .record_manual_solana_refund(id, &inputs("sig1"), "again", "cli:op", NOW + 1)
        .unwrap();
    assert_eq!(again, ManualRefundRecordOutcome::AlreadyRecorded(row));
    assert_eq!(l.list_manual_solana_refunds(10).unwrap().len(), 1);
    assert_eq!(l.request_closures(10).unwrap().len(), 1);

    // A different signature for the same request: refused.
    let d = detail(
        l.record_manual_solana_refund(id, &inputs("sig2"), "again", "cli:op", NOW)
            .unwrap_err(),
    );
    assert!(d.contains("DIFFERENT signature"), "{d}");

    // The same signature for another request: refused, nothing written.
    let id2 = seed_parked(&l, "SolToGlc", 4002);
    let d = detail(
        l.record_manual_solana_refund(id2, &inputs("sig1"), "again", "cli:op", NOW)
            .unwrap_err(),
    );
    assert!(d.contains("one transaction proves one refund"), "{d}");
    assert_eq!(
        l.get_request(id2).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert!(l.request_closure(id2).unwrap().is_none());
}

#[test]
fn record_refuses_mismatched_inputs_and_ineligible_rows_without_writing() {
    let mut l = ledger();
    let id = seed_parked(&l, "SolToGlc", 4001);
    let mut bad = inputs("sigA");
    bad.recipient = [0x99; 32];
    let d = detail(
        l.record_manual_solana_refund(id, &bad, "n", "cli:op", NOW)
            .unwrap_err(),
    );
    assert!(d.contains("not this request's requester"), "{d}");
    let mut bad = inputs("sigA");
    bad.amount_canonical_atomic = GROSS - 1;
    let d = detail(
        l.record_manual_solana_refund(id, &bad, "n", "cli:op", NOW)
            .unwrap_err(),
    );
    assert!(d.contains("gross deposit"), "{d}");
    // Already closed some other way: refused.
    l.close_manual_review(
        id,
        ClosureDisposition::ReconciledToChain,
        "chain-tx",
        "n",
        "cli:op",
        NOW,
    )
    .unwrap();
    let d = detail(
        l.record_manual_solana_refund(id, &inputs("sigA"), "n", "cli:op", NOW)
            .unwrap_err(),
    );
    assert!(d.contains("not ManualReview"), "{d}");
    assert!(l.get_manual_solana_refund(id).unwrap().is_none());
    assert!(l.list_manual_solana_refunds(10).unwrap().is_empty());
}
