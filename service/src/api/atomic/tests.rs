//! Round-trip and boundary tests for the string atomic-amount encoding.
//!
//! The boundary that matters is `Number.MAX_SAFE_INTEGER` (`2^53 - 1`).
//! Values at, just past, and far past it are all pinned here, alongside
//! the exact production value that broke the Reserves page.

use super::*;

/// `Number.MAX_SAFE_INTEGER` — the largest integer a JavaScript double
/// represents exactly.
const MAX_SAFE: u64 = 9_007_199_254_740_991;
/// One past it. `JSON.parse("9007199254740992")` happens to round-trip,
/// but `9007199254740993` does not — the spacing between representable
/// doubles is 2 from here on, so this is where exactness stops being
/// guaranteed and the string encoding starts earning its keep.
const JUST_PAST_SAFE: u64 = 9_007_199_254_740_992;
/// The exact production value from `GET /stats`
/// `goldcoin_reserve.settled_volume_atomic` that a JavaScript client
/// parsed as `9408405829927560`.
const PRODUCTION_SETTLED_VOLUME: u64 = 9_408_405_829_927_559;

#[test]
fn amounts_serialize_as_decimal_strings() {
    for v in [
        0,
        1,
        MAX_SAFE - 1,
        MAX_SAFE,
        JUST_PAST_SAFE,
        PRODUCTION_SETTLED_VOLUME,
        u64::MAX,
    ] {
        let json = serde_json::to_string(&AtomicU64(v)).unwrap();
        assert_eq!(
            json,
            format!("\"{v}\""),
            "an atomic amount must be written as a quoted decimal string"
        );
    }
}

/// The regression itself: the production figure must reach a client as
/// digits, not as a double.
#[test]
fn the_production_settled_volume_survives_the_wire_exactly() {
    let json = serde_json::to_string(&AtomicU64(PRODUCTION_SETTLED_VOLUME)).unwrap();
    assert_eq!(json, "\"9408405829927559\"");

    // The value is genuinely outside the JavaScript-safe range — if this
    // ever stops being true the test is checking nothing. Compared through
    // `const` bindings so this reads as an assertion rather than a
    // compile-time constant clippy can fold away.
    let (production, max_safe) = (PRODUCTION_SETTLED_VOLUME, MAX_SAFE);
    assert!(production > max_safe);

    let back: AtomicU64 = serde_json::from_str(&json).unwrap();
    assert_eq!(back.0, PRODUCTION_SETTLED_VOLUME);
}

/// Proof that the OLD representation was genuinely lossy, so this
/// encoding is not solving a hypothetical. Reproduces the JavaScript
/// parse in Rust: `f64` is the same IEEE-754 double every JS engine uses.
#[test]
fn the_old_numeric_representation_was_lossy_for_this_value() {
    let as_double = PRODUCTION_SETTLED_VOLUME as f64;
    assert_eq!(
        as_double as u64, 9_408_405_829_927_560,
        "the production value does not survive a double — this is the defect"
    );
    assert_ne!(as_double as u64, PRODUCTION_SETTLED_VOLUME);

    // And the boundary below which a double is still exact.
    assert_eq!(MAX_SAFE as f64 as u64, MAX_SAFE);
}

#[test]
fn signed_amounts_serialize_as_decimal_strings_including_negatives() {
    for v in [
        0i64,
        -1,
        1,
        MAX_SAFE as i64,
        JUST_PAST_SAFE as i64,
        -(JUST_PAST_SAFE as i64),
        PRODUCTION_SETTLED_VOLUME as i64,
        i64::MIN,
        i64::MAX,
    ] {
        let json = serde_json::to_string(&AtomicI64(v)).unwrap();
        assert_eq!(json, format!("\"{v}\""));
        let back: AtomicI64 = serde_json::from_str(&json).unwrap();
        assert_eq!(back.0, v, "signed amounts must round-trip exactly");
    }
}

#[test]
fn amounts_round_trip_through_json() {
    for v in [
        0,
        1,
        MAX_SAFE,
        JUST_PAST_SAFE,
        PRODUCTION_SETTLED_VOLUME,
        u64::MAX,
    ] {
        let json = serde_json::to_string(&AtomicU64(v)).unwrap();
        let back: AtomicU64 = serde_json::from_str(&json).unwrap();
        assert_eq!(back.0, v);
    }
}

/// Backward compatibility for INPUTS: an existing client posting a JSON
/// number keeps working.
#[test]
fn a_json_number_is_still_accepted_on_input() {
    let n: AtomicU64 = serde_json::from_str("12345").unwrap();
    assert_eq!(n.0, 12_345);
    let n: AtomicU64 = serde_json::from_str("0").unwrap();
    assert_eq!(n.0, 0);
    let n: AtomicU64 = serde_json::from_str(&MAX_SAFE.to_string()).unwrap();
    assert_eq!(n.0, MAX_SAFE);

    let s: AtomicI64 = serde_json::from_str("-42").unwrap();
    assert_eq!(s.0, -42);
}

/// A float is refused rather than truncated: flooring `1.5` atomic units
/// would be exactly the silent precision loss this encoding exists to
/// prevent.
#[test]
fn a_float_is_refused_never_truncated() {
    let err = serde_json::from_str::<AtomicU64>("1.5").unwrap_err();
    assert!(err.to_string().contains("not an integer amount"), "{err}");
    let err = serde_json::from_str::<AtomicI64>("-1.5").unwrap_err();
    assert!(err.to_string().contains("not an integer amount"), "{err}");
}

#[test]
fn malformed_amount_strings_are_refused() {
    for bad in [
        "\"\"",
        "\" 1\"",
        "\"1 \"",
        "\"+1\"",
        "\"1_000\"",
        "\"0x10\"",
        "\"1e3\"",
        "\"abc\"",
        "\"1.0\"",
        "\"18446744073709551616\"", // u64::MAX + 1
    ] {
        assert!(
            serde_json::from_str::<AtomicU64>(bad).is_err(),
            "{bad} must not parse as an atomic amount"
        );
    }
    assert!(serde_json::from_str::<AtomicU64>("\"-1\"").is_err());
    // The signed form accepts a negative, and still refuses the rest.
    assert_eq!(serde_json::from_str::<AtomicI64>("\"-1\"").unwrap().0, -1);
    assert!(serde_json::from_str::<AtomicI64>("\"--1\"").is_err());
}

#[test]
fn a_negative_number_is_refused_for_an_unsigned_amount() {
    let err = serde_json::from_str::<AtomicU64>("-1").unwrap_err();
    assert!(err.to_string().contains("negative"), "{err}");
}

/// Values far beyond anything the bridge will hold must still be exact —
/// the encoding is not tuned to today's magnitudes.
#[test]
fn much_larger_future_values_are_exact() {
    for v in [
        1_000_000_000_000_000_000u64,
        9_000_000_000_000_000_001,
        u64::MAX - 1,
        u64::MAX,
    ] {
        let json = serde_json::to_string(&AtomicU64(v)).unwrap();
        assert_eq!(json, format!("\"{v}\""));
        assert_eq!(serde_json::from_str::<AtomicU64>(&json).unwrap().0, v);
    }
    // Not every large value is inexact as a double — 10^18 happens to be
    // representable — so exactness is asserted above for all of them, and
    // the "a double would have lost this" claim only for values that
    // genuinely are beyond it. Compared in `u128`: a float-to-`u64` cast
    // SATURATES in Rust, which would hide the loss for values near
    // `u64::MAX` (whose double is 2^64, one past the type).
    for v in [9_000_000_000_000_000_001u64, u64::MAX - 1, u64::MAX] {
        assert_ne!(
            v as f64 as u128, v as u128,
            "{v} was chosen because a double cannot hold it"
        );
    }
}
