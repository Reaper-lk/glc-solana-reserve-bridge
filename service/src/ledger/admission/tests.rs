//! The shared admission evaluator's ranking and its route-level form.
//!
//! These are pure-function tests over [`InboundAdmissionGates`]: no
//! database, no reserve row. What they pin is the part that used to be
//! duplicated — the ranking, and the exact relationship between the
//! per-request decision a fold makes and the amount-independent one the
//! public API reports.

use super::*;

/// Every gate open, comfortable headroom, no floor and no buffer.
fn healthy() -> InboundAdmissionGates {
    InboundAdmissionGates {
        paused: false,
        admission_closed: false,
        liquidity_admission_closed: false,
        confirmed_headroom_atomic: 1_000_000,
        admission_buffer_atomic: 0,
        min_available_utxo_count: 0,
        available_utxo_count: 0,
    }
}

#[test]
fn a_healthy_reserve_admits() {
    assert_eq!(healthy().blocker(1_000, InboundRateLimits::default()), None);
    assert_eq!(healthy().route_blocker(), None);
}

/// The ranking, stated once as data. Each row sets exactly one gate on
/// top of `healthy()` and asserts the blocker it must produce.
#[test]
fn each_gate_produces_its_own_blocker_and_note() {
    let cases: [(
        InboundAdmissionGates,
        InboundRateLimits,
        InboundAdmissionBlocker,
        &str,
    ); 6] = [
        (
            InboundAdmissionGates {
                admission_closed: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::AdmissionClosed,
            "admission_closed_at_fold",
        ),
        (
            InboundAdmissionGates {
                paused: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::ReservePaused,
            "reserve_paused_at_fold",
        ),
        (
            healthy(),
            InboundRateLimits {
                source_wallet_rate_limited: true,
                recipient_rate_limited: false,
            },
            InboundAdmissionBlocker::SourceWalletRateLimited,
            "source_wallet_rate_limited",
        ),
        (
            healthy(),
            InboundRateLimits {
                source_wallet_rate_limited: false,
                recipient_rate_limited: true,
            },
            InboundAdmissionBlocker::RecipientRateLimited,
            "recipient_rate_limited",
        ),
        (
            InboundAdmissionGates {
                min_available_utxo_count: 5,
                available_utxo_count: 5,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::UtxoLiquidityLow,
            "utxo_liquidity_low_at_fold",
        ),
        (
            InboundAdmissionGates {
                liquidity_admission_closed: true,
                ..healthy()
            },
            InboundRateLimits::default(),
            InboundAdmissionBlocker::LiquidityBufferLow,
            "liquidity_buffer_low_at_fold",
        ),
    ];
    for (gates, limits, expected, note) in cases {
        assert_eq!(
            gates.blocker(1_000, limits),
            Some(expected),
            "wrong blocker for {expected:?}"
        );
        assert_eq!(expected.manual_review_note(), note);
    }
}

/// Plain capacity exhaustion is the LAST resort, reached only when every
/// more specific gate is open.
#[test]
fn insufficient_capacity_is_the_fallback() {
    let gates = InboundAdmissionGates {
        confirmed_headroom_atomic: 500,
        ..healthy()
    };
    assert_eq!(
        gates.blocker(1_000, InboundRateLimits::default()),
        Some(InboundAdmissionBlocker::InsufficientCapacity)
    );
    assert_eq!(
        InboundAdmissionBlocker::InsufficientCapacity.manual_review_note(),
        "insufficient_capacity_at_fold"
    );
    // ...but the reserve is not closed to ALL demand: a small enough
    // deposit still fits, which is exactly what `route_blocker` reports.
    assert_eq!(gates.blocker(500, InboundRateLimits::default()), None);
    assert_eq!(gates.route_blocker(), None);
}

/// The ranking order itself: with EVERY gate closed at once, the most
/// specific one wins, and removing it reveals the next. This is the
/// property that used to live in two hand-written `else if` chains.
#[test]
fn the_ranking_is_most_specific_first() {
    let all_closed = InboundAdmissionGates {
        paused: true,
        admission_closed: true,
        liquidity_admission_closed: true,
        confirmed_headroom_atomic: -1,
        admission_buffer_atomic: 10,
        min_available_utxo_count: 5,
        available_utxo_count: 0,
    };
    let both_limits = InboundRateLimits {
        source_wallet_rate_limited: true,
        recipient_rate_limited: true,
    };
    let expected = [
        InboundAdmissionBlocker::AdmissionClosed,
        InboundAdmissionBlocker::ReservePaused,
        InboundAdmissionBlocker::SourceWalletRateLimited,
        InboundAdmissionBlocker::RecipientRateLimited,
        InboundAdmissionBlocker::UtxoLiquidityLow,
        InboundAdmissionBlocker::LiquidityBufferLow,
        InboundAdmissionBlocker::InsufficientCapacity,
    ];
    let mut gates = all_closed;
    let mut limits = both_limits;
    for step in expected {
        assert_eq!(
            gates.blocker(1_000, limits),
            Some(step),
            "expected {step:?} at this point in the ranking"
        );
        match step {
            InboundAdmissionBlocker::AdmissionClosed => gates.admission_closed = false,
            InboundAdmissionBlocker::ReservePaused => gates.paused = false,
            InboundAdmissionBlocker::SourceWalletRateLimited => {
                limits.source_wallet_rate_limited = false
            }
            InboundAdmissionBlocker::RecipientRateLimited => limits.recipient_rate_limited = false,
            InboundAdmissionBlocker::UtxoLiquidityLow => gates.min_available_utxo_count = 0,
            InboundAdmissionBlocker::LiquidityBufferLow => {
                gates.liquidity_admission_closed = false;
                gates.admission_buffer_atomic = 0;
            }
            InboundAdmissionBlocker::InsufficientCapacity => {}
        }
    }
}

/// `route_blocker` is not a re-statement of the amount-dependent gates,
/// it is literally the real decision at the smallest amount that can
/// exist. Pinned so nobody "optimises" it into a second formula.
#[test]
fn route_blocker_is_the_real_decision_at_one_atomic_unit() {
    for headroom in [-5i64, 0, 1, 2, 99, 100, 101, 1_000] {
        for buffer in [0i64, 1, 100] {
            for (min_count, count) in [(0i64, 0i64), (5, 5), (5, 6)] {
                let gates = InboundAdmissionGates {
                    confirmed_headroom_atomic: headroom,
                    admission_buffer_atomic: buffer,
                    min_available_utxo_count: min_count,
                    available_utxo_count: count,
                    ..healthy()
                };
                assert_eq!(
                    gates.route_blocker(),
                    gates.blocker(1, InboundRateLimits::default()),
                    "route_blocker must BE blocker(1, no limits) — headroom {headroom}, \
                     buffer {buffer}, utxo {count}/{min_count}"
                );
            }
        }
    }
}

/// The weakest-form identities the route-level answer relies on:
/// `available` is `headroom > 0` when no buffer is configured, and
/// `headroom > buffer` when one is. Asserted against `route_blocker`'s
/// actual output rather than assumed.
#[test]
fn route_availability_is_the_weakest_form_of_the_amount_gates() {
    for headroom in [-1i64, 0, 1, 2] {
        let gates = InboundAdmissionGates {
            confirmed_headroom_atomic: headroom,
            ..healthy()
        };
        assert_eq!(
            gates.route_blocker().is_none(),
            headroom > 0,
            "with no buffer, any admission at all requires headroom > 0 (headroom {headroom})"
        );
    }
    for headroom in [99i64, 100, 101] {
        let gates = InboundAdmissionGates {
            confirmed_headroom_atomic: headroom,
            admission_buffer_atomic: 100,
            ..healthy()
        };
        assert_eq!(
            gates.route_blocker().is_none(),
            headroom > 100,
            "with a buffer of 100, any admission at all requires headroom > 100 \
             (headroom {headroom})"
        );
    }
}

/// A disabled floor (`min_available_utxo_count == 0`) never blocks, no
/// matter how empty the pool is — the short-circuit every caller relies
/// on for reserves that have no vault pool at all.
#[test]
fn a_disabled_utxo_floor_never_blocks() {
    let gates = InboundAdmissionGates {
        min_available_utxo_count: 0,
        available_utxo_count: 0,
        ..healthy()
    };
    assert_eq!(gates.route_blocker(), None);
}
