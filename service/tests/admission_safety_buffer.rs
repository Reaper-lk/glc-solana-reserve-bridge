//! Confirmed-Goldcoin-liquidity admission safety buffer, with hysteresis.
//!
//! The hard invariant (`total_reserve_balance >= protected_minimum +
//! reserved_liquidity`) is a solvency floor: reaching it stops payouts.
//! Admission used to close only AT that floor, so ordinary traffic could
//! walk right up to it. The buffer closes admission substantially earlier.
//!
//! Two separate mechanisms, tested separately here:
//!
//! 1. A PER-REQUEST gate in `fold_sol_deposit` (and, identically, in
//!    `resume_manual_review_sol_to_glc`): admit only if
//!    `balance >= protected_minimum + reserved + amount + buffer`.
//! 2. A DIRECTION-WIDE hysteresis state machine
//!    (`evaluate_liquidity_admission`): close at the buffer, reopen only
//!    at the higher reopen threshold, and never reopen a closure an
//!    operator made.
//!
//! # Why "confirmed" is structural here, not a policy that could drift
//!
//! Every figure below derives from `total_reserve_balance`, which is set
//! by reconciliation from outputs at `vault_min_confirmations` only
//! (`Orchestrator::tick_goldcoin_reconciliation` filters
//! `e.confirmations >= vault_min_confirmations` before summing). Immature
//! payout change and every zero-conf recursive candidate live in
//! `vault_utxos` rows with `state = 'Unconfirmed'`
//! (`Ledger::zero_conf_change_vault_utxos_with_depth` selects exactly
//! those), and are therefore absent from that balance by construction —
//! not filtered out by a rule someone could later relax. The tests named
//! `..._does_not_count_as_admission_capacity` pin that property directly.

use glc_reserve_bridge_service::amount_conversion::{compute_fee, CanonicalAtomic};
use glc_reserve_bridge_service::goldcoin::coin::VaultUtxo;
use glc_reserve_bridge_service::ledger::{
    AdmissionLiquidityTransition, Ledger, LedgerError, RequestAmounts, RequestState,
    ReserveDirection, SolFoldOutcome,
};
use glc_reserve_bridge_service::reconciliation;

/// Confirmations below this make a `vault_utxos` row `Unconfirmed` — the
/// state that holds both immature payout change and every zero-conf
/// recursive candidate, and the state `total_reserve_balance` excludes.
const MIN_CONFIRMATIONS: i64 = 6;

/// Sets the MATURE CONFIRMED balance through the production path
/// (`reconciliation::reconcile`), not a test-only setter, so these tests
/// exercise the same write the daemon performs. The tolerance is wide
/// enough that reconciliation never auto-pauses on the deliberately large
/// swings below — auto-pause is a different mechanism with its own tests.
fn set_confirmed_balance(ledger: &mut Ledger, atomic: u64, now: i64) {
    reconciliation::reconcile(
        ledger,
        ReserveDirection::GoldcoinReserve,
        atomic,
        u64::MAX / 4,
        now,
    )
    .unwrap();
}

/// Adds an UNCONFIRMED vault UTXO through the production sync path. Value
/// the vault genuinely holds, deliberately excluded from
/// `total_reserve_balance` until it matures.
fn add_unconfirmed_vault_utxo(ledger: &mut Ledger, txid: [u8; 32], vout: u32, atomic: u64) {
    let utxo = VaultUtxo {
        txid,
        vout,
        amount_atomic: atomic,
        script_pubkey_hex: "76a914000000000000000000000000000000000000000088ac".to_string(),
    };
    ledger
        .sync_vault_utxos(
            &[(
                utxo,
                1,
                "76a914000000000000000000000000000000000000000088ac".to_string(),
            )],
            MIN_CONFIRMATIONS,
            1_000,
        )
        .unwrap();
}

const GLC: u64 = 100_000_000;
/// The approved production admission safety buffer / automatic-close
/// threshold: 250,000 GLC. Mirrors `config::default_admission_safety_buffer_atomic`.
const CLOSE_BUFFER: u64 = 250_000 * GLC;
/// The approved production automatic-reopen threshold: 350,000 GLC.
/// Mirrors `config::default_admission_reopen_buffer_atomic`.
const REOPEN_BUFFER: u64 = 350_000 * GLC;
const PROTECTED_MINIMUM: u64 = 20_000 * GLC;

fn amounts_for_gross_glc(gross_glc: u64) -> RequestAmounts {
    let fb = compute_fee(CanonicalAtomic(gross_glc * GLC)).unwrap();
    RequestAmounts {
        gross_atomic: fb.gross.0,
        fee_bps: fb.fee_bps,
        fee_atomic: fb.fee.0,
        net_atomic: fb.net.0,
        net_destination_atomic: fb.net.0,
    }
}

/// A ledger whose Goldcoin reserve holds `balance_glc` of MATURE CONFIRMED
/// value, with the production buffer configured and UTXO-count
/// backpressure disabled so it can never be the reason a test fails.
fn setup(balance_glc: u64) -> Ledger {
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger, balance_glc);
    ledger
}

fn configure(ledger: &mut Ledger, balance_glc: u64) {
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            balance_glc * GLC,
            PROTECTED_MINIMUM,
            1_000_000 * GLC,
            500_000 * GLC,
            300_000 * GLC,
            0,
        )
        .unwrap();
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    // Disabled: this suite is about the VALUE buffer, and a count-based
    // refusal would mask the thing under test.
    ledger
        .set_utxo_pool_thresholds(ReserveDirection::GoldcoinReserve, 0, 0)
        .unwrap();
    ledger
        .set_admission_liquidity_buffer(
            ReserveDirection::GoldcoinReserve,
            CLOSE_BUFFER,
            REOPEN_BUFFER,
        )
        .unwrap();
}

fn fold(ledger: &mut Ledger, index: u64, gross_glc: u64) -> SolFoldOutcome {
    ledger
        .fold_sol_deposit(
            index,
            amounts_for_gross_glc(gross_glc),
            [index as u8; 32],
            format!("recipient-{index}").as_bytes(),
            1_000 + index as i64,
        )
        .unwrap()
}

fn headroom(ledger: &Ledger) -> i64 {
    ledger
        .confirmed_unreserved_headroom(ReserveDirection::GoldcoinReserve)
        .unwrap()
}

fn is_accepted(outcome: &SolFoldOutcome) -> bool {
    matches!(outcome, SolFoldOutcome::FoldedFinalized { .. })
}

fn park_reason(ledger: &Ledger, request_id: i64) -> Option<String> {
    ledger
        .get_request(request_id)
        .unwrap()
        .and_then(|r| r.manual_review_note)
}

fn request_id(outcome: &SolFoldOutcome) -> i64 {
    match outcome {
        SolFoldOutcome::FoldedFinalized { request_id } => *request_id,
        SolFoldOutcome::FoldedManualReview { request_id } => *request_id,
        SolFoldOutcome::AlreadyFolded { request_id } => *request_id,
    }
}

// ------------------------------------------------ per-request admission --

/// Plenty of confirmed headroom above the buffer: admitted normally.
#[test]
fn accepts_a_request_with_sufficient_confirmed_headroom() {
    // 20,000 protected + 250,000 buffer + 10,000 request = 280,000 needed.
    let mut ledger = setup(500_000);
    let outcome = fold(&mut ledger, 1, 10_000);
    assert!(
        is_accepted(&outcome),
        "500,000 GLC confirmed leaves ample headroom: {outcome:?}"
    );
}

/// The request itself fits under the hard invariant, but admitting it
/// would eat into the buffer. Parked, not dropped — and the recorded
/// reason says the buffer was why, not that the reserve was empty.
#[test]
fn rejects_a_request_that_would_breach_the_safety_buffer() {
    // 20,000 protected + 250,000 buffer = 270,000. At 275,000 confirmed,
    // headroom is 255,000 — a 10,000 GLC request leaves 245,000, under.
    let mut ledger = setup(275_000);
    let outcome = fold(&mut ledger, 1, 10_000);
    assert!(
        matches!(outcome, SolFoldOutcome::FoldedManualReview { .. }),
        "admitting would leave headroom below the buffer: {outcome:?}"
    );
    assert_eq!(
        park_reason(&ledger, request_id(&outcome)).as_deref(),
        Some("liquidity_buffer_at_fold"),
        "the buffer must be named as the reason, distinctly from an empty reserve"
    );
    // Nothing was reserved for a parked request.
    assert_eq!(headroom(&ledger), 255_000 * GLC as i64);
}

/// Exactly at the boundary: leaving headroom EQUAL to the buffer is
/// allowed; one atomic unit less is not. `>=`, not `>`.
#[test]
fn behaviour_exactly_at_the_buffer_boundary() {
    let net = amounts_for_gross_glc(10_000).net_destination_atomic;

    // Case 1: balance leaves headroom exactly == buffer after the request.
    let exact = PROTECTED_MINIMUM + CLOSE_BUFFER + net;
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger, 0);
    set_confirmed_balance(&mut ledger, (exact as i64) as u64, 9_000);
    assert!(
        is_accepted(&fold(&mut ledger, 1, 10_000)),
        "headroom exactly equal to the buffer must be admitted"
    );

    // Case 2: one atomic unit short.
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger, 0);
    set_confirmed_balance(&mut ledger, (exact as i64 - 1) as u64, 9_000);
    assert!(
        matches!(
            fold(&mut ledger, 2, 10_000),
            SolFoldOutcome::FoldedManualReview { .. }
        ),
        "one atomic unit below the boundary must be refused"
    );
}

/// Two requests that each fit alone but not together. The second is
/// refused because the first's reservation already consumed the headroom —
/// the check reads `reserved_liquidity` inside the same write transaction
/// that writes it, so there is no window to race through.
#[test]
fn concurrent_admissions_cannot_race_through_the_safety_buffer() {
    // Headroom 280,000; buffer 250,000; two 20,000 GLC requests.
    let mut ledger = setup(300_000);
    let first = fold(&mut ledger, 1, 20_000);
    assert!(is_accepted(&first), "the first request fits: {first:?}");

    let second = fold(&mut ledger, 2, 20_000);
    assert!(
        matches!(second, SolFoldOutcome::FoldedManualReview { .. }),
        "the second must see the first's reservation and refuse: {second:?}"
    );

    // And the buffer is intact: headroom never dipped below it.
    assert!(
        headroom(&ledger) >= CLOSE_BUFFER as i64,
        "headroom {} fell below the buffer {}",
        headroom(&ledger),
        CLOSE_BUFFER
    );
}

// ------------------------------------- immature / zero-conf never count --

/// Immature own payout change is real value the vault holds, but it is not
/// confirmed, so it must not buy admission capacity.
#[test]
fn immature_own_change_does_not_count_as_admission_capacity() {
    // Confirmed balance alone is one GLC short of admitting.
    let net = amounts_for_gross_glc(10_000).net_destination_atomic;
    let short = PROTECTED_MINIMUM + CLOSE_BUFFER + net - GLC;
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger, 0);
    set_confirmed_balance(&mut ledger, (short as i64) as u64, 9_000);

    // A large immature vault UTXO exists — far more than the shortfall.
    add_unconfirmed_vault_utxo(&mut ledger, [9u8; 32], 0, 100_000 * GLC);
    assert!(
        ledger.immature_vault_utxo_total().unwrap() > 0,
        "fixture must actually create immature value"
    );

    assert!(
        matches!(
            fold(&mut ledger, 1, 10_000),
            SolFoldOutcome::FoldedManualReview { .. }
        ),
        "immature value must not be counted toward the buffer"
    );

    // And it is reported, so an operator can see recovery is en route,
    // without ever being added to the headroom.
    let status = ledger
        .admission_liquidity_status(ReserveDirection::GoldcoinReserve, 1_000)
        .unwrap();
    assert_eq!(
        status.confirmed_unreserved_headroom,
        short as i64 - PROTECTED_MINIMUM as i64
    );
    assert!(
        status.immature_excluded_atomic > 0,
        "reported for visibility"
    );
}

/// Zero-conf recursive payout change is selectable for SPENDING by the
/// payout pipeline, which makes it exactly the value most likely to be
/// mistaken for capacity. It is `state = 'Unconfirmed'`, so it is not.
#[test]
fn zero_conf_recursive_change_does_not_count_as_admission_capacity() {
    let net = amounts_for_gross_glc(10_000).net_destination_atomic;
    let short = PROTECTED_MINIMUM + CLOSE_BUFFER + net - GLC;
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure(&mut ledger, 0);
    set_confirmed_balance(&mut ledger, (short as i64) as u64, 9_000);

    // An unconfirmed vault UTXO that IS a zero-conf change candidate.
    add_unconfirmed_vault_utxo(&mut ledger, [7u8; 32], 1, 100_000 * GLC);

    assert!(
        matches!(
            fold(&mut ledger, 1, 10_000),
            SolFoldOutcome::FoldedManualReview { .. }
        ),
        "zero-conf-spendable change must not be counted toward the buffer"
    );
}

// ------------------------------------------- existing obligations survive --

/// Closing admission stops NEW obligations only. Anything already accepted
/// keeps its reservation and continues through the pipeline.
#[test]
fn existing_accepted_obligations_continue_after_admission_closes() {
    let mut ledger = setup(500_000);
    let accepted = fold(&mut ledger, 1, 10_000);
    assert!(is_accepted(&accepted));
    let id = request_id(&accepted);
    let state_before = ledger.get_request(id).unwrap().unwrap().state;
    let reserved_before = ledger
        .reserve_snapshot(ReserveDirection::GoldcoinReserve)
        .unwrap()
        .2;

    // Liquidity collapses; the rule closes admission.
    set_confirmed_balance(&mut ledger, ((100_000 * GLC) as i64) as u64, 9_000);
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::AutoClosed { .. }),
        "{t:?}"
    );
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // The accepted obligation is untouched: same state, same reservation.
    assert_eq!(ledger.get_request(id).unwrap().unwrap().state, state_before);
    assert_eq!(
        ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap()
            .2,
        reserved_before,
        "closing admission must not release or cancel an existing reservation"
    );

    // A NEW obligation is parked, not accepted.
    assert!(matches!(
        fold(&mut ledger, 2, 10_000),
        SolFoldOutcome::FoldedManualReview { .. }
    ));
}

/// The buffer never weakens the hard invariant, and never fires in its
/// place. With the buffer disabled, behaviour is exactly as before.
#[test]
fn the_hard_invariant_is_unchanged_by_the_buffer() {
    let mut ledger = setup(500_000);
    // Buffer off: pre-buffer behaviour restored exactly.
    ledger
        .set_admission_liquidity_buffer(ReserveDirection::GoldcoinReserve, 0, 0)
        .unwrap();
    // 100,000 confirmed, 20,000 protected => 80,000 headroom. A 10,000 GLC
    // request fits under the hard invariant and, with no buffer, is taken.
    set_confirmed_balance(&mut ledger, ((100_000 * GLC) as i64) as u64, 9_000);
    assert!(
        is_accepted(&fold(&mut ledger, 1, 10_000)),
        "with the buffer disabled the old rule must apply unchanged"
    );
    ledger
        .check_invariant(ReserveDirection::GoldcoinReserve)
        .expect("the hard invariant still holds");

    // And the hard invariant still refuses what it always refused.
    let huge = fold(&mut ledger, 2, 200_000);
    assert!(
        matches!(huge, SolFoldOutcome::FoldedManualReview { .. }),
        "the hard invariant must still refuse an oversized request: {huge:?}"
    );
    assert_eq!(
        park_reason(&ledger, request_id(&huge)).as_deref(),
        Some("insufficient_capacity_at_fold"),
        "with no buffer configured the reason must be the pre-existing one"
    );
}

// ------------------------------------------------------------ hysteresis --

#[test]
fn admission_closes_automatically_below_the_close_threshold() {
    let mut ledger = setup(500_000);
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // Headroom 250,000 - 1 atomic unit: just under the close threshold.
    let balance = PROTECTED_MINIMUM + CLOSE_BUFFER - 1;
    set_confirmed_balance(&mut ledger, (balance as i64) as u64, 9_000);
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::AutoClosed { .. }),
        "{t:?}"
    );

    let status = ledger
        .admission_liquidity_status(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(status.admission_closed);
    assert!(
        status.admission_auto_closed,
        "closure must be marked as ours"
    );
    assert_eq!(
        status.admission_reason.as_deref(),
        Some("auto_confirmed_liquidity_buffer")
    );
}

/// Exactly AT the close threshold is healthy — the rule closes strictly
/// below it.
#[test]
fn admission_stays_open_exactly_at_the_close_threshold() {
    let mut ledger = setup(500_000);
    let balance = PROTECTED_MINIMUM + CLOSE_BUFFER;
    set_confirmed_balance(&mut ledger, (balance as i64) as u64, 9_000);
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::Unchanged { .. }),
        "{t:?}"
    );
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// The hysteresis band: recovered past the close threshold but not yet to
/// the reopen threshold. Admission must stay shut.
#[test]
fn no_automatic_reopen_between_the_close_and_reopen_thresholds() {
    let mut ledger = setup(500_000);
    set_confirmed_balance(&mut ledger, ((50_000 * GLC) as i64) as u64, 9_000);
    ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // Walk the whole band: 250,000 up to 350,000 - 1 headroom.
    for headroom_glc in [250_000u64, 275_000, 300_000, 325_000, 349_999] {
        let balance = PROTECTED_MINIMUM + headroom_glc * GLC;
        set_confirmed_balance(&mut ledger, (balance as i64) as u64, 9_000);
        let t = ledger
            .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 3_000)
            .unwrap();
        assert!(
            matches!(
                t,
                AdmissionLiquidityTransition::HeldBelowReopenThreshold { .. }
            ),
            "headroom {headroom_glc} GLC is inside the band and must NOT reopen: {t:?}"
        );
        assert!(
            ledger
                .is_admission_closed(ReserveDirection::GoldcoinReserve)
                .unwrap(),
            "admission reopened at {headroom_glc} GLC, inside the hysteresis band"
        );
    }
}

#[test]
fn admission_reopens_automatically_at_the_reopen_threshold() {
    let mut ledger = setup(500_000);
    set_confirmed_balance(&mut ledger, ((50_000 * GLC) as i64) as u64, 9_000);
    ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());

    // Exactly at the reopen threshold: >=, so this reopens.
    let balance = PROTECTED_MINIMUM + REOPEN_BUFFER;
    set_confirmed_balance(&mut ledger, (balance as i64) as u64, 9_000);
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 3_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::AutoReopened { .. }),
        "{t:?}"
    );

    let status = ledger
        .admission_liquidity_status(ReserveDirection::GoldcoinReserve, 3_000)
        .unwrap();
    assert!(!status.admission_closed);
    assert!(
        !status.admission_auto_closed,
        "the auto flag must be cleared"
    );
    assert_eq!(status.admission_reason, None);
}

/// Oscillating around the CLOSE threshold must not flap admission, because
/// reopening requires clearing the higher REOPEN threshold.
#[test]
fn admission_does_not_flap_across_the_close_threshold() {
    let mut ledger = setup(500_000);
    let mut transitions = Vec::new();
    for headroom_glc in [
        249_000u64, 251_000, 249_000, 260_000, 240_000, 300_000, 249_500, 310_000,
    ] {
        let balance = PROTECTED_MINIMUM + headroom_glc * GLC;
        set_confirmed_balance(&mut ledger, (balance as i64) as u64, 9_000);
        transitions.push(
            ledger
                .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 4_000)
                .unwrap(),
        );
    }
    let closes = transitions
        .iter()
        .filter(|t| matches!(t, AdmissionLiquidityTransition::AutoClosed { .. }))
        .count();
    let reopens = transitions
        .iter()
        .filter(|t| matches!(t, AdmissionLiquidityTransition::AutoReopened { .. }))
        .count();
    assert_eq!(closes, 1, "must close exactly once: {transitions:?}");
    assert_eq!(
        reopens, 0,
        "nothing in this walk reaches 350,000 GLC, so it must never reopen: {transitions:?}"
    );
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

// ------------------------------------------- operator closure is sacred --

/// An operator closed admission for their own reason. Liquidity then
/// recovers past the reopen threshold. Automatic recovery must NOT reopen
/// it — the operator's decision stands until they reverse it.
#[test]
fn automatic_reopen_never_overrides_an_operator_closure() {
    let mut ledger = setup(1_000_000);
    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("operator: investigating an unrelated incident"),
        )
        .unwrap();

    // Liquidity is far above the reopen threshold.
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 5_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::HeldClosedByOperator { .. }),
        "{t:?}"
    );
    assert!(
        ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap(),
        "an operator closure must survive automatic liquidity recovery"
    );
    let status = ledger
        .admission_liquidity_status(ReserveDirection::GoldcoinReserve, 5_000)
        .unwrap();
    assert!(!status.admission_auto_closed);
    assert_eq!(
        status.admission_reason.as_deref(),
        Some("operator: investigating an unrelated incident"),
        "the operator's reason must not be overwritten"
    );
}

/// The converse: an operator closing admission while the buffer had
/// already auto-closed takes OWNERSHIP of the closure, so automatic
/// recovery stops being able to reopen it.
#[test]
fn an_operator_close_takes_ownership_of_an_existing_automatic_closure() {
    let mut ledger = setup(500_000);
    set_confirmed_balance(&mut ledger, ((50_000 * GLC) as i64) as u64, 9_000);
    ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(
        ledger
            .admission_liquidity_status(ReserveDirection::GoldcoinReserve, 2_000)
            .unwrap()
            .admission_auto_closed
    );

    ledger
        .set_admission(
            ReserveDirection::GoldcoinReserve,
            true,
            Some("operator: holding closed pending review"),
        )
        .unwrap();

    set_confirmed_balance(
        &mut ledger,
        PROTECTED_MINIMUM + REOPEN_BUFFER + 100 * GLC,
        9_000,
    );
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 3_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::HeldClosedByOperator { .. }),
        "{t:?}"
    );
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// Reopening by hand is held to the REOPEN threshold too, so a manual open
/// cannot slip admission back on inside the hysteresis band.
#[test]
fn a_manual_reopen_inside_the_band_is_refused() {
    let mut ledger = setup(500_000);
    // Headroom 300,000: above close, below reopen.
    set_confirmed_balance(&mut ledger, PROTECTED_MINIMUM + 300_000 * GLC, 9_000);
    let err = ledger
        .check_liquidity_buffer_for_admission(ReserveDirection::GoldcoinReserve, 6_000)
        .expect_err("inside the band, a manual reopen must be refused");
    assert!(
        matches!(err, LedgerError::AdmissionLiquidityBufferLow { .. }),
        "{err:?}"
    );

    // Above the reopen threshold it is allowed.
    set_confirmed_balance(&mut ledger, PROTECTED_MINIMUM + REOPEN_BUFFER, 9_000);
    ledger
        .check_liquidity_buffer_for_admission(ReserveDirection::GoldcoinReserve, 6_000)
        .expect("at the reopen threshold a manual open is permitted");
}

// ------------------------------------------------ restart / persistence --

/// Admission state and the configured thresholds are durable ledger state.
/// Reopening the database must not lose either, and must not silently
/// reopen an admission that was closed.
#[test]
fn restart_preserves_admission_state_and_thresholds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger, 500_000);
        set_confirmed_balance(&mut ledger, ((50_000 * GLC) as i64) as u64, 9_000);
        ledger
            .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 2_000)
            .unwrap();
        assert!(ledger
            .is_admission_closed(ReserveDirection::GoldcoinReserve)
            .unwrap());
    }
    let ledger = Ledger::open(&path).unwrap();
    let status = ledger
        .admission_liquidity_status(ReserveDirection::GoldcoinReserve, 2_000)
        .unwrap();
    assert!(status.admission_closed, "closure must survive a restart");
    assert!(
        status.admission_auto_closed,
        "and so must the fact that WE closed it — otherwise recovery could never reopen it"
    );
    assert_eq!(status.safety_buffer_atomic, CLOSE_BUFFER as i64);
    assert_eq!(status.reopen_buffer_atomic, REOPEN_BUFFER as i64);
}

/// An operator closure also survives a restart AS an operator closure, so
/// automatic recovery still refuses to touch it after a daemon restart.
#[test]
fn restart_preserves_the_distinction_between_operator_and_automatic_closure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&path).unwrap();
        configure(&mut ledger, 1_000_000);
        ledger
            .set_admission(ReserveDirection::GoldcoinReserve, true, Some("operator"))
            .unwrap();
    }
    let mut ledger = Ledger::open(&path).unwrap();
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 7_000)
        .unwrap();
    assert!(
        matches!(t, AdmissionLiquidityTransition::HeldClosedByOperator { .. }),
        "{t:?}"
    );
    assert!(ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

// -------------------------------------------- ManualReview cannot bypass --

/// Resuming a parked request re-admits real demand exactly as a fresh fold
/// does, so it is held to the same buffer. Otherwise a backlog would drain
/// straight through the cushion back down to the hard invariant.
#[test]
fn manual_review_recovery_cannot_bypass_the_safety_buffer() {
    let mut ledger = setup(275_000);
    let parked = fold(&mut ledger, 1, 10_000);
    let id = request_id(&parked);
    assert!(matches!(parked, SolFoldOutcome::FoldedManualReview { .. }));

    // Mark the source finalized so the resume gets as far as the capacity
    // checks rather than refusing earlier.
    let err = ledger
        .resume_manual_review_sol_to_glc(id, "operator retry", "cli:test", 3_000)
        .expect_err("resuming must not be able to spend the buffer");
    assert!(
        matches!(err, LedgerError::LiquidityBufferLow { .. }),
        "expected the buffer to be the refusal reason, got {err:?}"
    );

    // Still parked, nothing reserved.
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::ManualReview
    );
    assert_eq!(
        ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)
            .unwrap()
            .2,
        0,
        "a refused resume must reserve nothing"
    );
}

/// Once confirmed liquidity genuinely recovers, the same resume succeeds —
/// the refusal is transient backpressure, not a terminal state.
#[test]
fn manual_review_recovery_succeeds_once_confirmed_liquidity_recovers() {
    let mut ledger = setup(275_000);
    let parked = fold(&mut ledger, 1, 10_000);
    let id = request_id(&parked);
    assert!(matches!(parked, SolFoldOutcome::FoldedManualReview { .. }));
    set_confirmed_balance(&mut ledger, ((600_000 * GLC) as i64) as u64, 9_000);
    ledger
        .resume_manual_review_sol_to_glc(id, "operator retry", "cli:test", 4_000)
        .expect("with ample confirmed liquidity the resume must succeed");
    assert_eq!(
        ledger.get_request(id).unwrap().unwrap().state,
        RequestState::SourceFinalized
    );
}

// ------------------------------------------------------- configuration --

/// A reopen threshold below the close threshold would reopen admission
/// while it was still closing. Refused at configuration time.
#[test]
fn an_inverted_hysteresis_configuration_is_refused() {
    let mut ledger = setup(500_000);
    let err = ledger
        .set_admission_liquidity_buffer(
            ReserveDirection::GoldcoinReserve,
            REOPEN_BUFFER,
            CLOSE_BUFFER,
        )
        .expect_err("reopen < close must be refused");
    assert!(
        matches!(err, LedgerError::InvalidAdmissionLiquidityBuffer { .. }),
        "{err:?}"
    );
}

/// With the buffer unconfigured the state machine is inert in both
/// directions — it never closes, and never reopens something it did not
/// close.
#[test]
fn a_disabled_buffer_is_inert() {
    let mut ledger = setup(500_000);
    ledger
        .set_admission_liquidity_buffer(ReserveDirection::GoldcoinReserve, 0, 0)
        .unwrap();
    set_confirmed_balance(&mut ledger, ((25_000 * GLC) as i64) as u64, 9_000);
    let t = ledger
        .evaluate_liquidity_admission(ReserveDirection::GoldcoinReserve, 8_000)
        .unwrap();
    assert!(matches!(t, AdmissionLiquidityTransition::Disabled), "{t:?}");
    assert!(!ledger
        .is_admission_closed(ReserveDirection::GoldcoinReserve)
        .unwrap());
}

/// The production policy values, asserted as numbers so a later edit to
/// either constant has to come past this test.
#[test]
fn the_production_thresholds_are_250k_and_350k_glc() {
    assert_eq!(
        CLOSE_BUFFER, 25_000_000_000_000,
        "250,000 GLC at 8 decimals"
    );
    assert_eq!(
        REOPEN_BUFFER, 35_000_000_000_000,
        "350,000 GLC at 8 decimals"
    );
    const { assert!(REOPEN_BUFFER > CLOSE_BUFFER, "the gap is the hysteresis") };
}
