use super::*;
use crate::ledger::{CreateRequestOutcome, Direction, Ledger};

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
            0,
        )
        .unwrap();
    ledger
}

#[test]
fn matching_balance_is_within_tolerance_and_never_pauses() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        0,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn small_drop_within_tolerance_is_accepted() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        999_950,
        100,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
}

#[test]
fn unexplained_drop_beyond_tolerance_breaches_and_auto_pauses() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        100,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn balance_increase_is_never_a_breach() {
    let mut ledger = setup();
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        2_000_000,
        100,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);
}

#[test]
fn hard_invariant_breach_pauses_even_within_the_delta_tolerance() {
    // Commit the maximum reservable amount (capped by available capacity:
    // balance 1_000_000 - protected_minimum 100_000 = 900_000) so
    // protected_minimum + pending_obligations == the cached balance
    // exactly. Then simulate the LIVE chain balance having dropped below
    // that (e.g. an unauthorized external movement) — this is precisely
    // what reconciliation exists to catch, and it must fire even with a
    // huge delta tolerance, because it is not a delta check at all.
    let mut ledger = setup();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: 900_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 900_000,
                net_destination_atomic: 900_000,
            },
            &[1u8; 32],
            None,
            3600,
            0,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xAA; 32], 0, 900_000, 1, [0xBB; 32], 1)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 2).unwrap();

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        950_000,
        1_000_000,
        10,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}

#[test]
fn reconciliation_never_auto_unpauses() {
    let mut ledger = setup();
    reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        100,
        10,
    )
    .unwrap();
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
    // Balance "recovers" to the original cached value.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        100,
        20,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(
        ledger.is_paused(ReserveDirection::SolanaReserve).unwrap(),
        "reconciliation must never auto-unpause; only an operator may"
    );
}

#[test]
fn reconciliation_reports_accrued_fees_without_them_masking_a_real_breach() {
    // Credit a large accrued-fee balance directly, as real settlements
    // would over time (docs/20-bridge-fee.md). This must be surfaced for
    // audit visibility but must NEVER be treated as capacity that could
    // excuse an otherwise-real invariant breach — "do not let collected
    // fees falsely increase the amount considered available for customer
    // payouts."
    let mut ledger = setup();
    ledger
        .raw()
        .execute(
            "UPDATE reserve_ledger SET accrued_fees_atomic = 500000 WHERE direction = 'SolanaReserve'",
            [],
        )
        .unwrap();

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        0,
        100,
    )
    .unwrap();
    assert_eq!(report.accrued_fees, 500_000);
    assert_eq!(report.classification, Classification::WithinTolerance);

    // A real unexplained drop must still breach and auto-pause regardless
    // of how large the accrued-fee balance is.
    let breach_report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        100,
        200,
    )
    .unwrap();
    assert_eq!(
        breach_report.accrued_fees, 500_000,
        "accrued fees are still reported during a breach"
    );
    assert_eq!(breach_report.classification, Classification::Breach);
    assert!(breach_report.auto_paused);
}

#[test]
fn every_finding_is_recorded_including_skips() {
    let mut ledger = setup();
    reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        1_000_000,
        0,
        1,
    )
    .unwrap();
    record_skipped(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        "rpc_timeout",
        2,
    )
    .unwrap();
    let count: i64 = ledger
        .raw()
        .query_row("SELECT count(*) FROM reconciliation_findings", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(count, 2);
    let skipped: String = ledger
        .raw()
        .query_row(
            "SELECT classification FROM reconciliation_findings ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(skipped.starts_with("SKIPPED"));
}

#[test]
fn confirmed_rebalance_is_never_misclassified_as_an_unexplained_breach() {
    // Mirrors the exact rationale behind mark_release_confirmed's own
    // immediate-balance-decrement fix (docs/14-phase6-checkpoint.md bug
    // 3): an operator-authorized, confirmed rebalance is an EXPLAINED
    // balance change, and Ledger::confirm_rebalance must keep the cached
    // total_reserve_balance self-consistent with it immediately — not
    // leave the next reconciliation tick to discover a "surprise" drop
    // and misclassify routine, authorized activity as a breach.
    let mut ledger = setup();
    // Baseline cached balance is 1_000_000 (see `setup`).

    let id = ledger
        .propose_rebalance(
            ReserveDirection::SolanaReserve,
            crate::ledger::RebalanceKind::Withdraw,
            50_000,
            "sweep surplus to cold storage",
            "ops-alice",
            1,
            5,
        )
        .unwrap();
    ledger.approve_rebalance(id, "ops-alice", 6).unwrap();
    ledger
        .record_rebalance_executed(id, "sig-reconciliation-interaction", "ops-alice", 7)
        .unwrap();

    // Before confirm_rebalance: the real (post-withdrawal) balance would
    // still look like an unexplained drop against the stale cache.
    let premature = reconcile(&mut ledger, ReserveDirection::SolanaReserve, 950_000, 0, 8).unwrap();
    assert_eq!(
        premature.classification,
        Classification::Breach,
        "before confirmation, reconciliation has no way to know this drop is explained"
    );
    ledger
        .set_paused(
            ReserveDirection::SolanaReserve,
            false,
            Some("test: undo premature auto-pause"),
        )
        .unwrap();

    ledger
        .confirm_rebalance(id, 50_000, "ops-alice", 9)
        .unwrap();

    // After confirm_rebalance: the same real balance now reconciles cleanly.
    let report = reconcile(&mut ledger, ReserveDirection::SolanaReserve, 950_000, 0, 10).unwrap();
    assert_eq!(
        report.classification,
        Classification::WithinTolerance,
        "a confirmed, operator-authorized rebalance must never be misclassified as a breach"
    );
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

/// Puts a GlcToSol request through to `DestinationSubmitted` for
/// `net_destination_atomic`, so its amount counts toward
/// `Ledger::pending_destination_settlement_amount(SolanaReserve)`.
fn glc_to_sol_request_destination_submitted(ledger: &mut Ledger, net_destination_atomic: u64) {
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::GlcToSol,
            crate::ledger::RequestAmounts {
                gross_atomic: net_destination_atomic,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: net_destination_atomic,
                net_destination_atomic,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .record_glc_deposit_observed(
            request_id,
            [0xAA; 32],
            0,
            net_destination_atomic,
            10,
            [0xBB; 32],
            2,
        )
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 3).unwrap();
    ledger
        .record_release_submitted(request_id, [0xCC; 64], 4)
        .unwrap();
}

#[test]
fn a_drop_fully_matching_a_pending_destination_submission_is_in_flight_explained_not_a_breach() {
    let mut ledger = setup();
    // A release for exactly 50_000 has been broadcast to Solana but not
    // yet folded into Settled bookkeeping when the observed balance
    // already reflects the debit.
    glc_to_sol_request_destination_submitted(&mut ledger, 50_000);

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        950_000, // 1_000_000 cached - 50_000, exactly the pending release
        0,
        0,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::InFlightExplained);
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn a_drop_beyond_the_pending_destination_submission_still_breaches_on_the_residual() {
    let mut ledger = setup();
    // Only 50_000 is legitimately pending, but the real balance dropped by
    // 90_000 — the extra 40_000 is genuinely unexplained and must still
    // breach and auto-pause, exactly as before this fix, on the residual
    // amount only.
    glc_to_sol_request_destination_submitted(&mut ledger, 50_000);

    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        910_000, // 1_000_000 cached - 90_000
        0,
        0,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());
}

#[test]
fn in_flight_explained_never_masks_a_hard_invariant_breach() {
    // Even when a drop is fully explained by a pending destination
    // submission, the hard solvency invariant (observed >=
    // protected_minimum + pending_obligations) is checked independently
    // and must still breach if it fails — in-flight explanation only ever
    // affects the delta/tolerance check, never the solvency floor.
    let mut ledger = setup();
    glc_to_sol_request_destination_submitted(&mut ledger, 50_000);
    // protected_minimum is 100_000 (setup()); observe a balance below that
    // floor even though it exactly matches the "explained" delta from
    // 1_000_000.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        50_000, // observed < protected_minimum (100_000)
        0,
        0,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}

#[test]
fn a_broadcast_goldcoin_payout_temporarily_consuming_its_whole_utxo_is_in_flight_explained() {
    // Regression: a UTXO-based-chain-specific effect distinct from the
    // generic DestinationSubmitted/DestinationConfirmed explanation above.
    // Spending the vault's UTXO to fund a payout makes that UTXO's ENTIRE
    // value (paid-out portion AND its own change) temporarily invisible to
    // a confirmed-only `listunspent` read until the payout transaction
    // itself confirms and the change output matures — even though none of
    // that value has actually left the vault's control (the change
    // returns to it). Observed once for real during release-candidate
    // validation: a 6,000 GLC vault funded via one UTXO, spent by a small
    // (~100 GLC) payout, made the ENTIRE 6,000 GLC balance disappear from
    // reconciliation's view for one tick and breach/auto-pause even though
    // ~5,900 GLC of legitimate change was still fully vault-controlled,
    // just unconfirmed.
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            600_000_000_000, // 6,000 GLC cached baseline
            0,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
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
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 10_000_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 10_000_000_000,
                net_destination_atomic: 10_000_000_000,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };

    // A payout consuming the whole vault UTXO: pays out 10_000_000_000
    // (100 GLC), returns 589_900_000_000 (5,899 GLC) as change, pays
    // 100_000_000 (1 GLC) fee — total input value 600_000_000_000 (6,000
    // GLC), matching the vault's entire cached balance.
    ledger
        .raw()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, broadcast_at)
             VALUES (?1, X'00', 10000000000, 589900000000, 100000000, X'00', 'Broadcast', 1, 1)",
            [request_id],
        )
        .unwrap();

    // Both the paid-out portion and the not-yet-matured change are
    // temporarily invisible: observed balance drops to ~0.
    let report = reconcile(&mut ledger, ReserveDirection::GoldcoinReserve, 0, 0, 10).unwrap();
    assert_eq!(
        report.classification,
        Classification::InFlightExplained,
        "the entire drop must be explained by the broadcast-but-unconfirmed payout's full \
         input value, not just its net payout amount: {report:?}"
    );
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
}

#[test]
fn a_confirmed_goldcoin_payout_with_still_immature_change_is_in_flight_explained() {
    // The gap the `Broadcast`-only term above cannot close on its own:
    // once a payout's OWN transaction reaches `Confirmed` (its
    // `required_payout_confirmations` threshold), that term stops
    // counting it — but its change output(s) can still be genuinely
    // `Unconfirmed` in `vault_utxos` if `vault_min_confirmations` differs
    // from (or simply hasn't caught up as fast as)
    // `required_payout_confirmations`. `Ledger::own_unconfirmed_change_
    // atomic` is grounded in the PHYSICAL `vault_utxos` state, not the
    // payout's lifecycle state, so it keeps explaining the drop for as
    // long as the change genuinely remains immature — this production
    // incident's actual production fix (docs/09-runbook.md's "UTXO
    // liquidity" section).
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            600_000_000_000, // 6,000 GLC cached baseline
            0,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
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
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 10_000_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 10_000_000_000,
                net_destination_atomic: 10_000_000_000,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };

    // Same shape as the `Broadcast` case above, but this payout's OWN
    // transaction has already reached `Confirmed` — the `Broadcast`-only
    // term no longer covers it at all. In real production,
    // `Ledger::update_goldcoin_payout_confirmations` moves
    // `goldcoin_payouts.state` and `bridge_requests.state` to `Confirmed`/
    // `DestinationConfirmed` together — reproduced directly here since
    // this test starts from a fixed, pre-existing fact, not the normal
    // build/sign/broadcast/confirm pipeline. `DestinationConfirmed`
    // already explains the NET payout_atomic portion via the pre-existing
    // `net_destination_atomic` term above; what it can never explain is
    // the CHANGE, which is UTXO-chain-specific and has no Solana-side
    // equivalent — that is exactly what `own_unconfirmed_change_atomic`
    // supplies.
    ledger
        .raw()
        .execute(
            "UPDATE bridge_requests SET state = 'DestinationConfirmed' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    ledger
        .raw()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, txid, state, built_at, broadcast_at, confirmations)
             VALUES (?1, X'00', 10000000000, 589900000000, 100000000, X'00', X'AABB', 'Confirmed', 1, 1, 6)",
            [request_id],
        )
        .unwrap();
    // The change output itself, however, is genuinely still immature in
    // `vault_utxos` — the physical fact `own_unconfirmed_change_atomic`
    // checks directly, independent of the payout's own lifecycle state.
    ledger
        .raw()
        .execute(
            "INSERT INTO vault_utxos
                (txid, vout, amount_atomic, script_pubkey_hex, confirmations, first_seen_at, state)
             VALUES (X'AABB', 1, 589900000000, '51', 1, 1, 'Unconfirmed')",
            [],
        )
        .unwrap();

    let report = reconcile(&mut ledger, ReserveDirection::GoldcoinReserve, 0, 0, 10).unwrap();
    assert_eq!(
        report.classification,
        Classification::InFlightExplained,
        "a Confirmed-state payout's still-genuinely-immature change must still be explained, \
         not just while the payout itself remains in Broadcast state: {report:?}"
    );
    assert!(!report.auto_paused);
    assert!(!ledger.is_paused(ReserveDirection::GoldcoinReserve).unwrap());
}

#[test]
fn goldcoin_in_flight_explanation_does_not_leak_into_solana_reconciliation() {
    // The broadcast-payout UTXO-value term is GoldcoinReserve-specific
    // (Goldcoin is UTXO-based; Solana is account-based and has no
    // "change" concept) — must never be added when reconciling
    // SolanaReserve.
    let mut ledger = setup();
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            600_000_000_000,
            0,
            500_000,
            200_000,
            150_000,
            0,
        )
        .unwrap();
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            Direction::SolToGlc,
            crate::ledger::RequestAmounts {
                gross_atomic: 10_000_000_000,
                fee_bps: 0,
                fee_atomic: 0,
                net_atomic: 10_000_000_000,
                net_destination_atomic: 10_000_000_000,
            },
            &[1u8; 32],
            None,
            3600,
            1,
        )
        .unwrap()
    else {
        panic!()
    };
    ledger
        .raw()
        .execute(
            "INSERT INTO goldcoin_payouts
                (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                 dest_p2pkh_hash, state, built_at, broadcast_at)
             VALUES (?1, X'00', 10000000000, 589900000000, 100000000, X'00', 'Broadcast', 1, 1)",
            [request_id],
        )
        .unwrap();

    // A real, unexplained drop on SolanaReserve must still breach — the
    // huge unrelated Goldcoin broadcast payout above must not leak in and
    // explain it away.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        900_000,
        0,
        100,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach);
    assert!(report.auto_paused);
}

// ----------------------------------------------- cross-route accounting --
//
// `SolToRhn` and `RhnToSol` are the first directions that sit in
// `DestinationConfirmed` AFTER their destination reserve's cached balance
// was debited (the source-side close-out is still to come). The
// in-flight explanation term must retire at that debit, not at `Settled`:
// otherwise the same value would be explained twice — once as the
// book's own decrement and once as "pending" — and a genuine loss of
// that size would be masked for as long as the close-out took.

use crate::ledger::{
    RequestAmounts, RequestState, RobinhoodDepositObservation, RobinhoodFinality,
    RobinhoodObservationRow, SolFoldOutcome,
};
use crate::robinhood::fold::FoldOutcome;
use crate::robinhood::testkit::BRIDGE;
use crate::routes::Route;

const BALANCE: u64 = 1_000_000_000;
/// The production reserve mint's decimals.
const MINT_DECIMALS: u8 = 6;
/// 5 GLC, canonical 8dp; at 300 bps the net is 4.85 GLC = 4_850_000 mint
/// units, and at 450 bps it is 4.775 GLC = 477_500_000 canonical units.
const GROSS_CANONICAL: u64 = 500_000_000;
const RHN_TO_SOL_BPS: u64 = 300;
const RHN_TO_SOL_NET_MINT: u64 = 4_850_000;
const SOL_TO_RHN_BPS: u64 = 450;
const SOL_TO_RHN_NET_CANONICAL: u64 = 477_500_000;
const EVM_RECIPIENT_TEXT: &str = "0x00000000000000000000000000000000000000ec";
const EVM_RECIPIENT: [u8; 20] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xec,
];

/// Every reserve at the same cached balance, no protected minimum, so any
/// direction can be driven to its destination-final state and every
/// arithmetic below reads off `BALANCE` directly.
fn ledger_with_every_reserve() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    for reserve in [
        ReserveDirection::GoldcoinReserve,
        ReserveDirection::SolanaReserve,
        ReserveDirection::RobinhoodReserve,
    ] {
        ledger
            .configure_reserve(reserve, BALANCE, 0, BALANCE, BALANCE / 2, BALANCE / 4, 0)
            .unwrap();
    }
    ledger
}

fn cached_balance(ledger: &Ledger, reserve: ReserveDirection) -> u64 {
    ledger.reserve_snapshot(reserve).unwrap().0
}

fn pending(ledger: &Ledger, reserve: ReserveDirection) -> u64 {
    ledger
        .pending_destination_settlement_amount(reserve, 1_000)
        .unwrap()
}

fn state_of(ledger: &Ledger, request_id: i64) -> RequestState {
    ledger.get_request(request_id).unwrap().unwrap().state
}

/// A FINAL Robinhood observation on `route`, stored so a fold can link
/// back to it.
fn stored_robinhood_observation(
    ledger: &Ledger,
    index: u64,
    route: Route,
    destination: Vec<u8>,
) -> RobinhoodObservationRow {
    let robinhood = u128::from(GROSS_CANONICAL) * 10_000_000_000;
    let row = RobinhoodObservationRow {
        id: index as i64 + 1,
        observation: RobinhoodDepositObservation {
            source_contract: BRIDGE.to_bytes(),
            obligation_index: index,
            route,
            depositor: [0x33; 20],
            destination,
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(robinhood).to_be_bytes(),
            amount_canonical_atomic: GROSS_CANONICAL,
            tx_hash: [0xaa; 32],
            log_index: 3,
            block_number: 500,
            block_hash: [0xbb; 32],
        },
        finality: RobinhoodFinality::Final,
        observed_at: 100,
        finalized_at: Some(200),
        reorged_at: None,
    };
    ledger
        .raw()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     'Final', 100, 200)",
            rusqlite::params![
                row.id,
                &row.observation.source_contract[..],
                row.observation.obligation_index as i64,
                route.contract_route_id().unwrap(),
                route.as_str(),
                &row.observation.depositor[..],
                row.observation.destination,
                &row.observation.amount_robinhood_atomic[..],
                row.observation.amount_canonical_atomic as i64,
                &row.observation.tx_hash[..],
                row.observation.log_index as i64,
                row.observation.block_number as i64,
                &row.observation.block_hash[..],
            ],
        )
        .unwrap();
    row
}

/// A payable `RhnToSol` request whose Solana release has been SUBMITTED.
fn rhn_to_sol_release_submitted(ledger: &mut Ledger) -> i64 {
    let row = stored_robinhood_observation(ledger, 0, Route::RhnToSol, vec![0x51; 32]);
    let FoldOutcome::FoldedFinalized { request_id } =
        crate::robinhood::fold::fold_observation_to_solana(
            ledger,
            &row,
            RHN_TO_SOL_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            MINT_DECIMALS,
            true,
            300,
        )
        .unwrap()
    else {
        panic!("a payable RhnToSol fold")
    };
    ledger
        .record_release_submitted(request_id, [0x77; 64], 400)
        .unwrap();
    request_id
}

/// A payable `SolToRhn` request, `SourceFinalized`, its Robinhood payout
/// not yet final.
fn sol_to_rhn_source_finalized(ledger: &mut Ledger) -> i64 {
    let fb = crate::amount_conversion::compute_fee_at_bps(
        crate::amount_conversion::CanonicalAtomic(GROSS_CANONICAL),
        SOL_TO_RHN_BPS,
    )
    .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            0,
            RequestAmounts {
                gross_atomic: fb.gross.0,
                fee_bps: fb.fee_bps,
                fee_atomic: fb.fee.0,
                net_atomic: fb.net.0,
                net_destination_atomic: fb.net.0,
            },
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
        .unwrap()
    else {
        panic!("a payable SolToRhn fold")
    };
    request_id
}

#[test]
fn an_rhn_to_sol_release_is_pending_until_the_book_is_debited_and_never_after() {
    let mut ledger = ledger_with_every_reserve();
    let request_id = rhn_to_sol_release_submitted(&mut ledger);

    // Submitted: the chain may already show the debit, the book does not.
    // The amount is pending, and a matching drop is explained.
    assert_eq!(
        state_of(&ledger, request_id),
        RequestState::DestinationSubmitted
    );
    assert_eq!(
        cached_balance(&ledger, ReserveDirection::SolanaReserve),
        BALANCE
    );
    assert_eq!(
        pending(&ledger, ReserveDirection::SolanaReserve),
        RHN_TO_SOL_NET_MINT
    );
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        BALANCE - RHN_TO_SOL_NET_MINT,
        0,
        450,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::InFlightExplained);
    assert!(!report.auto_paused);

    // Confirmed at `finalized`: the book is debited NOW and the request
    // parks in DestinationConfirmed until `executeSettlement` lands. The
    // pending term must retire here — the observed and cached balances
    // already agree on this value.
    ledger.mark_release_confirmed(request_id, 500).unwrap();
    assert_eq!(
        state_of(&ledger, request_id),
        RequestState::DestinationConfirmed
    );
    // (reconcile refreshed the cache to the observed figure; the confirm
    // then debited it once more — the documented one-tick gap, which the
    // next tick's balance INCREASE closes and can never breach on.)
    let cached = cached_balance(&ledger, ReserveDirection::SolanaReserve);
    assert_eq!(cached, BALANCE - 2 * RHN_TO_SOL_NET_MINT);
    assert_eq!(
        pending(&ledger, ReserveDirection::SolanaReserve),
        0,
        "a DestinationConfirmed RhnToSol row was already debited and must not be pending"
    );
    let (_, protected_minimum, reserved, pending_obligations) = ledger
        .reserve_snapshot(ReserveDirection::SolanaReserve)
        .unwrap();
    assert_eq!(
        (protected_minimum, reserved, pending_obligations),
        (0, 0, 0),
        "the reservation was released at the debit; nothing is held or manufactured"
    );

    // Book and chain agree: nothing to explain, nothing explained.
    let report = reconcile(&mut ledger, ReserveDirection::SolanaReserve, cached, 0, 550).unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);
    assert!(!report.auto_paused);

    // THE regression: a SECOND drop of exactly the release's size, while
    // the request still sits in DestinationConfirmed, is a genuine loss.
    // Before the fix the lingering row explained it away.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::SolanaReserve,
        cached - RHN_TO_SOL_NET_MINT,
        0,
        600,
    )
    .unwrap();
    assert_eq!(
        report.classification,
        Classification::Breach,
        "a drop the book was not debited for must never be explained by a row it was: {report:?}"
    );
    assert!(report.auto_paused);
    assert!(ledger.is_paused(ReserveDirection::SolanaReserve).unwrap());

    // And the close-out moves nothing, pending stays retired.
    ledger
        .mark_robinhood_settlement_confirmed(request_id, 700)
        .unwrap();
    assert_eq!(state_of(&ledger, request_id), RequestState::Settled);
    assert_eq!(pending(&ledger, ReserveDirection::SolanaReserve), 0);
}

#[test]
fn a_sol_to_rhn_payout_is_pending_never_and_its_confirmed_row_is_not_double_counted() {
    let mut ledger = ledger_with_every_reserve();
    let request_id = sol_to_rhn_source_finalized(&mut ledger);

    // Unchanged from `GlcToRhn`: while the payout is authorized/broadcast
    // the request is still SourceFinalized, so nothing is pending against
    // the Robinhood book (the reservation, not this term, holds the
    // amount). This pins that the fix did not widen the term either.
    assert_eq!(state_of(&ledger, request_id), RequestState::SourceFinalized);
    assert_eq!(pending(&ledger, ReserveDirection::RobinhoodReserve), 0);
    let (_, _, reserved, pending_obligations) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!(
        (reserved, pending_obligations),
        (SOL_TO_RHN_NET_CANONICAL, SOL_TO_RHN_NET_CANONICAL)
    );

    // Payout final: book debited, request parks in DestinationConfirmed
    // until the Solana completion confirms.
    ledger
        .mark_robinhood_payout_settled(request_id, 500)
        .unwrap();
    assert_eq!(
        state_of(&ledger, request_id),
        RequestState::DestinationConfirmed
    );
    let cached = cached_balance(&ledger, ReserveDirection::RobinhoodReserve);
    assert_eq!(cached, BALANCE - SOL_TO_RHN_NET_CANONICAL);
    assert_eq!(
        pending(&ledger, ReserveDirection::RobinhoodReserve),
        0,
        "a DestinationConfirmed SolToRhn row was already debited and must not be pending"
    );
    let (_, _, reserved, pending_obligations) = ledger
        .reserve_snapshot(ReserveDirection::RobinhoodReserve)
        .unwrap();
    assert_eq!((reserved, pending_obligations), (0, 0));

    // Book and chain agree.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::RobinhoodReserve,
        cached,
        0,
        550,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::WithinTolerance);

    // THE regression, Robinhood side: a second drop of the payout's size
    // is a genuine loss and must breach.
    let report = reconcile(
        &mut ledger,
        ReserveDirection::RobinhoodReserve,
        cached - SOL_TO_RHN_NET_CANONICAL,
        0,
        600,
    )
    .unwrap();
    assert_eq!(report.classification, Classification::Breach, "{report:?}");
    assert!(report.auto_paused);
}

/// Drives one request of `direction` to the ledger call that makes its
/// destination leg FINAL, and reports whether the destination reserve's
/// cached balance moved at that call. This is the ground truth
/// `Direction::destination_debited_at_destination_confirmed` claims to
/// describe, so the two are compared for all six directions: a seventh
/// direction, or a change to when any reserve is debited, fails here.
fn destination_final_moves_the_book(direction: Direction) -> (bool, RequestState) {
    let mut ledger = ledger_with_every_reserve();
    let reserve = direction.destination_reserve();
    let request_id = match direction {
        Direction::GlcToSol | Direction::GlcToRhn => {
            let recipient: &[u8] = if direction == Direction::GlcToSol {
                &[0x51; 32]
            } else {
                &EVM_RECIPIENT
            };
            let CreateRequestOutcome::Reserved { request_id } = ledger
                .create_request(
                    direction,
                    RequestAmounts {
                        gross_atomic: GROSS_CANONICAL,
                        fee_bps: 0,
                        fee_atomic: 0,
                        net_atomic: GROSS_CANONICAL,
                        net_destination_atomic: GROSS_CANONICAL,
                    },
                    recipient,
                    None,
                    3600,
                    1,
                )
                .unwrap()
            else {
                panic!()
            };
            ledger
                .record_glc_deposit_observed(
                    request_id,
                    [0xAA; 32],
                    0,
                    GROSS_CANONICAL,
                    10,
                    [0xBB; 32],
                    2,
                )
                .unwrap();
            ledger.mark_glc_source_finalized(request_id, 3).unwrap();
            request_id
        }
        Direction::SolToGlc => {
            let SolFoldOutcome::FoldedFinalized { request_id } = ledger
                .fold_sol_deposit(
                    0,
                    RequestAmounts {
                        gross_atomic: GROSS_CANONICAL,
                        fee_bps: 0,
                        fee_atomic: 0,
                        net_atomic: GROSS_CANONICAL,
                        net_destination_atomic: GROSS_CANONICAL,
                    },
                    [0x11; 32],
                    b"GLCtestaddress",
                    None,
                    300,
                )
                .unwrap()
            else {
                panic!()
            };
            request_id
        }
        Direction::RhnToGlc => {
            let destination = crate::goldcoin::address::encode_p2pkh(
                &[0x42; 20],
                crate::goldcoin::address::Network::Testnet,
            );
            let row =
                stored_robinhood_observation(&ledger, 0, Route::RhnToGlc, destination.into_bytes());
            let FoldOutcome::FoldedFinalized { request_id } =
                crate::robinhood::fold::fold_observation(
                    &mut ledger,
                    &row,
                    crate::goldcoin::address::Network::Testnet,
                    0,
                    crate::amount_conversion::CanonicalAtomic(1),
                    true,
                    300,
                )
                .unwrap()
            else {
                panic!()
            };
            request_id
        }
        Direction::SolToRhn => sol_to_rhn_source_finalized(&mut ledger),
        Direction::RhnToSol => {
            let row = stored_robinhood_observation(&ledger, 0, Route::RhnToSol, vec![0x51; 32]);
            let FoldOutcome::FoldedFinalized { request_id } =
                crate::robinhood::fold::fold_observation_to_solana(
                    &mut ledger,
                    &row,
                    RHN_TO_SOL_BPS,
                    crate::amount_conversion::CanonicalAtomic(1),
                    MINT_DECIMALS,
                    true,
                    300,
                )
                .unwrap()
            else {
                panic!()
            };
            request_id
        }
    };
    assert_eq!(state_of(&ledger, request_id), RequestState::SourceFinalized);
    let before = cached_balance(&ledger, reserve);

    // The destination leg goes final, through the same call production
    // makes for that direction.
    if direction.destination_is_solana() {
        ledger
            .record_release_submitted(request_id, [0x77; 64], 400)
            .unwrap();
        ledger.mark_release_confirmed(request_id, 500).unwrap();
    } else if direction.destination_is_robinhood() {
        ledger
            .mark_robinhood_payout_settled(request_id, 500)
            .unwrap();
    } else {
        // A Goldcoin payout broadcast and then observed at depth.
        ledger
            .raw()
            .execute(
                "INSERT INTO goldcoin_payouts
                    (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                     dest_p2pkh_hash, txid, state, built_at, broadcast_at)
                 VALUES (?1, X'00', ?2, 0, 0, X'00', X'CC', 'Broadcast', 1, 1)",
                rusqlite::params![request_id, GROSS_CANONICAL as i64],
            )
            .unwrap();
        ledger
            .raw()
            .execute(
                "UPDATE bridge_requests SET state = 'DestinationSubmitted', destination_txid = X'CC'
                 WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        assert!(ledger
            .update_goldcoin_payout_confirmations(request_id, 6, 100, 6, 500)
            .unwrap());
    }
    let state = state_of(&ledger, request_id);
    assert!(
        matches!(
            state,
            RequestState::DestinationConfirmed | RequestState::Settled
        ),
        "{direction:?} must be at or past DestinationConfirmed, was {state:?}"
    );
    (cached_balance(&ledger, reserve) != before, state)
}

#[test]
fn the_destination_debit_predicate_matches_the_ledger_for_every_direction() {
    for direction in Direction::ALL {
        let (moved, state) = destination_final_moves_the_book(direction);
        assert_eq!(
            moved,
            direction.destination_debited_at_destination_confirmed(),
            "{direction:?}: the book {} at destination finality but the predicate says {}",
            if moved { "moved" } else { "did not move" },
            direction.destination_debited_at_destination_confirmed()
        );
        // The shape the predicate describes: where the book moved, a
        // request either settled in the same instant or is now waiting in
        // DestinationConfirmed; where it did not, the request is in
        // DestinationConfirmed with its reservation still held.
        if direction.settles_on_release() || direction.settles_on_payout() {
            assert_eq!(state, RequestState::Settled, "{direction:?}");
        } else {
            assert_eq!(state, RequestState::DestinationConfirmed, "{direction:?}");
        }
    }
}

/// The two Goldcoin-bound directions keep their pre-existing term: a
/// `DestinationConfirmed` row is still pending against the vault's book
/// until the close-out debits it.
#[test]
fn a_goldcoin_bound_destination_confirmed_row_stays_pending_until_settled() {
    for direction in [Direction::SolToGlc, Direction::RhnToGlc] {
        let mut ledger = ledger_with_every_reserve();
        let request_id = match direction {
            Direction::SolToGlc => {
                let SolFoldOutcome::FoldedFinalized { request_id } = ledger
                    .fold_sol_deposit(
                        0,
                        RequestAmounts {
                            gross_atomic: GROSS_CANONICAL,
                            fee_bps: 0,
                            fee_atomic: 0,
                            net_atomic: GROSS_CANONICAL,
                            net_destination_atomic: GROSS_CANONICAL,
                        },
                        [0x11; 32],
                        b"GLCtestaddress",
                        None,
                        300,
                    )
                    .unwrap()
                else {
                    panic!()
                };
                request_id
            }
            _ => {
                let destination = crate::goldcoin::address::encode_p2pkh(
                    &[0x42; 20],
                    crate::goldcoin::address::Network::Testnet,
                );
                let row = stored_robinhood_observation(
                    &ledger,
                    0,
                    Route::RhnToGlc,
                    destination.into_bytes(),
                );
                let FoldOutcome::FoldedFinalized { request_id } =
                    crate::robinhood::fold::fold_observation(
                        &mut ledger,
                        &row,
                        crate::goldcoin::address::Network::Testnet,
                        0,
                        crate::amount_conversion::CanonicalAtomic(1),
                        true,
                        300,
                    )
                    .unwrap()
                else {
                    panic!()
                };
                request_id
            }
        };
        ledger
            .raw()
            .execute(
                "INSERT INTO goldcoin_payouts
                    (request_id, commitment_hash, payout_atomic, change_atomic, fee_atomic,
                     dest_p2pkh_hash, txid, state, built_at, broadcast_at)
                 VALUES (?1, X'00', ?2, 0, 0, X'00', X'CC', 'Broadcast', 1, 1)",
                rusqlite::params![request_id, GROSS_CANONICAL as i64],
            )
            .unwrap();
        ledger
            .raw()
            .execute(
                "UPDATE bridge_requests SET state = 'DestinationSubmitted', destination_txid = X'CC'
                 WHERE id = ?1",
                [request_id],
            )
            .unwrap();
        ledger
            .update_goldcoin_payout_confirmations(request_id, 6, 100, 6, 500)
            .unwrap();
        assert_eq!(
            state_of(&ledger, request_id),
            RequestState::DestinationConfirmed
        );
        assert_eq!(
            cached_balance(&ledger, ReserveDirection::GoldcoinReserve),
            BALANCE,
            "{direction:?}: the vault is debited at Settled, not here"
        );
        assert_eq!(
            pending(&ledger, ReserveDirection::GoldcoinReserve),
            GROSS_CANONICAL,
            "{direction:?}: still pending against the book"
        );
    }
}
