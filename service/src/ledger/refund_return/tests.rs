//! Returning a never-broadcast in-band refund to ManualReview — on the
//! exact production shapes of requests 4140 and 4185 (ledger snapshot
//! 2026-09-14 00:56 UTC), every refusal, idempotency, and the audit /
//! state-log trail.

use super::*;
use crate::admin_api::audited_refund_return_to_manual_review;
use crate::ledger::{AdminAuditFilter, ManualReviewDisposition};

const GROSS: i64 = 5_000_000_000_000;
const FEE: i64 = 300_000_000_000;
const NET: i64 = 4_700_000_000_000;
const MINT_UNITS: i64 = 50_000_000_000;
const NOW: i64 = 1_789_400_000;
const HOLD_NOTE: &str = "abusive automated submission: 50,000 GLC at per_transfer_limit, \
                         rotating source and destination per order; wallets reused from the \
                         2026-09-08 flood";
const REFUND_NOTE: &str = "TOS violation: automated submission at per_transfer_limit, rotating \
                           addresses";

/// Exact production fixture (id, obligation, requester, park reason,
/// created_at, refund-begin time).
struct Fixture {
    id: i64,
    obligation: i64,
    requester: [u8; 32],
    reason: &'static str,
    created_at: i64,
    refund_at: i64,
}

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn fixture_4140() -> Fixture {
    Fixture {
        id: 4140,
        obligation: 4017,
        requester: hex32("62E52380604B9B13967A93973950AD309F1B66058CFBA58272F3D6F1514FA457"),
        reason: "reserve_paused_at_fold",
        created_at: 1_789_241_040,
        refund_at: 1_789_299_426,
    }
}

fn fixture_4185() -> Fixture {
    Fixture {
        id: 4185,
        obligation: 4062,
        requester: hex32("68EE5AB959385BFAB1A6D846BF5CA6956AA34A9E19925F0F43FEDD287CADA001"),
        reason: "liquidity_buffer_low_at_fold",
        created_at: 1_789_246_902,
        refund_at: 1_789_299_444,
    }
}

/// Seeds the request row, the operator hold, the `solana_refunds`
/// Pending row and the state log exactly as production carries them.
fn seed(ledger: &Ledger, f: &Fixture) {
    let c = ledger.conn_for_tests();
    c.execute(
        "INSERT INTO bridge_requests
            (id, direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
             net_amount_atomic, net_destination_atomic, recipient, requester, created_at,
             source_chain, source_contract, source_obligation_index, source_confirmations,
             source_finalized_at, manual_review_note, manual_review_disposition, hold_reason,
             held_by, hold_started_at, auto_resume_hold_note)
         VALUES (?1, 'SolToGlc', 'RefundPending', ?2, 600, ?3, ?4, ?4, X'00', ?5, ?6,
                 'solana', X'01', ?7, 1, ?6, ?8, 'operator_hold', 'operator_hold', 'cli:root',
                 1789261636, ?9)",
        rusqlite::params![
            f.id,
            GROSS,
            FEE,
            NET,
            &f.requester[..],
            f.created_at,
            f.obligation,
            f.reason,
            HOLD_NOTE
        ],
    )
    .unwrap();
    let nonce = Ledger::solana_refund_nonce(f.id).unwrap() as i64;
    c.execute(
        "INSERT INTO solana_refunds
            (request_id, obligation_index, nonce, amount_solana_atomic, requester,
             destination_token_account, reserve_mint, token_program, manual_review_reason, note,
             created_by, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'cli:root', 'Pending', ?11)",
        rusqlite::params![
            f.id,
            f.obligation,
            nonce,
            MINT_UNITS,
            &f.requester[..],
            &[0x22u8; 32][..],
            &[0xaau8; 32][..],
            &[0xbbu8; 32][..],
            f.reason,
            REFUND_NOTE,
            f.refund_at
        ],
    )
    .unwrap();
    for (from, to, at, reason, actor) in [
        (
            None,
            "ManualReview",
            f.created_at,
            "retroactive_fold_sol_deposit",
            "system",
        ),
        (
            Some("ManualReview"),
            "ManualReview",
            1_789_261_636,
            "auto_resume_hold",
            "cli:root",
        ),
        (
            Some("ManualReview"),
            "ManualReview",
            1_789_295_226,
            "auto_resume_hold",
            "cli:root",
        ),
        (
            Some("ManualReview"),
            "RefundPending",
            f.refund_at,
            REFUND_NOTE,
            "cli:root",
        ),
    ] {
        c.execute(
            "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![f.id, from, to, at, reason, actor],
        )
        .unwrap();
    }
}

fn proof_for(ledger: &Ledger, id: i64) -> RefundReturnChainProof {
    let refund = ledger.get_solana_refund(id).unwrap().unwrap();
    RefundReturnChainProof {
        request_id: id,
        nonce: refund.nonce,
        nonce_pda_absent: true,
        obligation_pending: true,
        checked_at: NOW,
    }
}

fn assert_safe(ledger: &Ledger, f: &Fixture) {
    match ledger.refund_return_verdict(f.id).unwrap() {
        RefundReturnVerdict::SafeToReturn { request, refund } => {
            assert_eq!(request.id, f.id);
            assert_eq!(request.state, RequestState::RefundPending);
            assert_eq!(refund.obligation_index, f.obligation as u64);
            assert_eq!(refund.state, SolanaRefundState::Pending);
            assert!(refund.refund_signature.is_none());
        }
        other => panic!("expected SafeToReturn for {}, got {other:?}", f.id),
    }
}

fn assert_refused(ledger: &Ledger, id: i64, needle: &str) {
    match ledger.refund_return_verdict(id).unwrap() {
        RefundReturnVerdict::Refused(r) => assert!(r.contains(needle), "{id}: {r}"),
        other => panic!("expected Refused({needle}) for {id}, got {other:?}"),
    }
}

#[test]
fn exact_4140_and_4185_fixtures_are_safe_to_return() {
    let l = Ledger::open_in_memory().unwrap();
    for f in [fixture_4140(), fixture_4185()] {
        seed(&l, &f);
        assert_safe(&l, &f);
    }
}

#[test]
fn execute_returns_4140_and_4185_retires_the_lifecycle_and_is_idempotent() {
    let mut l = Ledger::open_in_memory().unwrap();
    for f in [fixture_4140(), fixture_4185()] {
        seed(&l, &f);
    }
    for f in [fixture_4140(), fixture_4185()] {
        let proof = proof_for(&l, f.id);
        let (outcome, receipt) = audited_refund_return_to_manual_review(
            &mut l,
            f.id,
            &proof,
            "return to ManualReview for the out-of-band refund batch",
            "cli:root",
        )
        .unwrap();
        let retired = match outcome {
            RefundReturnOutcome::Returned(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(retired.request_id, f.id);
        assert_eq!(retired.obligation_index, f.obligation as u64);
        assert_eq!(retired.nonce, Ledger::solana_refund_nonce(f.id).unwrap());
        assert_eq!(retired.amount_solana_atomic, MINT_UNITS as u64);
        assert_eq!(retired.retired_by, "cli:root");
        assert_eq!(retired.retire_reason, OUT_OF_BAND_REFUND_RECOVERY);
        assert_eq!(receipt.action, "refund_return_to_manual_review");

        // The request: ManualReview, hold/disposition/park reason untouched.
        let req = l.get_request(f.id).unwrap().unwrap();
        assert_eq!(req.state, RequestState::ManualReview);
        assert_eq!(
            req.manual_review_disposition,
            ManualReviewDisposition::OperatorHold
        );
        assert_eq!(req.manual_review_note.as_deref(), Some(f.reason));
        assert_eq!(req.auto_resume_hold_note.as_deref(), Some(HOLD_NOTE));
        assert_eq!(req.requester, Some(f.requester));
        assert_eq!(req.source_obligation_index, Some(f.obligation as u64));
        assert!(req.is_held());
        // The lifecycle: gone from solana_refunds, kept verbatim in retired.
        assert!(l.get_solana_refund(f.id).unwrap().is_none());
        let (note, created_by, state, sig, created_at, reason): (
            String,
            String,
            String,
            Option<String>,
            i64,
            String,
        ) = l
            .conn_for_tests()
            .query_row(
                "SELECT note, created_by, state, refund_signature, created_at, manual_review_reason
                   FROM solana_refunds_retired WHERE request_id = ?1",
                [f.id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(note, REFUND_NOTE);
        assert_eq!(created_by, "cli:root");
        assert_eq!(state, "Pending");
        assert!(sig.is_none());
        assert_eq!(created_at, f.refund_at);
        assert_eq!(reason, f.reason);
        // The state log: the full history plus one new entry with the
        // mandated reason.
        let log = l.state_log(f.id).unwrap();
        assert_eq!(log.len(), 5);
        let last = log.last().unwrap();
        assert_eq!(last.0, Some(RequestState::RefundPending));
        assert_eq!(last.1, RequestState::ManualReview);
        assert_eq!(last.3.as_deref(), Some(OUT_OF_BAND_REFUND_RECOVERY));
        assert_eq!(log[3].1, RequestState::RefundPending, "history preserved");
        // The audit row.
        let audit = l.list_admin_audit(&AdminAuditFilter::default()).unwrap();
        let row = audit.iter().find(|r| r.id == receipt.audit_id).unwrap();
        assert_eq!(row.action, "refund_return_to_manual_review");
        assert_eq!(row.target.as_deref(), Some(f.id.to_string().as_str()));
        assert_eq!(
            row.old_value.as_deref(),
            Some("state=RefundPending refund_lifecycle=Pending")
        );
        let new = row.new_value.as_deref().unwrap();
        assert!(new.contains("state=ManualReview"), "{new}");
        assert!(new.contains(OUT_OF_BAND_REFUND_RECOVERY), "{new}");
        assert!(new.contains("nonce_pda_absent=true"), "{new}");
        assert_eq!(
            row.note,
            "return to ManualReview for the out-of-band refund batch"
        );

        // Now eligible for the out-of-band export.
        l.manual_solana_refund_eligibility(f.id, NOW).unwrap();

        // Second run: ALREADY_RETURNED, zero writes.
        match l.refund_return_verdict(f.id).unwrap() {
            RefundReturnVerdict::AlreadyReturned { retired: r, .. } => assert_eq!(r, retired),
            other => panic!("{other:?}"),
        }
        let audits_before = l
            .list_admin_audit(&AdminAuditFilter::default())
            .unwrap()
            .len();
        let (again, _) =
            audited_refund_return_to_manual_review(&mut l, f.id, &proof, "again", "cli:root")
                .unwrap();
        assert_eq!(again, RefundReturnOutcome::AlreadyReturned(retired));
        assert_eq!(l.state_log(f.id).unwrap().len(), 5);
        let retired_rows: i64 = l
            .conn_for_tests()
            .query_row(
                "SELECT COUNT(*) FROM solana_refunds_retired WHERE request_id = ?1",
                [f.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(retired_rows, 1);
        // (the audited wrapper still records the no-op attempt)
        assert_eq!(
            l.list_admin_audit(&AdminAuditFilter::default())
                .unwrap()
                .len(),
            audits_before + 1
        );
    }
}

#[test]
fn a_broadcast_refund_is_refused() {
    let mut l = Ledger::open_in_memory().unwrap();
    let f = fixture_4140();
    seed(&l, &f);
    l.conn_for_tests()
        .execute(
            "UPDATE solana_refunds SET state = 'Broadcast', refund_signature = 'sig',
                recent_blockhash = 'bh', broadcast_at = ?2 WHERE request_id = ?1",
            rusqlite::params![f.id, NOW],
        )
        .unwrap();
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'RefundBroadcast' WHERE id = ?1",
            [f.id],
        )
        .unwrap();
    assert_refused(&l, f.id, "not RefundPending");
    // Even with the request forced back to RefundPending, the row's
    // broadcast shape refuses.
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'RefundPending' WHERE id = ?1",
            [f.id],
        )
        .unwrap();
    assert_refused(&l, f.id, "not Pending");
    let err = l
        .return_refund_to_manual_review(f.id, &proof_for(&l, f.id), "n", "cli:root", NOW)
        .unwrap_err();
    assert!(err.to_string().contains("not Pending"), "{err}");
    assert!(l.get_solana_refund(f.id).unwrap().is_some());
}

#[test]
fn a_state_log_that_ever_saw_the_network_is_refused() {
    let l = Ledger::open_in_memory().unwrap();
    let f = fixture_4185();
    seed(&l, &f);
    l.conn_for_tests()
        .execute(
            "INSERT INTO bridge_request_state_log (request_id, from_state, to_state, at, reason, actor)
             VALUES (?1, 'RefundPending', 'RefundBroadcast', ?2, 'sent', 'cli:root'),
                    (?1, 'RefundBroadcast', 'RefundPending', ?2, 'rebuilt', 'cli:root')",
            rusqlite::params![f.id, NOW],
        )
        .unwrap();
    assert_refused(&l, f.id, "RefundBroadcast/Refunded");
}

#[test]
fn a_finalized_refund_is_refused() {
    let l = Ledger::open_in_memory().unwrap();
    let f = fixture_4140();
    seed(&l, &f);
    l.conn_for_tests()
        .execute(
            "UPDATE solana_refunds SET state = 'Confirmed', refund_signature = 'sig',
                recent_blockhash = 'bh', broadcast_at = ?2, confirmed_at = ?2 WHERE request_id = ?1",
            rusqlite::params![f.id, NOW],
        )
        .unwrap();
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'Refunded' WHERE id = ?1",
            [f.id],
        )
        .unwrap();
    assert_refused(&l, f.id, "not RefundPending");
}

#[test]
fn payout_closure_competing_settlement_and_wrong_state_are_refused() {
    let mut l = Ledger::open_in_memory().unwrap();
    let f = fixture_4140();
    seed(&l, &f);
    // Payout row.
    l.conn_for_tests()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, confirmations)
             VALUES (?1, X'00', 1, 0, 0, X'00', 'Built', 100, 0)",
            [f.id],
        )
        .unwrap();
    assert_refused(&l, f.id, "Goldcoin payout row");
    l.conn_for_tests()
        .execute("DELETE FROM goldcoin_payouts WHERE request_id = ?1", [f.id])
        .unwrap();
    // Destination txid.
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET destination_txid = X'aa' WHERE id = ?1",
            [f.id],
        )
        .unwrap();
    assert_refused(&l, f.id, "destination transaction");
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET destination_txid = NULL WHERE id = ?1",
            [f.id],
        )
        .unwrap();
    assert_safe(&l, &f);
    // Closure: a ManualReview row with a closure is not "already
    // returned", and a RefundPending row with one is refused.
    let g = fixture_4185();
    seed(&l, &g);
    l.conn_for_tests()
        .execute(
            "INSERT INTO request_closures
                (request_id, disposition, reference, note, actor, closed_at, from_state,
                 manual_review_disposition)
             VALUES (?1, 'reconciled_to_chain', 'tx', 'n', 'cli:x', ?2, 'ManualReview',
                     'operator_hold')",
            rusqlite::params![g.id, NOW],
        )
        .unwrap();
    assert_refused(&l, g.id, "already closed");
    // Wrong state (a plain ManualReview row with a live lifecycle is
    // inconsistent, not returnable; a Settled row is simply refused).
    l.conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET state = 'Settled' WHERE id = ?1",
            [g.id],
        )
        .unwrap();
    assert_refused(&l, g.id, "not RefundPending");
    // Unknown request.
    assert_refused(&l, 999_999, "does not exist");
    // A chain proof for another request / a negative proof is refused
    // at execute time, nothing written.
    let mut bad = proof_for(&l, f.id);
    bad.nonce_pda_absent = false;
    let err = l
        .return_refund_to_manual_review(f.id, &bad, "n", "cli:root", NOW)
        .unwrap_err();
    assert!(err.to_string().contains("chain proof"), "{err}");
    let mut other = proof_for(&l, f.id);
    other.request_id = g.id;
    let err = l
        .return_refund_to_manual_review(f.id, &other, "n", "cli:root", NOW)
        .unwrap_err();
    assert!(err.to_string().contains("chain proof"), "{err}");
    assert_eq!(
        l.get_request(f.id).unwrap().unwrap().state,
        RequestState::RefundPending
    );
    assert!(l.get_solana_refund(f.id).unwrap().is_some());
}
