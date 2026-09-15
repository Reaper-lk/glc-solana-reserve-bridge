//! The persisted bridge quote across ALL SIX routes (docs/38-elastic-
//! bridge-rate.md, Phase 2A): every new request carries a unit-rate
//! quote, folds lock it, `POST /transfers`-shaped creation leaves it
//! indicative until the deposit observation locks it, a reorg unlocks it,
//! a tampered quote refuses to settle, and a legacy (NULL-quote) row
//! settles through the pre-v37 path untouched.
//!
//! Lives under the fold tests because they already own a fixture for
//! every reserve and both Robinhood observation shapes.

use super::cross_route::{
    ledger_with_every_reserve, rhn_to_sol_observation, store_rhn_to_sol, MINT_DECIMALS,
    RHN_TO_SOL_BPS, SOL_RECIPIENT,
};
use super::{destination, network, observation, store};
use crate::amount_conversion::{compute_fee_at_bps, CanonicalAtomic, ConversionError};
use crate::bridge_rate::{PersistedQuote, RateBook, PRICE_SCALE};
use crate::ledger::{
    CreateRequestOutcome, Direction, Ledger, RequestAmounts, RequestState, SolFoldOutcome,
};
use crate::robinhood::fold::{fold_observation, fold_observation_to_solana, FoldOutcome};
use crate::routes::Route;

const GROSS: u64 = 500_000_000; // 5 GLC
const NOW: i64 = 1_700_000_000;

/// Unit-rate amounts for `gross` at `fee_bps`, as every production
/// pricing site now builds them.
fn quoted(route: Route, gross: u64, fee_bps: u64, net_destination_atomic: u64) -> RequestAmounts {
    let quote = RateBook::fixed_unit(60)
        .quote(route, CanonicalAtomic(gross), fee_bps, NOW)
        .unwrap();
    RequestAmounts::from_quote(quote, net_destination_atomic)
}

fn expect_unit_quote(q: &PersistedQuote, gross: u64) {
    assert_eq!(q.source_price_e12, PRICE_SCALE);
    assert_eq!(q.destination_price_e12, PRICE_SCALE);
    assert_eq!(
        q.gross_out_atomic, gross,
        "a unit rate: gross_out == gross_in"
    );
    assert_eq!(q.quoted_at, NOW);
    assert_eq!(q.quote_expires_at, NOW + 60);
    assert_eq!(q.source_feed_at, NOW);
    assert_eq!(q.destination_feed_at, NOW);
}

fn sol_to_glc(ledger: &mut Ledger, index: u64, amounts: RequestAmounts) -> i64 {
    match ledger
        .fold_sol_deposit(
            index,
            amounts,
            [0x11; 32],
            destination().as_bytes(),
            None,
            NOW,
        )
        .unwrap()
    {
        SolFoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("expected a fold, got {other:?}"),
    }
}

fn sol_to_rhn(ledger: &mut Ledger, index: u64, amounts: RequestAmounts) -> i64 {
    match ledger
        .fold_sol_deposit_to_robinhood(
            index,
            amounts,
            [0x12; 32],
            // A recipient of its own: the rolling-24h destination window
            // would park a second request to the GlcToRhn recipient.
            Some([0xED; 20]),
            b"0x00000000000000000000000000000000000000ed",
            true,
            None,
            NOW,
        )
        .unwrap()
    {
        SolFoldOutcome::FoldedFinalized { request_id } => request_id,
        other => panic!("expected a fold, got {other:?}"),
    }
}

fn goldcoin_sourced(ledger: &mut Ledger, direction: Direction, recipient: &[u8]) -> i64 {
    let route = Route::from(direction);
    let net_destination = match direction {
        Direction::GlcToSol => 4_850_000, // net 4.85 GLC at 6 dp
        _ => 485_000_000,
    };
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(
            direction,
            quoted(route, GROSS, 300, net_destination),
            recipient,
            None,
            3600,
            NOW,
        )
        .unwrap()
    else {
        panic!("expected Reserved")
    };
    request_id
}

// =====================================================================
// All six routes
// =====================================================================

#[test]
fn every_new_request_on_every_route_carries_a_unit_rate_quote() {
    let mut ledger = ledger_with_every_reserve();

    // Goldcoin-sourced: created BEFORE the deposit -> indicative, unlocked.
    let glc_to_sol = goldcoin_sourced(&mut ledger, Direction::GlcToSol, &[0x51; 32]);
    let glc_to_rhn = goldcoin_sourced(&mut ledger, Direction::GlcToRhn, &[0xEC; 20]);
    for (id, direction) in [
        (glc_to_sol, Direction::GlcToSol),
        (glc_to_rhn, Direction::GlcToRhn),
    ] {
        let request = ledger.get_request(id).unwrap().unwrap();
        assert_eq!(request.direction, direction);
        let q = request
            .quote
            .expect("a quote on a new Goldcoin-sourced request");
        expect_unit_quote(&q, GROSS);
        assert_eq!(
            q.locked_at, None,
            "{direction:?}: indicative until observed"
        );
        assert!(matches!(
            request.verify_breakdown(),
            Err(ConversionError::QuoteNotLocked { .. })
        ));
    }

    // Solana-sourced: the fold is the lock.
    let sol_to_glc = sol_to_glc(
        &mut ledger,
        0,
        quoted(Route::SolToGlc, GROSS, 600, 470_000_000),
    );
    let sol_to_rhn = sol_to_rhn(
        &mut ledger,
        1,
        quoted(Route::SolToRhn, GROSS, 450, 477_500_000),
    );
    // Robinhood-sourced: the fold is the lock.
    // A Goldcoin recipient distinct from the SolToGlc request's, for the
    // same rolling-24h reason.
    let other_goldcoin_destination = crate::goldcoin::address::encode_p2pkh(&[0x43; 20], network());
    let row = observation(7, GROSS, other_goldcoin_destination.into_bytes());
    store(&ledger, &row);
    let FoldOutcome::FoldedFinalized {
        request_id: rhn_to_glc,
    } = fold_observation(
        &mut ledger,
        &row,
        network(),
        600,
        CanonicalAtomic(1),
        true,
        NOW,
    )
    .unwrap()
    else {
        panic!("expected a payable RhnToGlc fold")
    };
    // Likewise a Solana recipient distinct from the GlcToSol request's.
    let mut rhn_to_sol_recipient = SOL_RECIPIENT;
    rhn_to_sol_recipient[0] = 0x52;
    let row = rhn_to_sol_observation(8, GROSS, rhn_to_sol_recipient.to_vec());
    store_rhn_to_sol(&ledger, &row);
    let FoldOutcome::FoldedFinalized {
        request_id: rhn_to_sol,
    } = fold_observation_to_solana(
        &mut ledger,
        &row,
        RHN_TO_SOL_BPS,
        CanonicalAtomic(1),
        MINT_DECIMALS,
        true,
        NOW,
    )
    .unwrap()
    else {
        panic!("expected a payable RhnToSol fold")
    };

    for (id, direction, fee_bps) in [
        (sol_to_glc, Direction::SolToGlc, 600),
        (sol_to_rhn, Direction::SolToRhn, 450),
        (rhn_to_glc, Direction::RhnToGlc, 600),
        (rhn_to_sol, Direction::RhnToSol, RHN_TO_SOL_BPS),
    ] {
        let request = ledger.get_request(id).unwrap().unwrap();
        assert_eq!(request.direction, direction);
        assert_eq!(request.state, RequestState::SourceFinalized);
        assert_eq!(request.fee_bps, fee_bps);
        let q = request.quote.expect("a quote on a folded request");
        expect_unit_quote(&q, GROSS);
        assert_eq!(q.locked_at, Some(NOW), "{direction:?}: locked at the fold");
        // The settlement figures are the pre-quote fee rule's, exactly.
        let legacy = compute_fee_at_bps(CanonicalAtomic(GROSS), fee_bps).unwrap();
        let verified = request.verify_breakdown().unwrap();
        assert_eq!(verified, legacy, "{direction:?}");
        assert_eq!(request.fee_amount_atomic, legacy.fee.0);
        assert_eq!(request.net_amount_atomic, legacy.net.0);
    }
}

// =====================================================================
// Goldcoin-sourced lifecycle: indicative -> locked -> reorg -> re-locked
// =====================================================================

#[test]
fn a_goldcoin_deposit_observation_locks_the_quote_and_a_reorg_unlocks_it() {
    let mut ledger = ledger_with_every_reserve();
    let id = goldcoin_sourced(&mut ledger, Direction::GlcToSol, &[0x51; 32]);

    // Observed in a block: locked, at the observation's own instant.
    ledger
        .record_glc_deposit_observed(id, [0xAA; 32], 1, GROSS, 10, [0x01; 32], NOW + 90)
        .unwrap();
    let request = ledger.get_request(id).unwrap().unwrap();
    let q = request.quote.unwrap();
    assert_eq!(q.locked_at, Some(NOW + 90));
    assert_eq!(
        q.quoted_at,
        NOW + 90,
        "re-struck at observation, not the indicative one"
    );
    assert_eq!(q.quote_expires_at, NOW + 150);
    expect_settles_at_unit_rate(&request);

    // The observing block is orphaned: the lock goes, the quote stays as
    // a record, and nothing can settle.
    ledger.mark_glc_reorged(id, NOW + 120).unwrap();
    let request = ledger.get_request(id).unwrap().unwrap();
    assert_eq!(request.state, RequestState::AwaitingDeposit);
    assert_eq!(request.source_txid, None);
    let q = request.quote.unwrap();
    assert_eq!(q.locked_at, None, "a reorg unlocks the quote");
    assert_eq!(q.quoted_at, NOW + 90);
    assert!(matches!(
        request.verify_breakdown(),
        Err(ConversionError::QuoteNotLocked { .. })
    ));

    // Re-observed in a later block: a fresh lock, at the new instant.
    ledger
        .record_glc_deposit_observed(id, [0xAB; 32], 0, GROSS, 12, [0x02; 32], NOW + 300)
        .unwrap();
    let request = ledger.get_request(id).unwrap().unwrap();
    let q = request.quote.unwrap();
    assert_eq!(q.locked_at, Some(NOW + 300));
    assert_eq!(q.quoted_at, NOW + 300);
    expect_settles_at_unit_rate(&request);
}

fn expect_settles_at_unit_rate(request: &crate::ledger::BridgeRequest) {
    let legacy = compute_fee_at_bps(CanonicalAtomic(GROSS), request.fee_bps).unwrap();
    assert_eq!(request.verify_breakdown().unwrap(), legacy);
}

#[test]
fn the_bulk_reorg_rollback_unlocks_every_rolled_back_request() {
    let mut ledger = ledger_with_every_reserve();
    let a = goldcoin_sourced(&mut ledger, Direction::GlcToSol, &[0x51; 32]);
    let b = goldcoin_sourced(&mut ledger, Direction::GlcToRhn, &[0xEC; 20]);
    for (id, txid) in [(a, [0xA1; 32]), (b, [0xB1; 32])] {
        ledger
            .record_glc_deposit_observed(id, txid, 0, GROSS, 50, [0x05; 32], NOW + 10)
            .unwrap();
        assert_eq!(
            ledger
                .get_request(id)
                .unwrap()
                .unwrap()
                .quote
                .unwrap()
                .locked_at,
            Some(NOW + 10)
        );
    }
    // Everything observed above the fork height (49) is rolled back.
    ledger
        .goldcoin_rollback_reorg(49, [0x49; 32], 50, [0x05; 32], NOW + 20)
        .unwrap();
    for id in [a, b] {
        let request = ledger.get_request(id).unwrap().unwrap();
        assert_eq!(request.state, RequestState::AwaitingDeposit);
        assert_eq!(request.quote.unwrap().locked_at, None);
    }
}

// =====================================================================
// Legacy rows and tampering
// =====================================================================

#[test]
fn a_legacy_row_with_no_quote_settles_through_the_pre_v37_path_unchanged() {
    let mut ledger = ledger_with_every_reserve();
    // A `quote: None` fold writes a row indistinguishable from one created
    // before v37: every quote column NULL.
    let fb = compute_fee_at_bps(CanonicalAtomic(GROSS), 600).unwrap();
    let legacy = RequestAmounts {
        gross_atomic: GROSS,
        fee_bps: 600,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
        quote: None,
    };
    let id = sol_to_glc(&mut ledger, 3, legacy);
    let request = ledger.get_request(id).unwrap().unwrap();
    assert!(request.quote.is_none());
    assert_eq!(request.verify_breakdown().unwrap(), fb);
    // ... and the legacy tamper detection is the legacy error.
    ledger
        .conn_for_tests()
        .execute(
            "UPDATE bridge_requests SET net_amount_atomic = net_amount_atomic + 1 WHERE id = ?1",
            [id],
        )
        .unwrap();
    assert!(matches!(
        ledger.get_request(id).unwrap().unwrap().verify_breakdown(),
        Err(ConversionError::AccountingMismatch { .. })
    ));
}

#[test]
fn a_tampered_quote_refuses_to_settle_whichever_column_was_touched() {
    let mut ledger = ledger_with_every_reserve();
    let id = sol_to_glc(
        &mut ledger,
        4,
        quoted(Route::SolToGlc, GROSS, 600, 470_000_000),
    );
    let conn = ledger.conn_for_tests();
    let snapshot = || {
        conn.query_row(
            "SELECT quote_source_price_e12, quote_destination_price_e12, quote_gross_out_atomic,
                    fee_amount_atomic, net_amount_atomic
             FROM bridge_requests WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .unwrap()
    };
    let original = snapshot();
    let restore = |(src, dst, gross_out, fee, net): (i64, i64, i64, i64, i64)| {
        conn.execute(
            "UPDATE bridge_requests SET quote_source_price_e12 = ?2,
                quote_destination_price_e12 = ?3, quote_gross_out_atomic = ?4,
                fee_amount_atomic = ?5, net_amount_atomic = ?6 WHERE id = ?1",
            rusqlite::params![id, src, dst, gross_out, fee, net],
        )
        .unwrap();
    };
    for (column, value) in [
        ("quote_source_price_e12", 1_250_000_000_000i64),
        ("quote_destination_price_e12", 800_000_000_000),
        ("quote_gross_out_atomic", GROSS as i64 + 1),
        ("fee_amount_atomic", original.3 + 1),
        ("net_amount_atomic", original.4 + 1),
    ] {
        conn.execute(
            &format!("UPDATE bridge_requests SET {column} = ?2 WHERE id = ?1"),
            rusqlite::params![id, value],
        )
        .unwrap();
        assert!(
            matches!(
                ledger.get_request(id).unwrap().unwrap().verify_breakdown(),
                Err(ConversionError::QuoteMismatch { .. })
            ),
            "{column} tampered"
        );
        restore(original);
        assert!(ledger
            .get_request(id)
            .unwrap()
            .unwrap()
            .verify_breakdown()
            .is_ok());
    }
}

#[test]
fn the_ledger_refuses_amounts_that_are_not_the_quotes_own_figures() {
    let mut ledger = ledger_with_every_reserve();
    let mut amounts = quoted(Route::SolToGlc, GROSS, 600, 470_000_000);
    amounts.net_atomic += 1;
    let err = ledger
        .fold_sol_deposit(9, amounts, [0x11; 32], destination().as_bytes(), None, NOW)
        .unwrap_err();
    assert!(
        matches!(err, crate::ledger::LedgerError::BridgeQuote { .. }),
        "{err}"
    );
    assert!(
        ledger.get_request(1).unwrap().is_none(),
        "nothing was written"
    );
}

// =====================================================================
// Persistence across a reopen (restart / recovery reads the same quote)
// =====================================================================

#[test]
fn the_locked_quote_survives_a_reopen_and_is_what_recovery_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let id = {
        let mut ledger = Ledger::open(&db_path).unwrap();
        for reserve in [
            crate::ledger::ReserveDirection::GoldcoinReserve,
            crate::ledger::ReserveDirection::SolanaReserve,
        ] {
            ledger
                .configure_reserve(reserve, 1_000_000_000_000, 0, 1, 1, 1, 100)
                .unwrap();
        }
        sol_to_glc(
            &mut ledger,
            5,
            quoted(Route::SolToGlc, GROSS, 600, 470_000_000),
        )
    };
    let ledger = Ledger::open(&db_path).unwrap();
    let request = ledger.get_request(id).unwrap().unwrap();
    let q = request.quote.unwrap();
    expect_unit_quote(&q, GROSS);
    assert_eq!(q.locked_at, Some(NOW));
    assert_eq!(
        request.verify_breakdown().unwrap(),
        compute_fee_at_bps(CanonicalAtomic(GROSS), 600).unwrap()
    );
}
