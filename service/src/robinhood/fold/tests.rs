//! Fold tests: turning a FINALIZED Robinhood observation into exactly one
//! bridge request.

use super::*;
use crate::amount_conversion::BRIDGE_FEE_BPS;
use crate::ledger::{
    Direction, RequestState, RobinhoodDepositObservation, RobinhoodFinality,
    RobinhoodObservationRow,
};
use crate::robinhood::testkit::BRIDGE;

const CANONICAL_SCALE: u128 = 10_000_000_000;

fn ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().expect("an in-memory ledger");
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            1_000_000_000_000,
            0,
            1_000_000_000_000,
            500_000_000_000,
            250_000_000_000,
            100,
        )
        .expect("configures the Goldcoin reserve");
    ledger
}

/// A Goldcoin testnet P2PKH address the payout builder would accept.
fn destination() -> String {
    crate::goldcoin::address::encode_p2pkh(&[0x42; 20], crate::goldcoin::address::Network::Testnet)
}

fn observation(index: u64, canonical: u64, destination_bytes: Vec<u8>) -> RobinhoodObservationRow {
    let robinhood = u128::from(canonical) * CANONICAL_SCALE;
    RobinhoodObservationRow {
        id: index as i64 + 1,
        observation: RobinhoodDepositObservation {
            source_contract: BRIDGE.to_bytes(),
            obligation_index: index,
            route: Route::RhnToGlc,
            depositor: [0x33; 20],
            destination: destination_bytes,
            amount_robinhood_atomic: crate::evm::EvmU256::from_u128(robinhood).to_be_bytes(),
            amount_canonical_atomic: canonical,
            // Distinct per obligation, because `ux_robinhood_log_identity`
            // guards `(tx_hash, log_index)` independently of the durable
            // identity — one log can never claim two obligation indexes.
            tx_hash: {
                let mut h = [0xaa; 32];
                h[0] = index as u8;
                h
            },
            log_index: 0,
            block_number: 500,
            block_hash: [0xbb; 32],
        },
        finality: RobinhoodFinality::Final,
        observed_at: 100,
        finalized_at: Some(200),
        reorged_at: None,
    }
}

/// Inserts the observation so the fold has a row to link back to.
fn store(ledger: &Ledger, row: &RobinhoodObservationRow) {
    ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO robinhood_deposit_observations
                (id, source_chain, source_contract, source_obligation_index, contract_route_id,
                 route, depositor, destination, amount_robinhood_atomic,
                 amount_canonical_atomic, tx_hash, log_index, block_number, block_hash,
                 finality, observed_at, finalized_at)
             VALUES (?1, 'robinhood', ?2, ?3, 2, 'RhnToGlc', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
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

fn network() -> crate::goldcoin::address::Network {
    crate::goldcoin::address::Network::Testnet
}

#[test]
fn a_finalized_deposit_folds_exactly_once() {
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);

    let first = fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300).unwrap();
    let request_id = match first {
        FoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("expected a payable fold, got {other:?}"),
    };

    // Every subsequent attempt resumes rather than duplicating — the
    // durable identity guard, not a prior read.
    for _ in 0..3 {
        assert_eq!(
            fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 400).unwrap(),
            FoldOutcome::AlreadyFolded { request_id }
        );
    }
    let count: i64 = ledger
        .conn_for_tests()
        .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.direction, Direction::RhnToGlc);
    assert_eq!(request.state, RequestState::SourceFinalized);
    assert_eq!(request.source_obligation_index, Some(0));
    assert_eq!(
        request.source_contract.as_deref(),
        Some(&BRIDGE.to_bytes()[..])
    );
    // The recipient is the destination address bytes, exactly as
    // `SolToGlc` stores them.
    assert_eq!(request.recipient, destination().into_bytes());
    // The normal fee policy in canonical units.
    assert_eq!(request.fee_bps, crate::amount_conversion::BRIDGE_FEE_BPS);
    assert_eq!(
        request.gross_amount_atomic,
        request.fee_amount_atomic + request.net_amount_atomic
    );
    assert_eq!(request.net_destination_atomic, request.net_amount_atomic);
}

#[test]
fn a_deposit_on_a_closed_route_is_recorded_and_parked_rather_than_dropped() {
    // The most important decision in this module: a deposit that already
    // landed cannot be un-landed by a flag on this side. Declining to
    // record it would leave real money in the contract with no ledger
    // row, no accounting and no refund path.
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);

    let outcome =
        fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, false, 300).unwrap();
    let request_id = match outcome {
        FoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a parked fold, got {other:?}"),
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::ManualReview);
    assert_eq!(
        request.manual_review_note.as_deref(),
        Some("route_disabled_at_fold")
    );
    // Nothing is reserved for a parked request.
    let (_, _, reserved, pending) = ledger
        .reserve_snapshot(crate::ledger::ReserveDirection::GoldcoinReserve)
        .unwrap();
    assert_eq!((reserved, pending), (0, 0));
}

#[test]
fn an_undeliverable_destination_is_folded_and_parked_with_an_explicit_reason() {
    // The deposit is real and irreversible; it must be refunded on
    // Robinhood rather than paid out to a guess.
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, b"not a goldcoin address".to_vec());
    store(&ledger, &row);

    let outcome =
        fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300).unwrap();
    let request_id = match outcome {
        FoldOutcome::FoldedManualReview { request_id } => request_id,
        other => panic!("expected a parked fold, got {other:?}"),
    };
    let request = ledger.get_request(request_id).unwrap().unwrap();
    assert!(request
        .manual_review_note
        .unwrap()
        .contains("undeliverable destination"));
    // The RAW payload is kept: it is the evidence a refund decision rests
    // on, including when it is unusable.
    assert_eq!(request.recipient, b"not a goldcoin address".to_vec());
}

#[test]
fn a_mainnet_address_is_undeliverable_on_a_testnet_deployment() {
    let mut ledger = ledger();
    let mainnet = crate::goldcoin::address::encode_p2pkh(
        &[0x42; 20],
        crate::goldcoin::address::Network::Mainnet,
    );
    let row = observation(0, 1_000_000_000, mainnet.into_bytes());
    store(&ledger, &row);
    assert!(matches!(
        fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300).unwrap(),
        FoldOutcome::FoldedManualReview { .. }
    ));
}

#[test]
fn a_provisional_observation_is_refused() {
    // A provisional deposit can still be reorged away, and folding one
    // would create an obligation against a deposit that never happened.
    let mut ledger = ledger();
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.finality = RobinhoodFinality::Provisional;
    assert!(matches!(
        fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300),
        Err(FoldError::NotFinal { .. })
    ));
}

#[test]
fn a_non_executable_route_is_refused() {
    let mut ledger = ledger();
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.observation.route = Route::RhnToSol;
    assert!(matches!(
        fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300),
        Err(FoldError::UnsupportedRoute { .. })
    ));
}

// -------------------------------------------------------- amounts ----

#[test]
fn an_amount_that_is_not_an_exact_multiple_of_the_scale_is_refused() {
    // The contract refuses a non-canonical deposit on-chain, so observing
    // one means this service and the contract disagree about what was
    // deposited — not a rounding decision to make.
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    let inexact = u128::from(1_000_000_000u64) * CANONICAL_SCALE + 1;
    row.observation.amount_robinhood_atomic = crate::evm::EvmU256::from_u128(inexact).to_be_bytes();
    assert!(matches!(
        resolve_amounts(&row, BRIDGE_FEE_BPS),
        Err(FoldError::NotCanonical { .. })
    ));
}

#[test]
fn the_two_recorded_amounts_must_agree() {
    // The event carries both, and the decoder already cross-checked them.
    // Deriving again HERE is what makes a single stored number
    // trustworthy at the moment it becomes an entitlement.
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.observation.amount_canonical_atomic = 999_999_999;
    assert!(matches!(
        resolve_amounts(&row, BRIDGE_FEE_BPS),
        Err(FoldError::AmountDisagreement {
            recorded: 999_999_999,
            derived: 1_000_000_000,
            ..
        })
    ));
}

#[test]
fn a_word_too_large_for_the_amount_model_is_refused_rather_than_truncated() {
    let mut row = observation(0, 1_000_000_000, destination().into_bytes());
    row.observation.amount_robinhood_atomic = crate::evm::EvmU256::MAX.to_be_bytes();
    assert!(matches!(
        resolve_amounts(&row, BRIDGE_FEE_BPS),
        Err(FoldError::NotCanonical { .. })
    ));
}

#[test]
fn the_conversion_is_exact_across_a_range_of_real_amounts() {
    for whole in [1u64, 100, 20_000] {
        let canonical = whole * 100_000_000;
        let row = observation(0, canonical, destination().into_bytes());
        let amounts = resolve_amounts(&row, BRIDGE_FEE_BPS).expect("an exact amount");
        assert_eq!(amounts.gross_canonical, canonical);
        assert_eq!(
            amounts.gross_canonical,
            amounts.fee_canonical + amounts.net_canonical,
            "gross == fee + net is structural"
        );
        assert_eq!(amounts.fee_bps, crate::amount_conversion::BRIDGE_FEE_BPS);
        // And the net widens back to Robinhood units exactly.
        crate::amount_conversion::CanonicalAtomic(amounts.net_canonical)
            .to_robinhood()
            .expect("the net must be exactly representable at 18dp");
    }
}

#[test]
fn folding_links_the_observation_to_its_request_and_only_one_can_claim_it() {
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    let request_id = fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300)
        .unwrap()
        .request_id();

    let linked = ledger
        .robinhood_observation_for_request(request_id)
        .unwrap()
        .expect("the observation links back to its request");
    assert_eq!(linked.observation.obligation_index, 0);

    // A second observation cannot claim the same request.
    let other = observation(1, 1_000_000_000, destination().into_bytes());
    store(&ledger, &other);
    let forced = ledger.conn_for_tests().execute(
        "UPDATE robinhood_deposit_observations SET folded_request_id = ?1 WHERE id = ?2",
        rusqlite::params![request_id, other.id],
    );
    assert!(
        forced.is_err(),
        "two observations must not be able to claim one request"
    );
}

#[test]
fn only_unfolded_final_observations_are_offered_to_the_fold_phase() {
    let mut ledger = ledger();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    assert_eq!(
        ledger
            .unfolded_final_robinhood_observations()
            .unwrap()
            .len(),
        1
    );
    fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300).unwrap();
    assert_eq!(
        ledger
            .unfolded_final_robinhood_observations()
            .unwrap()
            .len(),
        0,
        "a folded observation is not offered again"
    );
}

#[test]
fn a_thin_reserve_parks_the_deposit_instead_of_refusing_it() {
    // Same posture as `fold_sol_deposit`: the deposit is recorded and
    // made visible rather than dropped, and pays out nothing.
    let mut ledger = Ledger::open_in_memory().unwrap();
    ledger
        .configure_reserve(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            1,
            0,
            10,
            5,
            2,
            100,
        )
        .unwrap();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    let outcome =
        fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300).unwrap();
    match outcome {
        FoldOutcome::FoldedManualReview { request_id } => {
            let request = ledger.get_request(request_id).unwrap().unwrap();
            assert_eq!(
                request.manual_review_note.as_deref(),
                Some("insufficient_capacity_at_fold")
            );
        }
        other => panic!("expected a parked fold, got {other:?}"),
    }
}

#[test]
fn a_paused_goldcoin_reserve_parks_the_deposit() {
    let mut ledger = ledger();
    ledger
        .set_paused(
            crate::ledger::ReserveDirection::GoldcoinReserve,
            true,
            Some("incident"),
        )
        .unwrap();
    let row = observation(0, 1_000_000_000, destination().into_bytes());
    store(&ledger, &row);
    match fold_observation(&mut ledger, &row, network(), BRIDGE_FEE_BPS, true, 300).unwrap() {
        FoldOutcome::FoldedManualReview { request_id } => {
            assert_eq!(
                ledger
                    .get_request(request_id)
                    .unwrap()
                    .unwrap()
                    .manual_review_note
                    .as_deref(),
                Some("reserve_paused_at_fold")
            );
        }
        other => panic!("expected a parked fold, got {other:?}"),
    }
}

/// The Robinhood launch rate is applied to a Robinhood deposit, and the
/// snapshot stored on the request is that rate — not the compiled-in one.
#[test]
fn a_robinhood_deposit_prices_at_the_rate_it_is_given() {
    const ROBINHOOD_FEE_BPS: u64 = 600;
    // 100 GLC in canonical 8-decimal units.
    let row = observation(1, 10_000_000_000, destination().into_bytes());
    let at_robinhood = resolve_amounts(&row, ROBINHOOD_FEE_BPS).expect("an exact amount");
    let at_global = resolve_amounts(&row, BRIDGE_FEE_BPS).expect("an exact amount");

    assert_eq!(at_robinhood.fee_bps, ROBINHOOD_FEE_BPS);
    assert_eq!(at_global.fee_bps, BRIDGE_FEE_BPS);
    assert_eq!(at_robinhood.gross_canonical, at_global.gross_canonical);

    // 6% of the gross, floored, and net derived by subtraction.
    assert_eq!(
        at_robinhood.fee_canonical,
        at_robinhood.gross_canonical * ROBINHOOD_FEE_BPS / 10_000
    );
    assert_eq!(
        at_robinhood.net_canonical,
        at_robinhood.gross_canonical - at_robinhood.fee_canonical
    );

    // And it is genuinely a different, larger fee than the global rate —
    // the whole point of a per-chain policy.
    const _: () = assert!(BRIDGE_FEE_BPS < ROBINHOOD_FEE_BPS);
    assert!(at_robinhood.fee_canonical > at_global.fee_canonical);
}

/// A rate the protocol has never charged fails closed at fold time rather
/// than creating a request that could never settle.
#[test]
fn an_unknown_rate_refuses_to_fold() {
    let row = observation(1, 10_000_000_000, destination().into_bytes());
    assert!(matches!(
        resolve_amounts(&row, 450),
        Err(FoldError::Fee { .. })
    ));
}
