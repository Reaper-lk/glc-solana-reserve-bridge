//! The observation store's guarantees: durable identity, three-way
//! idempotency, cursor atomicity, finality promotion, reorg tombstoning,
//! and the halt.

use super::*;
use crate::ledger::Ledger;
use crate::routes::Route;

const CONTRACT: [u8; 20] = [0x11; 20];
const OTHER_CONTRACT: [u8; 20] = [0x99; 20];

fn ledger() -> Ledger {
    Ledger::open_in_memory().expect("in-memory ledger")
}

fn hash(seed: u8) -> [u8; 32] {
    [seed; 32]
}

fn observation(index: u64, block: u64) -> RobinhoodDepositObservation {
    RobinhoodDepositObservation {
        source_contract: CONTRACT,
        obligation_index: index,
        route: Route::RhnToGlc,
        depositor: [0x33; 20],
        destination: vec![0xde, 0xad],
        amount_robinhood_atomic: {
            let mut word = [0u8; 32];
            word[16..].copy_from_slice(&(1_000_000_000_000u128).to_be_bytes());
            word
        },
        amount_canonical_atomic: 100,
        tx_hash: {
            let mut out = [0xaa; 32];
            out[..8].copy_from_slice(&index.to_be_bytes());
            out
        },
        log_index: 0,
        block_number: block,
        block_hash: hash(block as u8),
    }
}

fn apply(
    ledger: &mut Ledger,
    observations: &[RobinhoodDepositObservation],
    cursor: u64,
    now: i64,
) -> Result<RobinhoodRangeApplied, LedgerError> {
    let anchors: Vec<(u64, [u8; 32])> = observations
        .iter()
        .map(|o| (o.block_number, o.block_hash))
        .collect();
    ledger.robinhood_apply_scan_range(observations, &anchors, cursor, hash(cursor as u8), 12, now)
}

/// A brand-new ledger has no cursor. The indexer must then use its
/// configured start block — there is nothing here to fall back on, by
/// design.
#[test]
fn a_fresh_ledger_has_no_cursor() {
    assert_eq!(ledger().robinhood_scan_cursor().expect("reads"), None);
}

#[test]
fn records_an_observation_and_advances_the_cursor_together() {
    let mut ledger = ledger();
    let applied = apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");
    assert_eq!(applied.recorded, 1);
    assert_eq!(applied.already_recorded, 0);
    assert_eq!(
        ledger.robinhood_scan_cursor().expect("reads"),
        Some((100, hash(100)))
    );

    let rows = ledger.robinhood_observations().expect("reads");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].observation, observation(0, 100));
    // Never final on arrival.
    assert_eq!(rows[0].finality, RobinhoodFinality::Provisional);
    assert_eq!(rows[0].finalized_at, None);
}

/// Rescanning a range — after a restart, or because ranges overlap — must
/// be free. This is the property that makes the cursor safe to re-run.
#[test]
fn re_observing_the_identical_event_is_idempotent() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");
    let applied = apply(&mut ledger, &[observation(0, 100)], 100, 2).expect("re-applies");
    assert_eq!(applied.recorded, 0);
    assert_eq!(applied.already_recorded, 1);
    assert_eq!(ledger.robinhood_observations().expect("reads").len(), 1);
}

/// The other half of idempotency: a re-observation that DISAGREES is a
/// fact about the world no retry can fix, and is never written over.
#[test]
fn a_conflicting_re_observation_is_refused_and_names_the_field() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");

    let mut conflicting = observation(0, 100);
    conflicting.amount_canonical_atomic = 999;
    let error = apply(&mut ledger, &[conflicting], 100, 2).expect_err("refuses");
    match error {
        LedgerError::RobinhoodObservationConflict(conflict) => {
            assert_eq!(conflict.obligation_index, 0);
            assert_eq!(conflict.field, "amount_canonical_atomic");
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    // The stored row is untouched.
    let rows = ledger.robinhood_observations().expect("reads");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].observation.amount_canonical_atomic, 100);
}

/// A conflict rolls the WHOLE range back — including the cursor — so a
/// halted indexer resumes from a known point rather than from halfway
/// through a range it could not finish.
#[test]
fn a_conflict_rolls_back_the_entire_range_including_the_cursor() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");

    let mut conflicting = observation(0, 100);
    conflicting.depositor = [0x44; 20];
    let batch = [observation(1, 101), conflicting];
    apply(&mut ledger, &batch, 101, 2).expect_err("refuses");

    // Neither the good observation in the same batch nor the cursor move
    // survived.
    assert_eq!(ledger.robinhood_observations().expect("reads").len(), 1);
    assert_eq!(
        ledger.robinhood_scan_cursor().expect("reads"),
        Some((100, hash(100)))
    );
}

/// The v21 collision, closed for this table too: the same obligation
/// index under a different contract deployment is a different deposit,
/// not a duplicate.
#[test]
fn the_same_index_under_a_different_contract_is_a_different_deposit() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");

    let mut successor = observation(0, 101);
    successor.source_contract = OTHER_CONTRACT;
    successor.tx_hash = [0xbb; 32];
    let applied = apply(&mut ledger, &[successor], 101, 2).expect("applies");
    assert_eq!(applied.recorded, 1);
    assert_eq!(ledger.robinhood_observations().expect("reads").len(), 2);
}

/// Depth is `head - block + 1`, matching the Goldcoin indexer, so a
/// confirmation depth of 12 makes block 100 final exactly at head 111.
#[test]
fn promotion_uses_head_minus_block_plus_one() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");

    assert!(ledger
        .robinhood_promote_final(110, 12, 2)
        .expect("promotes")
        .is_empty());
    assert_eq!(
        ledger.robinhood_observations().expect("reads")[0].finality,
        RobinhoodFinality::Provisional
    );

    let promoted = ledger
        .robinhood_promote_final(111, 12, 3)
        .expect("promotes");
    assert_eq!(promoted, vec![0]);
    let row = &ledger.robinhood_observations().expect("reads")[0];
    assert_eq!(row.finality, RobinhoodFinality::Final);
    assert_eq!(row.finalized_at, Some(3));
}

#[test]
fn promotion_is_idempotent_and_never_re_promotes() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");
    assert_eq!(
        ledger
            .robinhood_promote_final(120, 12, 2)
            .expect("promotes"),
        vec![0]
    );
    assert!(ledger
        .robinhood_promote_final(121, 12, 3)
        .expect("promotes")
        .is_empty());
    assert_eq!(
        ledger.robinhood_observations().expect("reads")[0].finalized_at,
        Some(2)
    );
}

/// The routine case: a reorg orphans provisional sightings, which become
/// tombstones rather than disappearing.
#[test]
fn a_rollback_tombstones_provisional_observations_above_the_fork() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");
    apply(&mut ledger, &[observation(1, 105)], 105, 2).expect("applies");

    let orphaned = ledger
        .robinhood_rollback_reorg(102, hash(102), 105, hash(105), 3)
        .expect("rolls back");
    assert_eq!(orphaned, 1);

    let rows = ledger.robinhood_observations().expect("reads");
    let by_index = |i: u64| {
        rows.iter()
            .find(|r| r.observation.obligation_index == i)
            .expect("row exists")
    };
    assert_eq!(by_index(0).finality, RobinhoodFinality::Provisional);
    assert_eq!(by_index(1).finality, RobinhoodFinality::Reorged);
    assert_eq!(by_index(1).reorged_at, Some(3));
    // The cursor is back at the fork, re-anchored at its live hash.
    assert_eq!(
        ledger.robinhood_scan_cursor().expect("reads"),
        Some((102, hash(102)))
    );
}

/// A tombstone frees the durable identity, because a reorg can genuinely
/// reassign an obligation index to a different deposit.
#[test]
fn a_tombstoned_identity_can_be_claimed_again() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(1, 105)], 105, 1).expect("applies");
    ledger
        .robinhood_rollback_reorg(102, hash(102), 105, hash(105), 2)
        .expect("rolls back");

    // The same index, now a different deposit on the new chain.
    let mut replacement = observation(1, 104);
    replacement.tx_hash = [0xcc; 32];
    replacement.amount_canonical_atomic = 555;
    let applied = apply(&mut ledger, &[replacement], 104, 3).expect("applies");
    assert_eq!(applied.recorded, 1);

    let rows = ledger.robinhood_observations().expect("reads");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .filter(|r| r.finality == RobinhoodFinality::Reorged)
            .count(),
        1
    );
}

/// The incident case. A rollback that would contradict a finalized
/// observation is refused at the database layer, even though the caller
/// is expected to have caught it first.
#[test]
fn a_rollback_over_a_finalized_observation_is_refused() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 105)], 105, 1).expect("applies");
    ledger
        .robinhood_promote_final(120, 12, 2)
        .expect("promotes");

    assert_eq!(
        ledger
            .robinhood_final_observations_above(102)
            .expect("reads"),
        vec![0]
    );
    let error = ledger
        .robinhood_rollback_reorg(102, hash(102), 105, hash(105), 3)
        .expect_err("refuses");
    assert!(matches!(
        error,
        LedgerError::RobinhoodPostFinalityReorg {
            fork_block: 102,
            finalized_above: 1
        }
    ));
    // Nothing moved.
    assert_eq!(
        ledger.robinhood_observations().expect("reads")[0].finality,
        RobinhoodFinality::Final
    );
    assert_eq!(
        ledger.robinhood_scan_cursor().expect("reads"),
        Some((105, hash(105)))
    );
}

/// Anchors are pruned to a window so the table stays bounded, but the
/// highest one below the floor is kept — that anchor is what makes a
/// reorg deeper than the window DETECTABLE rather than unfalsifiable.
#[test]
fn anchor_pruning_keeps_one_anchor_below_the_retention_floor() {
    let mut ledger = ledger();
    for block in [10u64, 20, 30, 100, 105, 110] {
        ledger
            .robinhood_apply_scan_range(&[], &[], block, hash(block as u8), 12, 1)
            .expect("applies");
    }
    let anchors = ledger.robinhood_scan_anchors_desc().expect("reads");
    let blocks: Vec<u64> = anchors.iter().map(|(b, _)| *b).collect();
    // Floor is 110 - 12 = 98; everything at or above it survives, plus
    // exactly one below it.
    assert_eq!(blocks, vec![110, 105, 100, 30]);
}

#[test]
fn the_halt_is_persisted_and_the_first_reason_wins() {
    let mut ledger = ledger();
    assert_eq!(ledger.robinhood_halt().expect("reads"), None);

    ledger
        .robinhood_record_halt(RobinhoodHaltReason::ChainIdMismatch, "wrong network", 10)
        .expect("halts");
    ledger
        .robinhood_record_halt(
            RobinhoodHaltReason::ObservationConflict,
            "a later complaint",
            11,
        )
        .expect("halts again");

    let halt = ledger.robinhood_halt().expect("reads").expect("is halted");
    assert_eq!(halt.reason, RobinhoodHaltReason::ChainIdMismatch);
    assert_eq!(halt.detail, "wrong network");
    assert_eq!(halt.halted_at, 10);

    ledger.robinhood_clear_halt(12).expect("clears");
    assert_eq!(ledger.robinhood_halt().expect("reads"), None);
}

#[test]
fn the_summary_counts_every_finality_state() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");
    apply(&mut ledger, &[observation(1, 105)], 105, 2).expect("applies");
    apply(&mut ledger, &[observation(2, 106)], 106, 3).expect("applies");
    ledger
        .robinhood_promote_final(111, 12, 4)
        .expect("promotes");
    ledger
        .robinhood_rollback_reorg(105, hash(105), 106, hash(106), 5)
        .expect("rolls back");

    let summary = ledger.robinhood_observation_summary().expect("reads");
    assert_eq!(summary.finalized, 1);
    assert_eq!(summary.provisional, 1);
    assert_eq!(summary.reorged, 1);
    assert_eq!(summary.highest_finalized_block, Some(100));
}

/// v22 pinned `settled` to zero so that settling a Robinhood deposit
/// would require a migration a reviewer would see. v23 is that migration
/// (Phase F), so the column is now a real two-valued flag — and it is
/// still a CONSTRAINED one: nothing but 0 or 1 may be written to it, so a
/// stray value can never read as "settled" by accident.
#[test]
fn the_settled_flag_admits_exactly_zero_and_one() {
    let mut ledger = ledger();
    apply(&mut ledger, &[observation(0, 100)], 100, 1).expect("applies");

    for allowed in [0i64, 1] {
        ledger
            .conn_for_tests()
            .execute(
                "UPDATE robinhood_deposit_observations SET settled = ?1",
                [allowed],
            )
            .unwrap_or_else(|e| panic!("settled = {allowed} must be writable: {e}"));
    }
    for refused in [-1i64, 2, 255] {
        let error = ledger
            .conn_for_tests()
            .execute(
                "UPDATE robinhood_deposit_observations SET settled = ?1",
                [refused],
            )
            .expect_err("the CHECK must refuse it");
        assert!(
            error.to_string().to_lowercase().contains("constraint"),
            "settled = {refused} expected a constraint failure, got {error}",
        );
    }
}
