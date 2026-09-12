//! Route-by-route coverage of the rolling-24-hour wallet uniqueness rule
//! (the module docs above), driven through each route's REAL admission
//! path — the four folds for the contract-sourced routes,
//! `create_request_from` + `record_glc_deposit_observed_from` for the two
//! Goldcoin-sourced ones — never through the window query directly.
//!
//! Every scenario the rule promises is run for every one of the six
//! routes by the same harness, so a route cannot quietly get a weaker
//! version of the rule than its neighbours:
//!
//! 1. the same source wallet twice inside 24h is blocked;
//! 2. the same destination wallet twice inside 24h is blocked;
//! 3. a different source AND destination is admitted;
//! 4. the same wallets after the window has elapsed are admitted again;
//! 5. two simultaneous attempts (two connections, one instant) admit
//!    exactly one;
//! 6. a duplicate whose deposit is already on-chain is recorded, parked
//!    in `ManualReview` under the explicit reason, and takes no payout
//!    capacity;
//! 7. replaying the source-chain event is idempotent — the same
//!    obligation/observation/outpoint folds to the same row and is never
//!    reinterpreted as a second attempt.

use super::*;
use crate::ledger::{
    CreateRequestOutcome, GlcObservationOutcome, RequestAmounts, RequestState, ReserveDirection,
    ResumeManualReviewOutcome, RobinhoodDepositObservation, SolFoldOutcome,
};
use crate::robinhood::fold::FoldOutcome;
use crate::routes::Route;

const T0: i64 = 1_000_000;
const WINDOW: i64 = Ledger::WALLET_WINDOW_SECS;
const AMOUNT: u64 = 50_000;
const CONTRACT: [u8; 20] = [0xC0; 20];

fn amounts() -> RequestAmounts {
    RequestAmounts {
        gross_atomic: AMOUNT,
        fee_bps: 0,
        fee_atomic: 0,
        net_atomic: AMOUNT,
        net_destination_atomic: AMOUNT,
    }
}

fn configure(ledger: &mut Ledger) {
    for reserve in ReserveDirection::ALL {
        ledger
            .configure_reserve(
                reserve,
                100_000_000,
                1_000_000,
                50_000_000,
                20_000_000,
                15_000_000,
                1,
            )
            .unwrap();
    }
}

fn fresh_ledger() -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger);
    ledger
}

/// A distinct wallet on `chain` per `tag`, spelled the way that chain's
/// rows record it: Goldcoin address text, a 32-byte pubkey, a non-zero
/// 20-byte EVM address.
fn wallet(chain: Chain, tag: u8) -> Vec<u8> {
    match chain {
        Chain::Goldcoin => crate::goldcoin::address::encode_p2pkh(
            &[tag; 20],
            crate::goldcoin::address::Network::Testnet,
        )
        .into_bytes(),
        Chain::Solana => vec![tag; 32],
        Chain::Robinhood => {
            let mut address = [tag; 20];
            address[0] = 0xEE;
            address.to_vec()
        }
    }
}

fn source(direction: Direction, tag: u8) -> Vec<u8> {
    wallet(direction.source_chain(), tag)
}

fn destination(direction: Direction, tag: u8) -> Vec<u8> {
    wallet(direction.destination_chain(), tag)
}

/// What one bridge attempt produced, uniformly across the six routes.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// A row in a payable state (`SourceFinalized` for a fold,
    /// `Confirming` for a funded Goldcoin deposit).
    Admitted(i64),
    /// The deposit is on-chain, recorded, and parked in `ManualReview`
    /// under `reason` — never paid.
    Parked { request_id: i64, reason: String },
    /// Nothing is on-chain yet and the request was refused outright
    /// (`POST /transfers` on a Goldcoin-sourced route): no row at all.
    Refused { reason: String },
}

impl Attempt {
    fn reason(&self) -> Option<&str> {
        match self {
            Attempt::Admitted(_) => None,
            Attempt::Parked { reason, .. } | Attempt::Refused { reason } => Some(reason),
        }
    }
}

fn note_of(ledger: &Ledger, request_id: i64) -> String {
    ledger
        .get_request(request_id)
        .unwrap()
        .unwrap()
        .manual_review_note
        .expect("a parked row carries its reason")
}

fn state_of(ledger: &Ledger, request_id: i64) -> RequestState {
    ledger.get_request(request_id).unwrap().unwrap().state
}

fn reserved_liquidity(ledger: &Ledger, reserve: ReserveDirection) -> i64 {
    ledger
        .conn_for_tests()
        .query_row(
            "SELECT reserved_liquidity FROM reserve_ledger WHERE direction = ?1",
            [reserve],
            |r| r.get(0),
        )
        .unwrap()
}

fn observation(
    route: Route,
    seq: u64,
    depositor: &[u8],
    destination: &[u8],
) -> RobinhoodDepositObservation {
    let mut tx_hash = [0xAA; 32];
    tx_hash[0] = seq as u8;
    tx_hash[1] = (seq >> 8) as u8;
    RobinhoodDepositObservation {
        source_contract: CONTRACT,
        obligation_index: seq,
        route,
        depositor: <[u8; 20]>::try_from(depositor).unwrap(),
        destination: destination.to_vec(),
        amount_robinhood_atomic: crate::evm::EvmU256::from_u128(
            u128::from(AMOUNT) * 10_000_000_000,
        )
        .to_be_bytes(),
        amount_canonical_atomic: AMOUNT,
        tx_hash,
        log_index: 0,
        block_number: 500 + seq,
        block_hash: [0xBB; 32],
    }
}

/// One bridge attempt on `direction` from `source` to `destination`,
/// through the route's real admission path. `seq` is the unique
/// source-chain identity of this attempt (obligation index, or the
/// Goldcoin deposit txid) — distinct per attempt unless a test is
/// deliberately replaying one.
fn attempt(
    ledger: &mut Ledger,
    direction: Direction,
    source: &[u8],
    destination: &[u8],
    seq: u64,
    now: i64,
) -> Attempt {
    match direction {
        Direction::GlcToSol | Direction::GlcToRhn => {
            // The UI path: the caller declares its funding address, so the
            // source window is consumed at creation; the deposit is then
            // observed from that same wallet.
            let created = ledger
                .create_request_from(
                    direction,
                    amounts(),
                    destination,
                    None,
                    Some(source),
                    3_600,
                    now,
                )
                .unwrap();
            let request_id = match created {
                CreateRequestOutcome::Reserved { request_id } => request_id,
                CreateRequestOutcome::WalletLimited { eligibility } => {
                    return Attempt::Refused {
                        reason: eligibility.manual_review_note().unwrap().to_string(),
                    }
                }
                other => panic!("{direction:?}: unexpected create outcome {other:?}"),
            };
            fund_glc(ledger, request_id, source, seq, now)
        }
        Direction::SolToGlc => match ledger
            .fold_sol_deposit(
                seq,
                amounts(),
                source.try_into().unwrap(),
                destination,
                None,
                now,
            )
            .unwrap()
        {
            SolFoldOutcome::FoldedFinalized { request_id }
            | SolFoldOutcome::AlreadyFolded { request_id } => Attempt::Admitted(request_id),
            SolFoldOutcome::FoldedManualReview { request_id } => Attempt::Parked {
                request_id,
                reason: note_of(ledger, request_id),
            },
        },
        Direction::SolToRhn => match ledger
            .fold_sol_deposit_to_robinhood(
                seq,
                amounts(),
                source.try_into().unwrap(),
                Some(destination.try_into().unwrap()),
                destination,
                true,
                None,
                now,
            )
            .unwrap()
        {
            SolFoldOutcome::FoldedFinalized { request_id }
            | SolFoldOutcome::AlreadyFolded { request_id } => Attempt::Admitted(request_id),
            SolFoldOutcome::FoldedManualReview { request_id } => Attempt::Parked {
                request_id,
                reason: note_of(ledger, request_id),
            },
        },
        Direction::RhnToGlc | Direction::RhnToSol => {
            let route = Route::from(direction);
            let obs = observation(route, seq, source, destination);
            ledger
                .robinhood_record_final_observation(&obs, now)
                .unwrap();
            let row = ledger
                .robinhood_observation_by_source(CONTRACT, seq)
                .unwrap()
                .expect("the observation was just recorded");
            match ledger
                .fold_robinhood_deposit(&row, amounts(), Some(destination), true, None, now)
                .unwrap()
            {
                FoldOutcome::FoldedFinalized { request_id }
                | FoldOutcome::AlreadyFolded { request_id } => Attempt::Admitted(request_id),
                FoldOutcome::FoldedManualReview { request_id } => Attempt::Parked {
                    request_id,
                    reason: note_of(ledger, request_id),
                },
            }
        }
    }
}

/// Observes the Goldcoin deposit funding `request_id`, traced to
/// `funding_wallet` — what `goldcoin::indexer` does once the deposit
/// transaction is in a block.
fn fund_glc(
    ledger: &mut Ledger,
    request_id: i64,
    funding_wallet: &[u8],
    seq: u64,
    now: i64,
) -> Attempt {
    let mut txid = [0x77; 32];
    txid[0] = seq as u8;
    txid[1] = (seq >> 8) as u8;
    match ledger
        .record_glc_deposit_observed_from(
            request_id,
            txid,
            0,
            AMOUNT,
            10 + seq as i64,
            [0x55; 32],
            &[funding_wallet.to_vec()],
            now,
        )
        .unwrap()
    {
        GlcObservationOutcome::Recorded
        | GlcObservationOutcome::AlreadyRecorded
        | GlcObservationOutcome::LateDepositRecreated => Attempt::Admitted(request_id),
        GlcObservationOutcome::WalletLimited { reason, .. } => Attempt::Parked {
            request_id,
            reason: reason.to_string(),
        },
        other => panic!("unexpected observation outcome {other:?}"),
    }
}

fn admitted(attempt: Attempt, direction: Direction, what: &str) -> i64 {
    match attempt {
        Attempt::Admitted(id) => id,
        other => panic!("{direction:?}: {what} must be admitted, got {other:?}"),
    }
}

// ------------------------------------------------------------ the rule --

#[test]
fn the_same_source_wallet_twice_inside_24h_is_blocked_on_every_route() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let src = source(direction, 1);
        admitted(
            attempt(
                &mut ledger,
                direction,
                &src,
                &destination(direction, 1),
                1,
                T0,
            ),
            direction,
            "the first attempt from a fresh wallet",
        );
        // Same source, a DIFFERENT destination — only the source rule
        // can be doing the work.
        let second = attempt(
            &mut ledger,
            direction,
            &src,
            &destination(direction, 2),
            2,
            T0 + 3_600,
        );
        assert_eq!(
            second.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
            "{direction:?}: a second attempt from the same source inside 24h must be blocked, \
             got {second:?}"
        );
        // The last second of the window still blocks.
        let edge = attempt(
            &mut ledger,
            direction,
            &src,
            &destination(direction, 3),
            3,
            T0 + WINDOW - 1,
        );
        assert_eq!(
            edge.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
            "{direction:?}"
        );
    }
}

#[test]
fn the_same_destination_wallet_twice_inside_24h_is_blocked_on_every_route() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let dst = destination(direction, 1);
        admitted(
            attempt(&mut ledger, direction, &source(direction, 1), &dst, 1, T0),
            direction,
            "the first attempt to a fresh destination",
        );
        // A DIFFERENT source, the same destination.
        let second = attempt(
            &mut ledger,
            direction,
            &source(direction, 2),
            &dst,
            2,
            T0 + 3_600,
        );
        assert_eq!(
            second.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT),
            "{direction:?}: a second attempt to the same destination inside 24h must be \
             blocked, got {second:?}"
        );
        let edge = attempt(
            &mut ledger,
            direction,
            &source(direction, 3),
            &dst,
            3,
            T0 + WINDOW - 1,
        );
        assert_eq!(
            edge.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT),
            "{direction:?}"
        );
    }
}

#[test]
fn the_source_reason_outranks_the_destination_reason_when_both_apply() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let (src, dst) = (source(direction, 1), destination(direction, 1));
        admitted(
            attempt(&mut ledger, direction, &src, &dst, 1, T0),
            direction,
            "the first",
        );
        let both = attempt(&mut ledger, direction, &src, &dst, 2, T0 + 10);
        assert_eq!(
            both.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
            "{direction:?}: source first, matching the eligibility API's precedence"
        );
    }
}

#[test]
fn a_different_source_and_destination_is_admitted_on_every_route() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        admitted(
            attempt(
                &mut ledger,
                direction,
                &source(direction, 1),
                &destination(direction, 1),
                1,
                T0,
            ),
            direction,
            "the first",
        );
        admitted(
            attempt(
                &mut ledger,
                direction,
                &source(direction, 2),
                &destination(direction, 2),
                2,
                T0 + 10,
            ),
            direction,
            "a second attempt from a fresh source to a fresh destination",
        );
        admitted(
            attempt(
                &mut ledger,
                direction,
                &source(direction, 3),
                &destination(direction, 3),
                3,
                T0 + 10,
            ),
            direction,
            "a third, at the very same instant",
        );
    }
}

#[test]
fn the_same_wallets_are_admitted_again_once_the_window_has_elapsed() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let (src, dst) = (source(direction, 1), destination(direction, 1));
        admitted(
            attempt(&mut ledger, direction, &src, &dst, 1, T0),
            direction,
            "the first",
        );
        // `created_at + WINDOW` is the first instant the window has
        // fully elapsed (the query is strictly-greater-than), on BOTH
        // legs at once.
        admitted(
            attempt(&mut ledger, direction, &src, &dst, 2, T0 + WINDOW),
            direction,
            "the same source AND destination exactly 24h later",
        );
        // And that second attempt starts a window of its own.
        let third = attempt(&mut ledger, direction, &src, &dst, 3, T0 + WINDOW + 10);
        assert_eq!(
            third.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
            "{direction:?}: a rolling window, not a daily bucket"
        );
    }
}

// ------------------------------------------- already on-chain: parked --

#[test]
fn an_already_on_chain_duplicate_is_parked_in_manual_review_and_takes_no_payout_capacity() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let reserve = direction.destination_reserve();
        let (src, dst) = (source(direction, 1), destination(direction, 1));
        let first = admitted(
            attempt(&mut ledger, direction, &src, &dst, 1, T0),
            direction,
            "the first",
        );
        let reserved_after_first = reserved_liquidity(&ledger, reserve);
        assert!(
            reserved_after_first > 0,
            "{direction:?}: the admitted attempt holds capacity"
        );

        // The duplicate's deposit is on-chain. On the fold routes that is
        // what a fold IS; on the Goldcoin-sourced routes the request is
        // created WITHOUT declaring a source (nothing to refuse up
        // front) and the deposit then arrives, traced to the busy wallet.
        let parked_id = match direction {
            Direction::GlcToSol | Direction::GlcToRhn => {
                let CreateRequestOutcome::Reserved { request_id } = ledger
                    .create_request_from(
                        direction,
                        amounts(),
                        &destination(direction, 2),
                        None,
                        None,
                        3_600,
                        T0 + 60,
                    )
                    .unwrap()
                else {
                    panic!("{direction:?}: an undeclared request to a fresh destination is created")
                };
                let reserved_before_deposit = reserved_liquidity(&ledger, reserve);
                let funded = fund_glc(&mut ledger, request_id, &src, 2, T0 + 120);
                assert_eq!(
                    funded,
                    Attempt::Parked {
                        request_id,
                        reason: Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT.to_string()
                    },
                    "{direction:?}"
                );
                // The deposit's evidence is recorded exactly as an amount
                // mismatch's is, so the refund path can act on it.
                let row = ledger.get_request(request_id).unwrap().unwrap();
                assert!(
                    row.source_txid.is_some(),
                    "{direction:?}: outpoint recorded"
                );
                assert_eq!(
                    row.source_wallet.as_deref(),
                    Some(src.as_slice()),
                    "{direction:?}: the TRACED wallet is recorded"
                );
                let checks = ledger.glc_refund_db_checks(request_id).unwrap();
                assert!(
                    checks.reason_is_refundable,
                    "{direction:?}: refundable — {:?}",
                    checks.refusal
                );
                assert_eq!(
                    checks.durable_observed_amount_atomic,
                    Some(AMOUNT),
                    "{direction:?}"
                );
                // A Goldcoin-sourced park keeps its reservation (released
                // by the refund), exactly like every other Goldcoin park;
                // what it never does is advance to `Confirming`.
                assert_eq!(
                    reserved_liquidity(&ledger, reserve),
                    reserved_before_deposit,
                    "{direction:?}"
                );
                request_id
            }
            _ => {
                let second = attempt(
                    &mut ledger,
                    direction,
                    &src,
                    &destination(direction, 2),
                    2,
                    T0 + 60,
                );
                let Attempt::Parked { request_id, reason } = second else {
                    panic!("{direction:?}: expected a park, got {second:?}")
                };
                assert_eq!(reason, Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT);
                // A fold-time park takes NO reserve capacity.
                assert_eq!(
                    reserved_liquidity(&ledger, reserve),
                    reserved_after_first,
                    "{direction:?}"
                );
                let row = ledger.get_request(request_id).unwrap().unwrap();
                assert_eq!(
                    row.source_wallet.as_deref(),
                    Some(src.as_slice()),
                    "{direction:?}"
                );
                assert!(
                    row.source_finalized_at.is_some(),
                    "{direction:?}: the deposit is final"
                );
                request_id
            }
        };
        assert_eq!(
            state_of(&ledger, parked_id),
            RequestState::ManualReview,
            "{direction:?}"
        );
        assert_eq!(
            state_of(&ledger, first),
            if direction.source_is_goldcoin() {
                RequestState::Confirming
            } else {
                RequestState::SourceFinalized
            }
        );
        assert!(
            ledger.get_destination_txid(parked_id).unwrap().is_none(),
            "{direction:?}: no payout"
        );
        // The park is itself a blocker for the next arrival — that is
        // what makes a queue drain oldest-first.
        let third = attempt(
            &mut ledger,
            direction,
            &src,
            &destination(direction, 3),
            3,
            T0 + 200,
        );
        assert_eq!(
            third.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
            "{direction:?}"
        );
    }
}

/// The Goldcoin-sourced routes' late-deposit shape of the destination
/// rule: `A` expires unfunded, `B` to the same destination is admitted,
/// then `A`'s deposit arrives late. Whichever deposit is observed second
/// parks — here `A`, under the destination reason — so two live requests
/// never share a destination.
#[test]
fn a_late_goldcoin_deposit_to_a_destination_reused_meanwhile_is_parked_not_paid() {
    for direction in [Direction::GlcToSol, Direction::GlcToRhn] {
        let mut ledger = fresh_ledger();
        let dst = destination(direction, 1);
        let CreateRequestOutcome::Reserved { request_id: a } = ledger
            .create_request_from(direction, amounts(), &dst, None, None, 100, T0)
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(ledger.expire_reservations(T0 + 101).unwrap(), 1);
        // `A` is Expired, which does not consume the window, so `B` is
        // admitted and funded.
        let b = admitted(
            attempt(
                &mut ledger,
                direction,
                &source(direction, 2),
                &dst,
                2,
                T0 + 200,
            ),
            direction,
            "B, after A expired",
        );
        // `A`'s late deposit: the reservation is recreated (capacity is
        // available) but the destination is now B's for 24 hours.
        let late = fund_glc(&mut ledger, a, &source(direction, 1), 1, T0 + 300);
        assert_eq!(
            late,
            Attempt::Parked {
                request_id: a,
                reason: Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT.to_string()
            },
            "{direction:?}"
        );
        assert_eq!(
            state_of(&ledger, b),
            RequestState::Confirming,
            "{direction:?}: B is untouched"
        );
    }
}

/// A Goldcoin deposit combining an input from a busy wallet is that
/// wallet's second attempt, whichever input it is; the FIRST input is
/// what the row records as its source.
#[test]
fn every_traced_goldcoin_input_is_checked_and_the_first_is_recorded() {
    let direction = Direction::GlcToSol;
    let mut ledger = fresh_ledger();
    let busy = source(direction, 1);
    admitted(
        attempt(
            &mut ledger,
            direction,
            &busy,
            &destination(direction, 1),
            1,
            T0,
        ),
        direction,
        "the first",
    );

    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request_from(
            direction,
            amounts(),
            &destination(direction, 2),
            None,
            None,
            3_600,
            T0 + 10,
        )
        .unwrap()
    else {
        panic!()
    };
    let fresh = source(direction, 9);
    let outcome = ledger
        .record_glc_deposit_observed_from(
            request_id,
            [0x02; 32],
            0,
            AMOUNT,
            12,
            [0x55; 32],
            &[fresh.clone(), busy.clone()],
            T0 + 20,
        )
        .unwrap();
    assert!(
        matches!(outcome, GlcObservationOutcome::WalletLimited { reason, .. } if reason == Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
        "{outcome:?}"
    );
    assert_eq!(
        ledger
            .get_request(request_id)
            .unwrap()
            .unwrap()
            .source_wallet,
        Some(fresh),
        "input 0 is the recorded source"
    );
}

/// A deposit whose inputs could not be traced at all (a coinbase-funded
/// deposit, or a test double with no inputs) is recorded without a
/// source-wallet check — the destination window still applies.
#[test]
fn an_untraceable_goldcoin_deposit_is_recorded_with_no_source_and_its_destination_still_checked() {
    let direction = Direction::GlcToRhn;
    let mut ledger = fresh_ledger();
    let dst = destination(direction, 1);
    let CreateRequestOutcome::Reserved { request_id: a } = ledger
        .create_request_from(direction, amounts(), &dst, None, None, 3_600, T0)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        ledger
            .record_glc_deposit_observed(a, [0x01; 32], 0, AMOUNT, 11, [0x55; 32], T0 + 10)
            .unwrap(),
        GlcObservationOutcome::Recorded
    );
    assert_eq!(ledger.get_request(a).unwrap().unwrap().source_wallet, None);
    // A second request to the same destination: refused at creation,
    // by the destination window alone.
    let refused = ledger
        .create_request_from(direction, amounts(), &dst, None, None, 3_600, T0 + 20)
        .unwrap();
    assert!(
        matches!(refused, CreateRequestOutcome::WalletLimited { eligibility } if eligibility.destination_retry_after == Some(T0 + WINDOW))
    );
}

// ------------------------------------------------------------- replay --

#[test]
fn replaying_the_source_event_is_idempotent_and_never_a_second_attempt() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let (src, dst) = (source(direction, 1), destination(direction, 1));
        let first = admitted(
            attempt(&mut ledger, direction, &src, &dst, 1, T0),
            direction,
            "the first",
        );
        let rows_before: i64 = ledger
            .conn_for_tests()
            .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
            .unwrap();
        let reserved_before = reserved_liquidity(&ledger, direction.destination_reserve());

        // The SAME source-chain event again, later, inside the window —
        // a restart re-observing it. It must resolve to the same row,
        // not be parked as the wallet's "second" attempt.
        let replay = match direction {
            Direction::GlcToSol | Direction::GlcToRhn => {
                fund_glc(&mut ledger, first, &src, 1, T0 + 600)
            }
            _ => attempt(&mut ledger, direction, &src, &dst, 1, T0 + 600),
        };
        assert_eq!(
            replay,
            Attempt::Admitted(first),
            "{direction:?}: replay resolves to the same row"
        );
        let rows_after: i64 = ledger
            .conn_for_tests()
            .query_row("SELECT COUNT(*) FROM bridge_requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows_after, rows_before, "{direction:?}: no new row");
        assert_eq!(
            reserved_liquidity(&ledger, direction.destination_reserve()),
            reserved_before,
            "{direction:?}: no double reservation"
        );
        assert_ne!(
            state_of(&ledger, first),
            RequestState::ManualReview,
            "{direction:?}"
        );
    }
}

// ---------------------------------------------------------- the race --

/// Two attempts sharing a wallet, from two independent connections onto
/// one database file, released at the same instant: exactly one is
/// admitted. The check and the write share one `BEGIN IMMEDIATE`
/// transaction, so the second writer — whichever it is — sees the
/// first's committed row.
#[test]
fn two_simultaneous_attempts_sharing_a_wallet_admit_exactly_one_on_every_route() {
    for direction in Direction::ALL {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        configure(&mut Ledger::open(&path).unwrap());

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let src = source(direction, 1);
        // Same source, different destinations (and vice versa would do
        // the same): the shared wallet is what the race is about.
        let handles: Vec<_> = (1..=2u64)
            .map(|seq| {
                let path = path.clone();
                let barrier = barrier.clone();
                let src = src.clone();
                let dst = destination(direction, seq as u8);
                std::thread::spawn(move || {
                    let mut ledger = Ledger::open(&path).unwrap();
                    barrier.wait();
                    attempt(&mut ledger, direction, &src, &dst, seq, T0)
                })
            })
            .collect();
        let outcomes: Vec<Attempt> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        let admitted_count = outcomes
            .iter()
            .filter(|o| matches!(o, Attempt::Admitted(_)))
            .count();
        assert_eq!(
            admitted_count, 1,
            "{direction:?}: exactly one admitted, got {outcomes:?}"
        );
        let blocked = outcomes
            .iter()
            .find(|o| !matches!(o, Attempt::Admitted(_)))
            .unwrap();
        assert_eq!(
            blocked.reason(),
            Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT),
            "{direction:?}: the loser is blocked by the winner's row"
        );
    }
}

// ------------------------------------------------ scope and spelling --

/// The six direction sets the window query scopes on, pinned to the
/// Rust predicates over every direction, so a seventh direction cannot
/// be missed by a literal.
#[test]
fn the_direction_scope_literals_match_the_chain_predicates() {
    for chain in Chain::ALL {
        for role in WalletRole::ALL {
            let expected: Vec<&str> = Direction::ALL
                .iter()
                .copied()
                .filter(|d| match role {
                    WalletRole::Source => d.source_chain() == chain,
                    WalletRole::Destination => d.destination_chain() == chain,
                })
                .map(Direction::as_str)
                .collect();
            let literal = Ledger::wallet_window_directions_sql_in(chain, role);
            let mut spelled: Vec<&str> = literal
                .trim_matches(|c| c == '(' || c == ')')
                .split(',')
                .map(|s| s.trim().trim_matches('\''))
                .collect();
            spelled.sort_unstable();
            let mut expected = expected;
            expected.sort_unstable();
            assert_eq!(spelled, expected, "{chain:?}/{role:?}: {literal}");
            assert_eq!(
                expected.len(),
                2,
                "every chain plays each role on exactly two routes"
            );
        }
    }
}

/// A window belongs to a wallet on ONE chain and spans both routes that
/// chain plays the role on — and never crosses to another chain, even
/// for byte-confusable identities.
#[test]
fn a_window_spans_the_routes_sharing_a_chain_and_never_crosses_chains() {
    // Solana source: SolToGlc consumes it for SolToRhn.
    let mut ledger = fresh_ledger();
    let sol_wallet = wallet(Chain::Solana, 1);
    admitted(
        attempt(
            &mut ledger,
            Direction::SolToGlc,
            &sol_wallet,
            &wallet(Chain::Goldcoin, 1),
            1,
            T0,
        ),
        Direction::SolToGlc,
        "",
    );
    let cross = attempt(
        &mut ledger,
        Direction::SolToRhn,
        &sol_wallet,
        &wallet(Chain::Robinhood, 1),
        2,
        T0 + 10,
    );
    assert_eq!(
        cross.reason(),
        Some(Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT)
    );

    // Solana destination: GlcToSol consumes it for RhnToSol.
    let mut ledger = fresh_ledger();
    let sol_dest = wallet(Chain::Solana, 2);
    admitted(
        attempt(
            &mut ledger,
            Direction::GlcToSol,
            &wallet(Chain::Goldcoin, 2),
            &sol_dest,
            1,
            T0,
        ),
        Direction::GlcToSol,
        "",
    );
    let cross = attempt(
        &mut ledger,
        Direction::RhnToSol,
        &wallet(Chain::Robinhood, 2),
        &sol_dest,
        2,
        T0 + 10,
    );
    assert_eq!(
        cross.reason(),
        Some(Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT)
    );

    // Robinhood destination: GlcToRhn consumes it for SolToRhn.
    let mut ledger = fresh_ledger();
    let evm_dest = wallet(Chain::Robinhood, 3);
    admitted(
        attempt(
            &mut ledger,
            Direction::GlcToRhn,
            &wallet(Chain::Goldcoin, 3),
            &evm_dest,
            1,
            T0,
        ),
        Direction::GlcToRhn,
        "",
    );
    let cross = attempt(
        &mut ledger,
        Direction::SolToRhn,
        &wallet(Chain::Solana, 3),
        &evm_dest,
        2,
        T0 + 10,
    );
    assert_eq!(
        cross.reason(),
        Some(Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT)
    );

    // Never across chains: a Solana source whose first 20 bytes ARE a
    // busy EVM depositor, and a Goldcoin destination text that is also
    // a busy... nothing — the direction predicate keeps every chain's
    // rows apart, so these are all fresh.
    let mut ledger = fresh_ledger();
    let evm = [0xAB; 20];
    admitted(
        attempt(
            &mut ledger,
            Direction::RhnToGlc,
            &evm,
            &wallet(Chain::Goldcoin, 4),
            1,
            T0,
        ),
        Direction::RhnToGlc,
        "",
    );
    admitted(
        attempt(
            &mut ledger,
            Direction::SolToGlc,
            &[0xAB; 32],
            &wallet(Chain::Goldcoin, 5),
            2,
            T0 + 10,
        ),
        Direction::SolToGlc,
        "a confusable Solana wallet",
    );
    admitted(
        attempt(
            &mut ledger,
            Direction::GlcToRhn,
            &wallet(Chain::Goldcoin, 6),
            &evm,
            3,
            T0 + 10,
        ),
        Direction::GlcToRhn,
        "the EVM address as a DESTINATION is a different window",
    );
}

/// The read-only views agree with admission at every boundary — they
/// are the same query.
#[test]
fn the_eligibility_views_agree_with_admission_on_every_route() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let (src, dst) = (source(direction, 1), destination(direction, 1));
        let fresh = ledger
            .route_wallet_eligibility(direction, Some(&src), Some(&dst), T0)
            .unwrap();
        assert!(fresh.is_eligible(), "{direction:?}");
        assert_eq!(fresh.blocked_reasons(), Vec::<&str>::new());

        admitted(
            attempt(&mut ledger, direction, &src, &dst, 1, T0),
            direction,
            "",
        );

        let busy = ledger
            .route_wallet_eligibility(direction, Some(&src), Some(&dst), T0 + 10)
            .unwrap();
        assert_eq!(busy.source_retry_after, Some(T0 + WINDOW), "{direction:?}");
        assert_eq!(
            busy.destination_retry_after,
            Some(T0 + WINDOW),
            "{direction:?}"
        );
        assert_eq!(busy.blocker(), Some((WalletRole::Source, T0 + WINDOW)));
        assert_eq!(
            busy.blocked_reasons(),
            vec![
                Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
                Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT
            ]
        );
        // One leg at a time, and a leg not asked about is unblocked.
        let only_dst = ledger
            .route_wallet_eligibility(direction, None, Some(&dst), T0 + 10)
            .unwrap();
        assert_eq!(
            only_dst.blocker(),
            Some((WalletRole::Destination, T0 + WINDOW))
        );
        let only_src = ledger
            .route_wallet_eligibility(direction, Some(&src), None, T0 + 10)
            .unwrap();
        assert_eq!(only_src.blocker(), Some((WalletRole::Source, T0 + WINDOW)));
        // The last blocked second, and the first eligible one.
        assert!(!ledger
            .route_wallet_eligibility(direction, Some(&src), Some(&dst), T0 + WINDOW - 1)
            .unwrap()
            .is_eligible());
        assert!(ledger
            .route_wallet_eligibility(direction, Some(&src), Some(&dst), T0 + WINDOW)
            .unwrap()
            .is_eligible());
        // Reading consumed nothing.
        admitted(
            attempt(&mut ledger, direction, &src, &dst, 2, T0 + WINDOW),
            direction,
            "after the reads",
        );
    }
}

/// An empty identity is never compared: it matches nothing, and the
/// column CHECK means it can never be stored either.
#[test]
fn an_empty_wallet_is_never_blocked_and_never_stored() {
    let ledger = fresh_ledger();
    assert_eq!(
        ledger
            .wallet_window_retry_after(Chain::Goldcoin, WalletRole::Source, b"", T0)
            .unwrap(),
        None
    );
    let err = ledger
        .conn_for_tests()
        .execute(
            "INSERT INTO bridge_requests (direction, state, gross_amount_atomic, recipient,
                created_at, source_chain, source_wallet)
             VALUES ('GlcToSol', 'AwaitingDeposit', 1, X'01', 1, 'goldcoin', X'')",
            [],
        )
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
}

/// Every fold and the Goldcoin observation write `source_wallet`, so a
/// resume can always find it — and a resume with none recorded fails
/// closed rather than skipping the check.
#[test]
fn every_route_records_its_source_wallet_and_a_resume_without_one_is_refused() {
    for direction in Direction::ALL {
        let mut ledger = fresh_ledger();
        let src = source(direction, 1);
        let id = admitted(
            attempt(
                &mut ledger,
                direction,
                &src,
                &destination(direction, 1),
                1,
                T0,
            ),
            direction,
            "",
        );
        assert_eq!(
            ledger
                .get_request(id)
                .unwrap()
                .unwrap()
                .source_wallet
                .as_deref(),
            Some(src.as_slice()),
            "{direction:?}"
        );
    }
    // A parked cross-route row stripped of its source wallet: refused.
    let mut ledger = fresh_ledger();
    let direction = Direction::RhnToSol;
    let src = source(direction, 1);
    admitted(
        attempt(
            &mut ledger,
            direction,
            &src,
            &destination(direction, 1),
            1,
            T0,
        ),
        direction,
        "",
    );
    let Attempt::Parked { request_id, .. } = attempt(
        &mut ledger,
        direction,
        &src,
        &destination(direction, 2),
        2,
        T0 + 10,
    ) else {
        panic!()
    };
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET source_wallet = NULL WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    let err = ledger
        .resume_manual_review_cross_route(direction, request_id, "try", "operator", T0 + WINDOW + 1)
        .unwrap_err();
    assert!(
        matches!(&err, LedgerError::ManualReviewNotRecoverable { detail, .. } if detail.contains("no source wallet")),
        "{err}"
    );
}

// --------------------------------------------------------------- resume --

/// A park clears on its own: once the blocking window has elapsed the
/// SAME request resumes — on every fold route, through the route's own
/// resume path — and not one second earlier, on either leg, regardless
/// of what the park's own reason was.
#[test]
fn a_parked_duplicate_resumes_only_once_both_windows_have_elapsed_on_every_fold_route() {
    for direction in [
        Direction::SolToGlc,
        Direction::RhnToGlc,
        Direction::SolToRhn,
        Direction::RhnToSol,
    ] {
        let mut ledger = fresh_ledger();
        let (src, dst) = (source(direction, 1), destination(direction, 1));
        admitted(
            attempt(&mut ledger, direction, &src, &dst, 1, T0),
            direction,
            "",
        );
        // Parked for the SOURCE; the destination is also busy, and its
        // window (from the same first row) matters too.
        let Attempt::Parked { request_id, .. } =
            attempt(&mut ledger, direction, &src, &dst, 2, T0 + 10)
        else {
            panic!("{direction:?}")
        };
        let resume = |ledger: &mut Ledger, now: i64| match direction {
            Direction::SolToGlc => {
                ledger.resume_manual_review_sol_to_glc(request_id, "ops", "operator", now)
            }
            Direction::RhnToGlc => {
                ledger.resume_manual_review_rhn_to_glc(request_id, "ops", "operator", now)
            }
            _ => ledger
                .resume_manual_review_cross_route(direction, request_id, "ops", "operator", now),
        };
        let early = resume(&mut ledger, T0 + WINDOW - 1).unwrap_err();
        assert!(
            matches!(&early, LedgerError::WalletWindowActive { role: WalletRole::Source, retry_after, .. } if *retry_after == T0 + WINDOW),
            "{direction:?}: {early}"
        );
        assert_eq!(state_of(&ledger, request_id), RequestState::ManualReview);
        assert_eq!(
            resume(&mut ledger, T0 + WINDOW).unwrap(),
            ResumeManualReviewOutcome::Resumed,
            "{direction:?}"
        );
        assert_eq!(
            state_of(&ledger, request_id),
            RequestState::SourceFinalized,
            "{direction:?}"
        );
        assert_eq!(
            resume(&mut ledger, T0 + WINDOW + 1).unwrap(),
            ResumeManualReviewOutcome::AlreadyResumed {
                state: RequestState::SourceFinalized
            }
        );
    }
}

/// Only a strict predecessor blocks a resume: the oldest of three parked
/// siblings resumes first, and a newer sibling never shadow-blocks it —
/// on the cross routes exactly as on the inbound ones.
#[test]
fn a_cross_route_backlog_to_one_wallet_drains_oldest_first() {
    let direction = Direction::RhnToSol;
    let mut ledger = fresh_ledger();
    let src = source(direction, 1);
    admitted(
        attempt(
            &mut ledger,
            direction,
            &src,
            &destination(direction, 1),
            1,
            T0,
        ),
        direction,
        "A",
    );
    let Attempt::Parked { request_id: b, .. } = attempt(
        &mut ledger,
        direction,
        &src,
        &destination(direction, 2),
        2,
        T0 + 10,
    ) else {
        panic!()
    };
    let Attempt::Parked { request_id: c, .. } = attempt(
        &mut ledger,
        direction,
        &src,
        &destination(direction, 3),
        3,
        T0 + 20,
    ) else {
        panic!()
    };
    // A's window clears at T0 + WINDOW: B (blocked only by A) resumes;
    // C is still blocked by B, whose own window runs to T0 + 10 + WINDOW.
    assert_eq!(
        ledger
            .resume_manual_review_cross_route(direction, b, "ops", "operator", T0 + WINDOW)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
    let c_err = ledger
        .resume_manual_review_cross_route(direction, c, "ops", "operator", T0 + WINDOW)
        .unwrap_err();
    assert!(
        matches!(&c_err, LedgerError::WalletWindowActive { retry_after, .. } if *retry_after == T0 + 10 + WINDOW),
        "{c_err}"
    );
    assert_eq!(
        ledger
            .resume_manual_review_cross_route(direction, c, "ops", "operator", T0 + 10 + WINDOW)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
}

// ------------------------------------------------------------- reasons --

#[test]
fn both_reasons_in_both_spellings_keep_every_exit() {
    for reason in [
        Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
        Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT,
        Ledger::LEGACY_MANUAL_REVIEW_REASON_SOURCE_WALLET_RATE_LIMITED,
        Ledger::LEGACY_MANUAL_REVIEW_REASON_RECIPIENT_RATE_LIMITED,
    ] {
        assert!(Ledger::is_wallet_window_manual_review_reason(reason));
        assert!(
            Ledger::is_recoverable_manual_review_reason(Some(reason)),
            "{reason}"
        );
        assert!(
            Ledger::is_auto_resumable_manual_review_reason(Some(reason), false),
            "{reason}"
        );
        assert!(
            Ledger::is_refundable_manual_review_reason(Direction::SolToGlc, Some(reason)),
            "{reason}"
        );
        assert!(
            Ledger::is_refundable_manual_review_reason(Direction::SolToRhn, Some(reason)),
            "{reason}"
        );
    }
    assert_eq!(WalletRole::Source.limit_reason(), "wallet_source_24h_limit");
    assert_eq!(
        WalletRole::Destination.limit_reason(),
        "wallet_destination_24h_limit"
    );
    // The Goldcoin refund path accepts the two current spellings (a
    // Goldcoin-sourced row never carried a legacy one).
    for reason in [
        Ledger::MANUAL_REVIEW_REASON_WALLET_SOURCE_24H_LIMIT,
        Ledger::MANUAL_REVIEW_REASON_WALLET_DESTINATION_24H_LIMIT,
    ] {
        assert!(Ledger::REFUNDABLE_GLC_MANUAL_REVIEW_REASONS.contains(&reason));
    }
    assert!(!Ledger::is_wallet_window_manual_review_reason(
        "insufficient_capacity_at_fold"
    ));
}

/// A row parked under a LEGACY spelling before the generalization still
/// resumes through the real path, and the resume's own re-check still
/// applies to it.
#[test]
fn a_legacy_spelled_park_still_resumes_and_is_still_window_checked() {
    let direction = Direction::SolToGlc;
    let mut ledger = fresh_ledger();
    let (src, dst) = (source(direction, 1), destination(direction, 1));
    admitted(
        attempt(&mut ledger, direction, &src, &dst, 1, T0),
        direction,
        "",
    );
    let Attempt::Parked { request_id, .. } =
        attempt(&mut ledger, direction, &src, &dst, 2, T0 + 10)
    else {
        panic!()
    };
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET manual_review_note = 'source_wallet_rate_limited' WHERE id = ?1",
            [request_id],
        )
        .unwrap();
    assert!(ledger
        .resume_manual_review_sol_to_glc(request_id, "ops", "operator", T0 + 100)
        .is_err());
    assert_eq!(
        ledger
            .resume_manual_review_sol_to_glc(request_id, "ops", "operator", T0 + WINDOW)
            .unwrap(),
        ResumeManualReviewOutcome::Resumed
    );
}

/// Operator rendering of a wallet, per chain — what a refusal message
/// shows.
#[test]
fn render_wallet_spells_each_chain_its_own_way() {
    assert_eq!(render_wallet(Chain::Goldcoin, b"QabcDEF123"), "QabcDEF123");
    assert!(render_wallet(Chain::Goldcoin, &[0xff, 0x00]).starts_with("ff00"));
    assert_eq!(
        render_wallet(Chain::Solana, &[0u8; 32]),
        "11111111111111111111111111111111"
    );
    assert_eq!(
        render_wallet(Chain::Robinhood, &[0xab; 20]),
        format!("0x{}", "ab".repeat(20))
    );
}
