//! Submitter and nonce-manager tests.
//!
//! The nonce rules are the ones the whole restart story rests on, so
//! they are tested against the real ledger rather than against a model of
//! it: every allocation goes through the same write transaction and the
//! same unique index the production path uses.

use super::*;
use crate::evm::TxEnvelope;
use crate::ledger::{BeginTxOutcome, NewRobinhoodTx, RobinhoodTxKind, RobinhoodTxState};
use crate::robinhood::testkit::{signer_addresses, submitter_key, MockNode, SendBehaviour};
use crate::routes::Route;

fn ledger() -> Ledger {
    Ledger::open_in_memory().expect("an in-memory ledger")
}

/// One `bridge_requests` row a Robinhood operation can hang off.
///
/// The obligation index is derived from the row count, so repeated calls
/// produce DISTINCT durable identities — the v21/v23 replay guard is
/// real, and a helper that reused one would trip it rather than seed a
/// second request.
fn seed_request(ledger: &Ledger, direction: &str) -> i64 {
    let next: i64 = ledger
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .expect("counts existing rows");
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests
                (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                 net_amount_atomic, net_destination_atomic, recipient, created_at, source_chain,
                 source_contract, source_obligation_index)
             VALUES (?1, 'SourceFinalized', 1000, 300, 30, 970, 970, X'ab', 100, 'robinhood',
                     X'1111111111111111111111111111111111111111', ?2)",
            rusqlite::params![direction, next],
        )
        .expect("seeds a request");
    ledger.conn_for_tests().last_insert_rowid()
}

fn new_tx(request_id: i64, kind: RobinhoodTxKind, tag: u8) -> NewRobinhoodTx {
    let (route, obligation_index, recipient, amount) = match kind {
        RobinhoodTxKind::Payout => (Route::GlcToRhn, None, Some([0xec; 20]), Some([1u8; 32])),
        RobinhoodTxKind::Settlement => (Route::RhnToGlc, Some(u64::from(tag)), None, None),
        RobinhoodTxKind::Refund => (
            Route::RhnToGlc,
            Some(u64::from(tag)),
            Some([0xd0; 20]),
            Some([2u8; 32]),
        ),
    };
    NewRobinhoodTx {
        kind,
        request_id,
        route,
        bridge_contract: [0x11; 20],
        chain_id: 4663,
        contract_request_id: [tag; 32],
        obligation_index,
        recipient,
        amount_robinhood: amount,
        signer_epoch: 7,
        expiry: 1_800_000_000,
        auth_digest: [tag ^ 0x5a; 32],
    }
}

/// Drives one operation as far as `Authorized`, which is the state a
/// nonce may be allocated from.
fn authorized(ledger: &mut Ledger, request_id: i64, kind: RobinhoodTxKind, tag: u8) -> i64 {
    let id = match ledger.begin_robinhood_tx(&new_tx(request_id, kind, tag), 100) {
        Ok(BeginTxOutcome::Created { id }) | Ok(BeginTxOutcome::Exists { id }) => id,
        Err(e) => panic!("begin: {e}"),
    };
    ledger
        .record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
            100,
        )
        .expect("records a quorum");
    id
}

const SUBMITTER: [u8; 20] = [0x5b; 20];

// ---------------------------------------------------- nonce allocation --

#[test]
fn two_simultaneous_operations_get_different_nonces() {
    let mut ledger = ledger();
    let r1 = seed_request(&ledger, "GlcToRhn");
    let r2 = seed_request(&ledger, "GlcToRhn");
    let a = authorized(&mut ledger, r1, RobinhoodTxKind::Payout, 1);
    let b = authorized(&mut ledger, r2, RobinhoodTxKind::Payout, 2);

    let na = ledger
        .allocate_robinhood_nonce(a, SUBMITTER, 4663, 100)
        .unwrap();
    let nb = ledger
        .allocate_robinhood_nonce(b, SUBMITTER, 4663, 100)
        .unwrap();
    assert_eq!((na, nb), (0, 1), "nonces must be distinct and sequential");
}

#[test]
fn re_allocating_an_operations_nonce_returns_the_same_one() {
    // THE restart property: after a crash at any point, the same
    // operation re-derives the SAME nonce and is re-broadcast rather than
    // duplicated.
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);

    let first = ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    for _ in 0..5 {
        assert_eq!(
            ledger
                .allocate_robinhood_nonce(id, SUBMITTER, 4663, 200)
                .unwrap(),
            first,
            "re-allocation must never hand out a second nonce"
        );
    }
}

#[test]
fn the_pending_count_is_a_floor_and_never_moves_an_allocation_backwards() {
    let mut ledger = ledger();

    // A chain that has already moved past nonce 41 — another process, or
    // a ledger restored from a backup. The next allocation must clear it.
    ledger
        .record_evm_submitter_nonce(SUBMITTER, 4663, 42, 100)
        .unwrap();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    assert_eq!(
        ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap(),
        42,
        "the observed pending count is a floor"
    );

    // A LAGGING replica answering with a stale count must not make the
    // service believe an already-used nonce is free again.
    let highest = ledger
        .record_evm_submitter_nonce(SUBMITTER, 4663, 5, 200)
        .unwrap();
    assert_eq!(highest, 42, "the cursor is monotonic");
    let r2 = seed_request(&ledger, "GlcToRhn");
    let id2 = authorized(&mut ledger, r2, RobinhoodTxKind::Payout, 2);
    assert_eq!(
        ledger
            .allocate_robinhood_nonce(id2, SUBMITTER, 4663, 200)
            .unwrap(),
        43,
        "a stale observation must never rewind the allocator"
    );
}

#[test]
fn the_database_refuses_two_operations_sharing_a_nonce() {
    // Not a code convention — a unique index. A bug in the allocator
    // produces a constraint violation, never a second transfer.
    let mut ledger = ledger();
    let r1 = seed_request(&ledger, "GlcToRhn");
    let r2 = seed_request(&ledger, "GlcToRhn");
    let a = authorized(&mut ledger, r1, RobinhoodTxKind::Payout, 1);
    let b = authorized(&mut ledger, r2, RobinhoodTxKind::Payout, 2);
    ledger
        .allocate_robinhood_nonce(a, SUBMITTER, 4663, 100)
        .unwrap();
    ledger
        .allocate_robinhood_nonce(b, SUBMITTER, 4663, 100)
        .unwrap();

    let forced = ledger.conn_for_tests().execute(
        "UPDATE robinhood_transactions SET nonce = 0 WHERE id = ?1",
        [b],
    );
    assert!(
        forced.is_err(),
        "the unique index must refuse two operations under one nonce"
    );
}

#[test]
fn a_nonce_allocated_for_a_different_submitter_is_refused() {
    // A ledger restored alongside a rotated key: its nonce sequence
    // belongs to an account this process no longer controls.
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    let other = [0x99u8; 20];
    assert!(matches!(
        ledger.allocate_robinhood_nonce(id, other, 4663, 100),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
}

#[test]
fn a_nonce_cannot_be_allocated_before_a_quorum_exists() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = match ledger
        .begin_robinhood_tx(&new_tx(r, RobinhoodTxKind::Payout, 1), 100)
        .unwrap()
    {
        BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
    };
    assert!(matches!(
        ledger.allocate_robinhood_nonce(id, SUBMITTER, 4663, 100),
        Err(LedgerError::RobinhoodTxWrongState {
            expected: RobinhoodTxState::Authorized,
            actual: RobinhoodTxState::Authorizing,
            ..
        })
    ));
}

#[test]
fn a_nonce_may_only_be_drawn_from_the_chain_the_operation_was_authorized_for() {
    // A nonce sequence belongs to one account on one chain. Allocating
    // against a different chain than the row was authorized for would
    // take a nonce out of one sequence and record it under another — the
    // exact way two operations end up sharing one.
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    assert!(matches!(
        ledger.allocate_robinhood_nonce(id, SUBMITTER, 46630, 100),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
    // The chain it WAS authorized for works normally.
    assert_eq!(
        ledger
            .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
            .unwrap(),
        0
    );
}

// -------------------------------------------------- persistence order --

#[test]
fn signed_bytes_cannot_be_stored_before_a_nonce_is_allocated() {
    // The ordering invariant is a database CHECK, not a convention.
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    let result = ledger.record_robinhood_signed(
        id,
        "eip1559",
        150_000,
        "fees",
        &[0xde, 0xad],
        [0xbe; 32],
        100,
    );
    assert!(
        result.is_err(),
        "signing before allocating a nonce must be refused by the schema"
    );
}

#[test]
fn re_signing_the_same_bytes_is_idempotent_and_different_bytes_are_refused() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1, 2, 3], [0xaa; 32], 100)
        .unwrap();
    // Identical bytes: a no-op.
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1, 2, 3], [0xaa; 32], 200)
        .unwrap();
    // Different bytes: refused, because a replacement must be COUNTED.
    assert!(matches!(
        ledger.record_robinhood_signed(id, "eip1559", 150_000, "f", &[9], [0xbb; 32], 200),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
}

#[test]
fn a_replacement_keeps_the_nonce_and_increments_its_own_counter() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    let nonce = ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f0", &[1], [0xaa; 32], 100)
        .unwrap();
    ledger.record_robinhood_broadcast(id, 100).unwrap();
    ledger
        .record_robinhood_replacement(id, "f1", &[2], [0xbb; 32], 200)
        .unwrap();

    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(row.nonce, Some(nonce), "a replacement keeps the SAME nonce");
    assert_eq!(row.replacement_attempts, 1);
    assert_eq!(row.raw_tx.as_deref(), Some(&[2u8][..]));
    assert_eq!(row.tx_hash, Some([0xbb; 32]));
}

// -------------------------------------------------------- receipts ----

#[test]
fn a_reverted_receipt_is_terminal_and_never_reallocates() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    let nonce = ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
        .unwrap();
    ledger.record_robinhood_broadcast(id, 100).unwrap();

    let state = ledger
        .record_robinhood_receipt(id, false, 500, [0xcc; 32], 200)
        .unwrap();
    assert_eq!(state, RobinhoodTxState::Reverted);

    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(row.receipt_status, Some(0));
    assert_eq!(
        row.nonce,
        Some(nonce),
        "the nonce is not released or reused"
    );
    assert!(row.state.is_terminal());
    assert!(
        row.failure_reason.as_deref().unwrap().contains("REVERTED"),
        "the reason must say plainly what happened"
    );
}

#[test]
fn a_successful_receipt_reaches_included_and_only_finalizes_at_depth() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
        .unwrap();
    ledger.record_robinhood_broadcast(id, 100).unwrap();
    assert_eq!(
        ledger
            .record_robinhood_receipt(id, true, 500, [0xcc; 32], 200)
            .unwrap(),
        RobinhoodTxState::Included
    );

    // Below the threshold: still Included.
    assert!(!ledger
        .update_robinhood_confirmations(id, 2, 3, 300)
        .unwrap());
    assert_eq!(
        ledger.get_robinhood_tx(id).unwrap().unwrap().state,
        RobinhoodTxState::Included
    );
    // At the threshold: fires exactly once.
    assert!(ledger
        .update_robinhood_confirmations(id, 3, 3, 400)
        .unwrap());
    assert!(!ledger
        .update_robinhood_confirmations(id, 9, 3, 500)
        .unwrap());
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert_eq!(row.state, RobinhoodTxState::Finalized);
    assert_eq!(row.finalized_at, Some(400));
    assert_eq!(row.confirmations, 9, "depth keeps being refreshed");
}

#[test]
fn a_receipt_that_moves_block_is_a_post_inclusion_contradiction() {
    // Never reconciled automatically at any depth: the two facts cannot
    // both be true, and choosing between them is a human's decision.
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
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

    let state = ledger
        .record_robinhood_receipt(id, true, 501, [0xdd; 32], 300)
        .unwrap();
    assert_eq!(state, RobinhoodTxState::ManualReview);
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert!(row.failure_reason.unwrap().contains("contradicted"));
}

#[test]
fn a_finalized_operation_can_never_be_moved_to_manual_review() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
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
    assert!(matches!(
        ledger.mark_robinhood_tx_manual_review(id, "second thoughts", 400),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
}

// ------------------------------------------------------ authorization --

#[test]
fn a_quorum_is_exactly_two_distinct_signers() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = match ledger
        .begin_robinhood_tx(&new_tx(r, RobinhoodTxKind::Payout, 1), 100)
        .unwrap()
    {
        BeginTxOutcome::Created { id } | BeginTxOutcome::Exists { id } => id,
    };

    // One is not a quorum.
    assert!(ledger
        .record_robinhood_authorization(id, &[([0xa1; 20], [0x11; 65])], 100)
        .is_err());
    // Three is not the quorum the contract accepts either.
    assert!(ledger
        .record_robinhood_authorization(
            id,
            &[
                ([0xa1; 20], [0x11; 65]),
                ([0xa2; 20], [0x22; 65]),
                ([0xa3; 20], [0x33; 65])
            ],
            100
        )
        .is_err());
    // Two from ONE signer is a quorum of one.
    assert!(ledger
        .record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x11; 65]), ([0xa1; 20], [0x22; 65])],
            100
        )
        .is_err());
    // Two distinct: accepted.
    ledger
        .record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
            100,
        )
        .unwrap();
}

#[test]
fn a_second_different_authorization_is_refused_and_the_same_one_is_a_no_op() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    // Identical: no-op.
    ledger
        .record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x11; 65]), ([0xa2; 20], [0x22; 65])],
            200,
        )
        .unwrap();
    // Different: refused — one operation is authorized once.
    assert!(matches!(
        ledger.record_robinhood_authorization(
            id,
            &[([0xa1; 20], [0x99; 65]), ([0xa3; 20], [0x88; 65])],
            200
        ),
        Err(LedgerError::RobinhoodTxInvalid { .. })
    ));
}

// ------------------------------------------------------- duplication --

#[test]
fn one_operation_of_each_kind_per_request_ever() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "RhnToGlc");
    let first = ledger
        .begin_robinhood_tx(&new_tx(r, RobinhoodTxKind::Settlement, 1), 100)
        .unwrap();
    let second = ledger
        .begin_robinhood_tx(&new_tx(r, RobinhoodTxKind::Settlement, 1), 200)
        .unwrap();
    match (first, second) {
        (BeginTxOutcome::Created { id: a }, BeginTxOutcome::Exists { id: b }) => {
            assert_eq!(a, b, "the second attempt resumes the first")
        }
        other => panic!("expected Created then Exists, got {other:?}"),
    }
}

// ------------------------------------------------------ fee behaviour --

#[test]
fn a_fee_bump_is_at_least_geths_minimum_increment_and_compounds() {
    // A replacement must offer a strictly higher fee or the node refuses
    // it as `replacement underpriced`.
    let base = 1_000_000_000u128;
    let one = bump_fee(base, 1);
    let two = bump_fee(base, 2);
    assert!(one > base, "a bump must strictly increase the fee");
    assert!(
        one >= base + base / 8,
        "at least geth's 12.5% PriceBump: {one} vs {base}"
    );
    assert!(two > one, "bumps compound");
    assert_eq!(bump_fee(base, 0), base, "no bump is no change");
    // A zero fee still increases, so a chain with a zero base fee can
    // still produce a valid replacement.
    assert!(bump_fee(0, 1) > 0);
}

#[test]
fn a_stale_broadcast_becomes_an_incident_rather_than_a_pending_transaction() {
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, SUBMITTER, 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
        .unwrap();
    ledger.record_robinhood_broadcast(id, 1_000).unwrap();
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();

    assert!(!broadcast_is_stale(&row, 1_000));
    assert!(!broadcast_is_stale(
        &row,
        1_000 + UNRESOLVED_BROADCAST_INCIDENT_SECS - 1
    ));
    assert!(broadcast_is_stale(
        &row,
        1_000 + UNRESOLVED_BROADCAST_INCIDENT_SECS
    ));
}

// ------------------------------------------------------- key loading --

#[test]
fn a_key_that_controls_a_different_address_is_refused() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    let mut config = node.settlement_config();
    config.submitter_address = crate::evm::EvmAddress::from_bytes([0x99; 20]);
    let err = Submitter::from_key(submitter_key(), &config).unwrap_err();
    assert!(matches!(err, SubmitterKeyError::AddressMismatch { .. }));
}

#[test]
fn the_configured_key_loads_and_reports_its_address() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    assert_eq!(submitter.address(), submitter_key().address());
    // The submitter is never one of the authorization signers.
    assert!(!signer_addresses().contains(&submitter.address()));
    // And its Debug never renders key material.
    let rendered = format!("{submitter:?}");
    assert!(rendered.contains("address"), "{rendered}");
    assert!(!rendered.contains("key"), "{rendered}");
}

// ------------------------------------------------- envelope agreement --

#[tokio::test]
async fn the_configured_envelope_is_cross_checked_against_the_chains_fee_market() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);

    // eip1559 configured, chain HAS a base fee: agrees.
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    assert!(submitter.read_fees(&node, 0).await.is_ok());

    // eip1559 configured, chain has NO base fee: refused.
    node.with(|s| s.contract.base_fee = None);
    let err = submitter.read_fees(&node, 0).await.unwrap_err();
    assert!(
        matches!(
            err,
            SubmitError::EnvelopeMismatch {
                envelope: "eip1559",
                ..
            }
        ),
        "{err}"
    );

    // legacy configured, chain HAS a base fee: also refused, the other
    // way round.
    node.with(|s| s.contract.base_fee = Some(1_000));
    let mut legacy_cfg = node.settlement_config();
    legacy_cfg.tx_envelope = TxEnvelope::Legacy;
    let legacy = Submitter::from_key(submitter_key(), &legacy_cfg).unwrap();
    let err = legacy.read_fees(&node, 0).await.unwrap_err();
    assert!(
        matches!(
            err,
            SubmitError::EnvelopeMismatch {
                envelope: "legacy",
                ..
            }
        ),
        "{err}"
    );

    // legacy configured, no base fee: agrees.
    node.with(|s| s.contract.base_fee = None);
    assert!(legacy.read_fees(&node, 0).await.is_ok());
}

#[tokio::test]
async fn a_fee_above_the_configured_ceiling_refuses_to_sign() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    node.with(|s| s.contract.base_fee = Some(500_000_000_000));
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let err = submitter.read_fees(&node, 0).await.unwrap_err();
    assert!(
        matches!(err, SubmitError::FeeAboveCeiling { .. }),
        "a suggestion above the ceiling must refuse, never clamp: {err}"
    );
}

#[tokio::test]
async fn an_underfunded_submitter_is_caught_before_any_nonce_is_allocated() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    node.with(|s| s.contract.submitter_balance = 0);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let err = submitter.check_funding(&node).await.unwrap_err();
    assert!(
        matches!(err, SubmitError::SubmitterUnderfunded { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_failing_gas_estimate_is_a_dry_run_failure_not_an_outage() {
    // `eth_estimateGas` EXECUTES the call, so a revert here means the
    // real transaction would have reverted — caught before a nonce is
    // consumed.
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    node.with(|s| s.contract.gas_estimate = None);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let err = submitter
        .estimate_gas(
            &node,
            &crate::robinhood::rpc::EvmCall {
                to: crate::robinhood::testkit::BRIDGE,
                data: vec![0; 4],
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SubmitError::EstimateFailed { .. }), "{err}");
}

#[tokio::test]
async fn nonce_reconciliation_records_the_pending_count() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    node.with(|s| s.pending_nonce = 17);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let mut ledger = ledger();
    let observed = submitter
        .reconcile_nonce(&node, &mut ledger, 100)
        .await
        .unwrap();
    assert_eq!(observed, 17);
    assert_eq!(
        ledger
            .evm_submitter_nonce(submitter.address().to_bytes(), 4663)
            .unwrap()
            .map(|(n, _)| n),
        Some(17)
    );
}

#[tokio::test]
async fn every_definitive_broadcast_answer_is_classified_not_propagated() {
    // `already known` and `nonce too low` are ROUTINE — the expected
    // answers when re-broadcasting after a crash — and must never surface
    // as errors that a caller might respond to by reallocating.
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, submitter.address().to_bytes(), 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1, 2, 3], [0xaa; 32], 100)
        .unwrap();
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();

    for (behaviour, expected) in [
        (
            SendBehaviour::AlreadyKnown,
            EvmBroadcastOutcome::AlreadyKnown,
        ),
        (SendBehaviour::NonceTooLow, EvmBroadcastOutcome::NonceTooLow),
        (
            SendBehaviour::ReplacementUnderpriced,
            EvmBroadcastOutcome::ReplacementUnderpriced,
        ),
    ] {
        node.with(|s| s.send_behaviour.push_back(behaviour.clone()));
        assert_eq!(submitter.broadcast(&node, &row).await.unwrap(), expected);
    }

    // A transport failure is the one case that PROPAGATES, because the
    // question was never answered.
    node.with(|s| s.send_behaviour.push_back(SendBehaviour::Transport));
    assert!(matches!(
        submitter.broadcast(&node, &row).await,
        Err(SubmitError::Rpc { .. })
    ));
}

#[tokio::test]
async fn a_definitive_node_rejection_is_surfaced_and_never_silently_retried() {
    // Not routine, unlike `already known`: the node gave a reason, and
    // the caller must see it rather than loop.
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, submitter.address().to_bytes(), 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
        .unwrap();
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();

    node.with(|s| {
        s.send_behaviour.push_back(SendBehaviour::Reject {
            code: -32000,
            message: "intrinsic gas too low".to_string(),
        })
    });
    assert_eq!(
        submitter.broadcast(&node, &row).await.unwrap(),
        EvmBroadcastOutcome::Rejected {
            code: -32000,
            message: "intrinsic gas too low".to_string()
        }
    );
    assert_eq!(
        node.with(|s| s.broadcasts.len()),
        0,
        "a rejected send never entered the mempool"
    );
}

#[tokio::test]
async fn re_broadcasting_sends_the_identical_persisted_bytes() {
    // The property that makes an uncertain broadcast safe: whatever
    // happened to the first attempt, the second is the SAME transaction.
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, submitter.address().to_bytes(), 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[7, 7, 7], [0xaa; 32], 100)
        .unwrap();
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();

    for _ in 0..3 {
        submitter.broadcast(&node, &row).await.unwrap();
    }
    let sent: Vec<Vec<u8>> = node.with(|s| s.broadcasts.iter().map(|b| b.raw.clone()).collect());
    assert_eq!(sent.len(), 3);
    assert!(
        sent.iter().all(|raw| raw == &vec![7u8, 7, 7]),
        "every re-broadcast must be byte-identical"
    );
}

#[tokio::test]
async fn a_persisted_transaction_signed_by_another_key_is_refused() {
    let node = MockNode::new(crate::robinhood::testkit::BRIDGE);
    let submitter = Submitter::from_key(submitter_key(), &node.settlement_config()).unwrap();
    let mut ledger = ledger();
    let r = seed_request(&ledger, "GlcToRhn");
    let id = authorized(&mut ledger, r, RobinhoodTxKind::Payout, 1);
    ledger
        .allocate_robinhood_nonce(id, [0x99; 20], 4663, 100)
        .unwrap();
    ledger
        .record_robinhood_signed(id, "eip1559", 150_000, "f", &[1], [0xaa; 32], 100)
        .unwrap();
    let row = ledger.get_robinhood_tx(id).unwrap().unwrap();
    assert!(matches!(
        submitter.broadcast(&node, &row).await,
        Err(SubmitError::SubmitterChanged { .. })
    ));
}
