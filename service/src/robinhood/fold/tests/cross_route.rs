//! `RhnToSol` and `SolToRhn` at the ledger boundary: the two cross-route
//! folds, their reserve accounting through settlement, their parks, and
//! their resume/refund guards — every one of them driven through the real
//! ledger functions against a real in-memory database.

use super::*;
use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic};
use crate::ledger::{RequestAmounts, ReserveDirection, SolFoldOutcome};

/// The production reserve mint's decimals.
const MINT_DECIMALS: u8 = 6;
const RHN_TO_SOL_BPS: u64 = 300;
const SOL_TO_RHN_BPS: u64 = 450;

/// A Solana wallet as the deposit's destination payload — raw 32 bytes.
const SOL_RECIPIENT: [u8; 32] = [0x51; 32];
/// An EVM wallet as a Solana deposit's destination, as the ASCII text a
/// wallet would put in the `deposit_to_reserve` payload.
const EVM_RECIPIENT_TEXT: &str = "0x00000000000000000000000000000000000000ec";
const EVM_RECIPIENT: [u8; 20] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xec,
];

fn ledger_with_every_reserve() -> Ledger {
    let mut ledger = ledger();
    for reserve in [
        ReserveDirection::SolanaReserve,
        ReserveDirection::RobinhoodReserve,
    ] {
        ledger
            .configure_reserve(
                reserve,
                1_000_000_000_000,
                0,
                1_000_000_000_000,
                500_000_000_000,
                250_000_000_000,
                100,
            )
            .unwrap();
    }
    ledger
}

fn rhn_to_sol_observation(
    index: u64,
    canonical: u64,
    destination: Vec<u8>,
) -> RobinhoodObservationRow {
    let mut row = observation(index, canonical, destination);
    row.observation.route = Route::RhnToSol;
    row.observation.log_index = 3;
    row
}

fn store_rhn_to_sol(ledger: &Ledger, row: &RobinhoodObservationRow) {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, 4, 'RhnToSol', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     'Final', 100, 200)",
            rusqlite::params![
                row.id,
                &row.observation.source_contract[..],
                row.observation.obligation_index as i64,
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
        .expect("stores the observation");
}

fn reserve_row(ledger: &Ledger, reserve: ReserveDirection) -> (i64, i64, i64, i64, i64) {
    ledger
        .conn_for_tests()
        .query_row(
            "SELECT total_reserve_balance, reserved_liquidity, pending_obligations,
                    settled_liquidity_total, accrued_fees_atomic
             FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
}

fn sol_to_rhn_amounts(gross_canonical: u64) -> RequestAmounts {
    let fb = compute_fee_at_bps(CanonicalAtomic(gross_canonical), SOL_TO_RHN_BPS).unwrap();
    RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

// =====================================================================
// RhnToSol fold
// =====================================================================

#[test]
fn rhn_to_sol_folds_against_the_solana_reserve_in_mint_units() {
    let mut ledger = ledger_with_every_reserve();
    let gross = 500_000_000u64; // 5 GLC
    let row = rhn_to_sol_observation(0, gross, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);

    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedFinalized { request_id } = outcome else {
        panic!("expected a payable fold, got {outcome:?}");
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::RhnToSol);
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert_eq!(
        request.recipient,
        SOL_RECIPIENT.to_vec(),
        "raw 32-byte pubkey"
    );
    assert_eq!(request.fee_bps, RHN_TO_SOL_BPS);
    assert_eq!(request.gross_amount_atomic, gross);
    assert_eq!(request.net_amount_atomic, 485_000_000);
    // The destination figure is the net in the MINT's own unit.
    assert_eq!(request.net_destination_atomic, 4_850_000);
    // The release claim's source binding: the deposit's own tx hash and
    // log index, recorded from the finalized log.
    assert_eq!(request.source_txid, Some(row.observation.tx_hash));
    assert_eq!(request.source_vout, Some(3));
    assert_eq!(request.source_obligation_index, Some(0));

    // Reserved on the SOLANA reserve, in mint units; Goldcoin and
    // Robinhood untouched.
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve).1,
        4_850_000
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve).2,
        4_850_000
    );
    assert_eq!(reserve_row(&ledger, ReserveDirection::GoldcoinReserve).1, 0);
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve).1,
        0
    );

    // Idempotent on the durable identity.
    assert_eq!(
        fold_observation_to_solana(
            &mut ledger,
            &row,
            RHN_TO_SOL_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            MINT_DECIMALS,
            true,
            301
        )
        .unwrap(),
        FoldOutcome::AlreadyFolded { request_id }
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve).1,
        4_850_000
    );
}

#[test]
fn rhn_to_sol_accepts_a_base58_destination_and_refuses_anything_else() {
    let pubkey = solana_sdk::pubkey::Pubkey::new_from_array(SOL_RECIPIENT);
    let row = rhn_to_sol_observation(0, 100_000_000, pubkey.to_string().into_bytes());
    assert_eq!(validate_solana_destination(&row).unwrap(), SOL_RECIPIENT);
    let row = rhn_to_sol_observation(0, 100_000_000, SOL_RECIPIENT.to_vec());
    assert_eq!(validate_solana_destination(&row).unwrap(), SOL_RECIPIENT);
    // ANY 32-byte payload is a raw pubkey by definition — including one
    // that happens to be ASCII — which is exactly why the two spellings
    // are unconfusable: a valid base58 spelling of a 32-byte key is never
    // 32 bytes long.
    let ascii32 = b"0123456789abcdef0123456789abcdef".to_vec();
    let row = rhn_to_sol_observation(0, 100_000_000, ascii32.clone());
    assert_eq!(validate_solana_destination(&row).unwrap().to_vec(), ascii32);

    for bad in [
        vec![0x01],
        vec![0x51; 31],
        vec![0x51; 33],
        b"not a pubkey".to_vec(),
        // A Goldcoin address is not a Solana one.
        destination().into_bytes(),
    ] {
        let row = rhn_to_sol_observation(0, 100_000_000, bad.clone());
        assert!(
            matches!(
                validate_solana_destination(&row),
                Err(FoldError::UndeliverableDestination { .. })
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn rhn_to_sol_parks_an_undeliverable_destination_refundable_and_unreserved() {
    let mut ledger = ledger_with_every_reserve();
    let row = rhn_to_sol_observation(0, 500_000_000, b"nonsense".to_vec());
    store_rhn_to_sol(&ledger, &row);
    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}");
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert!(request
        .manual_review_note
        .as_deref()
        .unwrap()
        .starts_with("undeliverable destination"));
    assert_eq!(
        request.recipient,
        b"nonsense".to_vec(),
        "the payload as deposited"
    );
    assert_eq!(request.net_destination_atomic, 0);
    assert_eq!(reserve_row(&ledger, ReserveDirection::SolanaReserve).1, 0);
    // Not resumable — refund is the only exit.
    assert!(ledger
        .resume_manual_review_cross_route(Direction::RhnToSol, request_id, "try", "cli:test", 400)
        .is_err());
}

#[test]
fn rhn_to_sol_parks_a_net_that_cannot_be_spelled_at_the_mints_precision() {
    let mut ledger = ledger_with_every_reserve();
    // 1.00000010 GLC: canonical-exact, but net at 300 bps ends in ...10.
    let row = rhn_to_sol_observation(0, 100_000_010, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}");
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    let note = request.manual_review_note.as_deref().unwrap();
    assert!(note.starts_with("undeliverable amount"), "{note}");
    assert!(note.contains("97000010"), "names the net: {note}");
    assert_eq!(request.recipient, SOL_RECIPIENT.to_vec());
    assert_eq!(request.net_amount_atomic, 97_000_010, "nothing was rounded");
    assert_eq!(request.net_destination_atomic, 0);
    assert_eq!(reserve_row(&ledger, ReserveDirection::SolanaReserve).1, 0);
    // The same deposit at a different mint precision (8dp) is deliverable
    // — the refusal is about THIS mint, read live, never a constant.
    let mut ledger = ledger_with_every_reserve();
    store_rhn_to_sol(&ledger, &row);
    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        8,
        true,
        300,
    )
    .unwrap();
    assert!(matches!(outcome, FoldOutcome::FoldedFinalized { .. }));
}

#[test]
fn rhn_to_sol_parks_while_the_route_is_closed_and_while_its_own_admission_is_closed() {
    // Route gate closed: parked with the route reason, nothing reserved.
    let mut ledger = ledger_with_every_reserve();
    let row = rhn_to_sol_observation(0, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        false,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}");
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("route_disabled_at_fold")
    );
    assert_eq!(reserve_row(&ledger, ReserveDirection::SolanaReserve).1, 0);

    // Route admission closed for RhnToSol ONLY: parked with the
    // route-admission reason, while a SolToGlc-style draw on the same
    // Solana reserve is not what closed (GlcToSol shares the reserve).
    let mut ledger = ledger_with_every_reserve();
    ledger
        .set_route_admission(Route::RhnToSol, true, Some("incident"))
        .unwrap();
    let row = rhn_to_sol_observation(1, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .manual_review_note
            .as_deref(),
        Some("route_admission_closed_at_fold")
    );
    // SolToRhn's own gate is untouched by RhnToSol's closure.
    assert_eq!(
        ledger.route_admission_blocker(Direction::SolToRhn).unwrap(),
        None
    );
    // Reopen and resume: the SAME request re-admits exactly once.
    ledger
        .set_route_admission(Route::RhnToSol, false, None)
        .unwrap();
    assert_eq!(
        ledger
            .resume_manual_review_cross_route(
                Direction::RhnToSol,
                request_id,
                "ok",
                "cli:test",
                500
            )
            .unwrap(),
        crate::ledger::ResumeManualReviewOutcome::Resumed
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve).1,
        4_850_000
    );
    assert!(matches!(
        ledger
            .resume_manual_review_cross_route(
                Direction::RhnToSol,
                request_id,
                "ok",
                "cli:test",
                501
            )
            .unwrap(),
        crate::ledger::ResumeManualReviewOutcome::AlreadyResumed { .. }
    ));
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve).1,
        4_850_000
    );
}

#[test]
fn rhn_to_sol_parks_when_the_solana_reserve_is_paused_and_never_touches_goldcoin_gates() {
    let mut ledger = ledger_with_every_reserve();
    ledger
        .set_paused(ReserveDirection::SolanaReserve, true, Some("incident"))
        .unwrap();
    // The Goldcoin reserve being closed is irrelevant to this route.
    ledger
        .set_admission(ReserveDirection::GoldcoinReserve, true, Some("x"))
        .unwrap();
    let row = rhn_to_sol_observation(0, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let outcome = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedManualReview { request_id } = outcome else {
        panic!("{outcome:?}");
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
    ledger
        .set_paused(ReserveDirection::SolanaReserve, false, None)
        .unwrap();
    let row = rhn_to_sol_observation(1, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    assert!(matches!(
        fold_observation_to_solana(
            &mut ledger,
            &row,
            RHN_TO_SOL_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            MINT_DECIMALS,
            true,
            300
        )
        .unwrap(),
        FoldOutcome::FoldedFinalized { .. }
    ));
}

#[test]
fn rhn_to_glc_folds_exactly_as_before_the_cross_routes_existed() {
    // The Goldcoin-bound fold keeps its shape: recipient is the ASCII
    // address, the destination amount is canonical, and — the one thing
    // this change could have leaked — `source_txid`/`source_vout` stay
    // NULL, so the public `source_txid` a RhnToGlc transfer reports is
    // unchanged.
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    let outcome = fold_observation(
        &mut ledger,
        &row,
        network(),
        BRIDGE_FEE_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        true,
        300,
    )
    .unwrap();
    let FoldOutcome::FoldedFinalized { request_id } = outcome else {
        panic!("{outcome:?}");
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::RhnToGlc);
    assert_eq!(request.source_txid, None);
    assert_eq!(request.source_vout, None);
    assert_eq!(request.net_destination_atomic, request.net_amount_atomic);
    // And the RhnToSol fold refuses an RhnToGlc observation, and vice
    // versa — neither can be fed the other's route.
    assert!(matches!(
        fold_observation_to_solana(
            &mut ledger,
            &row,
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            MINT_DECIMALS,
            true,
            300
        ),
        Err(FoldError::UnsupportedRoute { .. })
    ));
    let cross = rhn_to_sol_observation(9, 1_000_000_000, SOL_RECIPIENT.to_vec());
    assert!(matches!(
        fold_observation(
            &mut ledger,
            &cross,
            network(),
            BRIDGE_FEE_BPS,
            crate::amount_conversion::CanonicalAtomic(1),
            true,
            300
        ),
        Err(FoldError::UnsupportedRoute { .. })
    ));
}

// =====================================================================
// RhnToSol settlement bookkeeping
// =====================================================================

#[test]
fn rhn_to_sol_release_confirms_to_destination_confirmed_and_settles_only_on_chain_settlement() {
    let mut ledger = ledger_with_every_reserve();
    let row = rhn_to_sol_observation(0, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let FoldOutcome::FoldedFinalized { request_id } = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap() else {
        panic!()
    };
    let before_solana = reserve_row(&ledger, ReserveDirection::SolanaReserve);
    let before_robinhood = reserve_row(&ledger, ReserveDirection::RobinhoodReserve);

    // Release submitted, then confirmed at `finalized`.
    ledger
        .record_release_submitted(request_id, [0x77; 64], 400)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationSubmitted
    );
    ledger.mark_release_confirmed(request_id, 500).unwrap();
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        request.state,
        RequestState::DestinationConfirmed,
        "a confirmed release is the destination leg, not the settlement"
    );

    // The Solana reserve moved NOW: reserved -> settled, balance down by
    // the mint-unit net; the fee accrued on the SOURCE (Robinhood) row.
    let after_solana = reserve_row(&ledger, ReserveDirection::SolanaReserve);
    assert_eq!(after_solana.0, before_solana.0 - 4_850_000);
    assert_eq!(after_solana.1, before_solana.1 - 4_850_000);
    assert_eq!(after_solana.2, before_solana.2 - 4_850_000);
    assert_eq!(after_solana.3, before_solana.3 + 4_850_000);
    assert_eq!(
        after_solana.4, before_solana.4,
        "no fee on the destination row"
    );
    let after_robinhood = reserve_row(&ledger, ReserveDirection::RobinhoodReserve);
    assert_eq!(
        after_robinhood.4,
        before_robinhood.4 + 15_000_000,
        "fee on the source row"
    );
    assert_eq!(
        after_robinhood.0, before_robinhood.0,
        "no netting across reserves"
    );

    // Idempotent: a second confirmation moves nothing.
    ledger.mark_release_confirmed(request_id, 501).unwrap();
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve),
        after_solana
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve),
        after_robinhood
    );

    // The on-chain settlement is the LAST step and moves no reserve.
    ledger
        .mark_robinhood_settlement_confirmed(request_id, 600)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve),
        after_solana
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve),
        after_robinhood
    );
    ledger
        .mark_robinhood_settlement_confirmed(request_id, 601)
        .unwrap();
}

#[test]
fn rhn_to_sol_cannot_be_settled_on_chain_before_its_release_confirmed() {
    let mut ledger = ledger_with_every_reserve();
    let row = rhn_to_sol_observation(0, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let FoldOutcome::FoldedFinalized { request_id } = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        300,
    )
    .unwrap() else {
        panic!()
    };
    // From SourceFinalized: refused.
    assert!(ledger
        .mark_robinhood_settlement_confirmed(request_id, 600)
        .is_err());
    // From DestinationSubmitted (release broadcast, not final): refused.
    ledger
        .record_release_submitted(request_id, [0x77; 64], 400)
        .unwrap();
    assert!(ledger
        .mark_robinhood_settlement_confirmed(request_id, 600)
        .is_err());
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::DestinationSubmitted
    );
}

#[test]
fn a_solana_reserve_release_function_refuses_a_robinhood_bound_request() {
    // Guards on every bookkeeping entry point: a request may only be
    // advanced by the functions of its OWN destination chain.
    let mut ledger = ledger_with_every_reserve();
    let request_id = match ledger
        .fold_sol_deposit_to_robinhood(
            0,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
        .unwrap()
    {
        SolFoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    assert!(ledger
        .mark_robinhood_settlement_confirmed(request_id, 1)
        .is_err());
    assert!(ledger
        .mark_robinhood_payout_completion_confirmed(request_id, 1)
        .is_err());
}

// =====================================================================
// SolToRhn fold
// =====================================================================

#[test]
fn sol_to_rhn_folds_against_the_robinhood_reserve_in_canonical_units() {
    let mut ledger = ledger_with_every_reserve();
    let outcome = ledger
        .fold_sol_deposit_to_robinhood(
            7,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
        .unwrap();
    let SolFoldOutcome::FoldedFinalized { request_id } = outcome else {
        panic!("{outcome:?}");
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::SolToRhn);
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert_eq!(
        request.recipient,
        EVM_RECIPIENT.to_vec(),
        "20 raw address bytes"
    );
    assert_eq!(request.requester, Some([0x11; 32]));
    assert_eq!(request.source_obligation_index, Some(7));
    assert_eq!(
        request.source_contract.as_deref(),
        Some(&glc_reserve_bridge_shared::PROGRAM_ID_BYTES[..])
    );
    assert_eq!(request.fee_bps, SOL_TO_RHN_BPS);
    assert_eq!(request.net_amount_atomic, 477_500_000);
    assert_eq!(
        request.net_destination_atomic, 477_500_000,
        "canonical, as the row is"
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve).1,
        477_500_000
    );
    assert_eq!(reserve_row(&ledger, ReserveDirection::SolanaReserve).1, 0);

    // The same obligation index can never fold twice — as SolToRhn OR as
    // SolToGlc: one Solana obligation, one request, ever.
    assert_eq!(
        ledger
            .fold_sol_deposit_to_robinhood(
                7,
                sol_to_rhn_amounts(500_000_000),
                [0x11; 32],
                Some(EVM_RECIPIENT),
                EVM_RECIPIENT_TEXT.as_bytes(),
                true,
                None,
                301,
            )
            .unwrap(),
        SolFoldOutcome::AlreadyFolded { request_id }
    );
    let as_goldcoin = ledger
        .fold_sol_deposit(
            7,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            destination().as_bytes(),
            None,
            302,
        )
        .unwrap();
    assert_eq!(as_goldcoin, SolFoldOutcome::AlreadyFolded { request_id });
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve).1,
        477_500_000
    );
}

#[test]
fn sol_to_rhn_parks_for_every_gate_with_its_own_reason_and_holds_nothing() {
    // Route closed.
    let mut ledger = ledger_with_every_reserve();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            0,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            false,
            None,
            300,
        )
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
        Some("route_disabled_at_fold")
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve).1,
        0
    );

    // Undeliverable destination (the raw payload is kept as evidence).
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            1,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            None,
            b"0xnot-an-address",
            true,
            Some("undeliverable destination: bad hex"),
            300,
        )
        .unwrap()
    else {
        panic!()
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("undeliverable destination: bad hex")
    );
    assert_eq!(request.recipient, b"0xnot-an-address".to_vec());
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve).1,
        0
    );

    // Route admission closed for SolToRhn alone.
    ledger
        .set_route_admission(Route::SolToRhn, true, Some("incident"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            2,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
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
        Some("route_admission_closed_at_fold")
    );
    // GlcToRhn draws on the same reserve and is NOT closed by it.
    assert_eq!(
        ledger.route_admission_blocker(Direction::GlcToRhn).unwrap(),
        None
    );
    ledger
        .set_route_admission(Route::SolToRhn, false, None)
        .unwrap();

    // Robinhood reserve paused.
    ledger
        .set_paused(ReserveDirection::RobinhoodReserve, true, Some("x"))
        .unwrap();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            3,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
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
    ledger
        .set_paused(ReserveDirection::RobinhoodReserve, false, None)
        .unwrap();

    // Capacity: more than the reserve can carry.
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            4,
            sol_to_rhn_amounts(5_000_000_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
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
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve).1,
        0
    );
}

#[test]
fn sol_to_rhn_payout_finality_moves_the_robinhood_reserve_and_completion_settles() {
    let mut ledger = ledger_with_every_reserve();
    let SolFoldOutcome::FoldedFinalized { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            0,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            true,
            None,
            300,
        )
        .unwrap()
    else {
        panic!()
    };
    let before_robinhood = reserve_row(&ledger, ReserveDirection::RobinhoodReserve);
    let before_solana = reserve_row(&ledger, ReserveDirection::SolanaReserve);

    // A finalized payout: DestinationConfirmed, NOT Settled.
    ledger
        .mark_robinhood_payout_settled(request_id, 500)
        .unwrap();
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::DestinationConfirmed);
    let after_robinhood = reserve_row(&ledger, ReserveDirection::RobinhoodReserve);
    assert_eq!(after_robinhood.0, before_robinhood.0 - 477_500_000);
    assert_eq!(after_robinhood.1, before_robinhood.1 - 477_500_000);
    assert_eq!(after_robinhood.2, before_robinhood.2 - 477_500_000);
    assert_eq!(after_robinhood.3, before_robinhood.3 + 477_500_000);
    assert_eq!(after_robinhood.4, before_robinhood.4);
    let after_solana = reserve_row(&ledger, ReserveDirection::SolanaReserve);
    assert_eq!(
        after_solana.4,
        before_solana.4 + 22_500_000,
        "fee on the SOURCE row"
    );
    assert_eq!(after_solana.0, before_solana.0);
    // Idempotent.
    ledger
        .mark_robinhood_payout_settled(request_id, 501)
        .unwrap();
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve),
        after_robinhood
    );

    // Settling requires the completion to have been SUBMITTED, and
    // submitting requires a finalized payout row to hang it on.
    assert!(matches!(
        ledger.mark_robinhood_payout_completion_confirmed(request_id, 600),
        Err(crate::ledger::LedgerError::CompletionNotSubmitted(_))
    ));
    assert!(
        ledger
            .record_robinhood_payout_completion_submitted(request_id, [0x55; 64], 550)
            .is_err(),
        "no finalized Payout row exists yet"
    );
    assert_eq!(
        ledger
            .robinhood_payout_completion_submission(request_id)
            .unwrap(),
        None
    );
    seed_finalized_payout_row(&ledger, request_id);
    ledger
        .record_robinhood_payout_completion_submitted(request_id, [0x55; 64], 550)
        .unwrap();
    assert_eq!(
        ledger
            .robinhood_payout_completion_submission(request_id)
            .unwrap(),
        Some(([0x55; 64], 550))
    );
    ledger
        .mark_robinhood_payout_completion_confirmed(request_id, 600)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    // Nothing moved on the way to Settled: the value left at finality.
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::RobinhoodReserve),
        after_robinhood
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::SolanaReserve),
        after_solana
    );
    ledger
        .mark_robinhood_payout_completion_confirmed(request_id, 601)
        .unwrap();
}

/// A `Finalized` Robinhood payout operation row for `request_id`, as the
/// settlement engine would leave it.
fn seed_finalized_payout_row(ledger: &Ledger, request_id: i64) {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, recipient, amount_robinhood, signer_epoch, expiry,
                 auth_digest, submitter, nonce, envelope, raw_tx, state, tx_hash,
                 first_broadcast_at, broadcast_attempts, receipt_status,
                 receipt_block_number, confirmations, finalized_at, created_at, updated_at)
             VALUES ('Payout', ?1, 'SolToRhn', ?2, 4663, 1, ?3, ?4, ?5, 1, 9999, ?3,
                     ?4, 0, 'eip1559', X'02', 'Finalized', ?6, 300, 1, 1, 200, 12, 500,
                     100, 500)",
            rusqlite::params![
                request_id,
                &BRIDGE.to_bytes()[..],
                &[0x01u8; 32][..],
                &EVM_RECIPIENT[..],
                &[0u8; 32][..],
                &[0x9au8; 32][..],
            ],
        )
        .expect("seeds a finalized payout row");
}

#[test]
fn glc_to_rhn_payout_finality_still_settles_in_one_step() {
    // The pre-existing route keeps its exact shape: payout finality IS
    // settlement, both transitions fire together, fee on Goldcoin.
    let mut ledger = ledger_with_every_reserve();
    let fb = compute_fee_at_bps(CanonicalAtomic(500_000_000), BRIDGE_FEE_BPS).unwrap();
    let request_id = match ledger
        .create_request(
            Direction::GlcToRhn,
            RequestAmounts {
                gross_atomic: fb.gross.0,
                fee_bps: fb.fee_bps,
                fee_atomic: fb.fee.0,
                net_atomic: fb.net.0,
                net_destination_atomic: fb.net.0,
            },
            &EVM_RECIPIENT,
            None,
            3600,
            100,
        )
        .unwrap()
    {
        crate::ledger::CreateRequestOutcome::Reserved { request_id } => request_id,
        other => panic!("{other:?}"),
    };
    ledger
        .record_glc_deposit_observed(request_id, [0xaa; 32], 0, fb.gross.0, 10, [0; 32], 110)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 120).unwrap();
    let before_goldcoin = reserve_row(&ledger, ReserveDirection::GoldcoinReserve);
    ledger
        .mark_robinhood_payout_settled(request_id, 500)
        .unwrap();
    assert_eq!(
        ledger.get_request(request_id).unwrap().unwrap().state,
        RequestState::Settled
    );
    assert_eq!(
        reserve_row(&ledger, ReserveDirection::GoldcoinReserve).4,
        before_goldcoin.4 + fb.fee.0 as i64
    );
}

// =====================================================================
// Refund guards
// =====================================================================

#[test]
fn a_parked_sol_to_rhn_request_is_refundable_on_solana_by_the_same_checks_as_sol_to_glc() {
    let mut ledger = ledger_with_every_reserve();
    let SolFoldOutcome::FoldedManualReview { request_id } = ledger
        .fold_sol_deposit_to_robinhood(
            0,
            sol_to_rhn_amounts(500_000_000),
            [0x11; 32],
            Some(EVM_RECIPIENT),
            EVM_RECIPIENT_TEXT.as_bytes(),
            false,
            None,
            300,
        )
        .unwrap()
    else {
        panic!()
    };
    let checks = ledger.solana_refund_db_checks(request_id).unwrap();
    assert!(checks.direction_ok, "SolToRhn is Solana-sourced");
    assert_eq!(checks.first_failure_for_begin(), None, "{checks:?}");

    // Once a Robinhood payout row exists for it, the refund is refused —
    // the same "a destination payout already exists" guard the Goldcoin
    // payout row trips for SolToGlc.
    seed_finalized_payout_row(&ledger, request_id);
    let checks = ledger.solana_refund_db_checks(request_id).unwrap();
    assert!(!checks.no_goldcoin_payout);
    assert!(checks.first_failure_for_begin().is_some());
}

#[test]
fn a_cross_route_resume_refuses_a_refund_lifecycle_and_a_non_recoverable_reason() {
    let mut ledger = ledger_with_every_reserve();
    let row = rhn_to_sol_observation(0, 500_000_000, SOL_RECIPIENT.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let FoldOutcome::FoldedManualReview { request_id } = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        crate::amount_conversion::CanonicalAtomic(1),
        MINT_DECIMALS,
        false,
        300,
    )
    .unwrap() else {
        panic!()
    };
    // `route_disabled_at_fold` is refundable, not resumable.
    let err = ledger
        .resume_manual_review_cross_route(Direction::RhnToSol, request_id, "x", "cli:test", 400)
        .unwrap_err();
    assert!(
        err.to_string().contains("not a known recoverable reason"),
        "{err}"
    );
    // The wrong direction is refused by direction.
    assert!(ledger
        .resume_manual_review_cross_route(Direction::SolToRhn, request_id, "x", "cli:test", 400)
        .is_err());
    // A refund row, once written, is permanent.
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_transactions
                (kind, request_id, route, bridge_contract, chain_id, action,
                 contract_request_id, obligation_index, recipient, amount_robinhood,
                 signer_epoch, expiry, auth_digest, state, created_at, updated_at)
             VALUES ('Refund', ?1, 'RhnToSol', ?2, 4663, 2, ?3, 0, ?4, ?5, 1, 9999, ?3,
                     'Authorizing', 100, 100)",
            rusqlite::params![
                request_id,
                &BRIDGE.to_bytes()[..],
                &[0x02u8; 32][..],
                &[0x33u8; 20][..],
                &row.observation.amount_robinhood_atomic[..],
            ],
        )
        .unwrap();
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'reserve_paused_at_fold' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    assert!(matches!(
        ledger
            .resume_manual_review_cross_route(Direction::RhnToSol, request_id, "x", "cli:test", 400)
            .unwrap_err(),
        crate::ledger::LedgerError::RefundLifecycleExists { .. }
    ));
}
