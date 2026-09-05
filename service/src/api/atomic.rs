//! Lossless JSON representation for atomic monetary amounts.
//!
//! # The defect this exists to prevent
//!
//! Every atomic amount in this ledger is a `u64` or `i64`. JSON has one
//! numeric type, and every mainstream JavaScript client parses it into an
//! IEEE-754 double, which represents integers exactly only up to
//! `2^53 - 1` (`Number.MAX_SAFE_INTEGER`, 9 007 199 254 740 991).
//!
//! Production `GET /stats` reported
//! `goldcoin_reserve.settled_volume_atomic = 9408405829927559`. That is
//! above the safe range, so `JSON.parse` silently returned
//! `9408405829927560` — a different number, wrong by one atomic unit,
//! with no error anywhere. The bridge UI's schema validation rejected it
//! (correctly: it refuses to render a value it cannot represent exactly)
//! and the Reserves page failed to load.
//!
//! The corruption happens inside the client's JSON parser, BEFORE any
//! validation can see it. No amount of client-side leniency can recover
//! the original digits — accepting the value would only mean displaying a
//! wrong balance instead of an error. The representation itself has to
//! change, and it has to change on the wire.
//!
//! # The representation
//!
//! A decimal string: `"9408405829927559"`. JSON strings survive every
//! parser byte-for-byte, so the client receives the exact digits and can
//! widen them to a `BigInt` — which is what the bridge UI already uses
//! internally for all amount arithmetic and formatting.
//!
//! Not hex, not scientific notation, not an object wrapper: the plainest
//! encoding that round-trips, and one a human reading a response body can
//! still read at a glance.
//!
//! # Which fields use this, and which deliberately do not
//!
//! Only fields carrying an atomic monetary amount whose magnitude this
//! service does not bound. Those are unbounded by construction: cumulative
//! settled volume and accrued fees only ever grow, reserve capacities and
//! rolling-volume limits are operator-set `u64`s, and a transfer amount is
//! bounded only by the per-transfer limit — itself an operator-set `u64`.
//!
//! Counts, timestamps, ids, decimals and basis points stay plain JSON
//! numbers. They are bounded by something real (row counts, unix seconds,
//! `0..=10_000` for bps, `0..=30` for decimals) and none can approach
//! `2^53`, so a string would add friction to every client for no
//! correctness gain. The distinction is recorded per field at the DTO
//! definitions in [`crate::api`], not left to inference here.
//!
//! # Compatibility
//!
//! Serialization is one-way — an amount is always WRITTEN as a string, so
//! the contract is unambiguous and self-consistent (docs/09-runbook.md's
//! "Atomic amounts are strings"). Deserialization deliberately accepts
//! BOTH a string and a JSON number, which keeps every existing client that
//! POSTs `{"amount_atomic": 12345}` to `/transfers` or `/quote` working
//! unchanged. A JSON number arriving on an input is still exact whenever
//! the sender could represent it exactly; a sender that cannot has the
//! string form available and no reason to use a number.
//!
//! Floats are refused outright on input rather than truncated: `1.5`
//! atomic units is not a quantity this ledger has, and silently flooring
//! it would be exactly the kind of quiet precision loss this module
//! exists to stop.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An unsigned atomic monetary amount, serialized as a decimal string.
///
/// Deserializes from a decimal string or a JSON integer — see the module
/// docs on why the input side is deliberately more permissive than the
/// output side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct AtomicU64(pub u64);

/// The signed twin of [`AtomicU64`], for amounts that are legitimately
/// negative: an available capacity that has gone below zero, and a
/// reconciliation delta, which is a difference and negative exactly when
/// the observed balance is short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct AtomicI64(pub i64);

impl From<u64> for AtomicU64 {
    fn from(v: u64) -> Self {
        Self(v)
    }
}
impl From<AtomicU64> for u64 {
    fn from(v: AtomicU64) -> Self {
        v.0
    }
}
impl From<i64> for AtomicI64 {
    fn from(v: i64) -> Self {
        Self(v)
    }
}
impl From<AtomicI64> for i64 {
    fn from(v: AtomicI64) -> Self {
        v.0
    }
}

impl fmt::Display for AtomicU64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl fmt::Display for AtomicI64 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for AtomicU64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}
impl Serialize for AtomicI64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

/// Accepts `"123"` or `123`; refuses a float, an empty string, a `+`
/// sign, leading/trailing whitespace, and anything out of range. Strict on
/// purpose: an amount that does not parse cleanly is a malformed request,
/// never a value to guess at.
struct AtomicU64Visitor;

impl Visitor<'_> for AtomicU64Visitor {
    type Value = AtomicU64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an atomic amount as a decimal string (preferred) or a JSON integer")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        // `u64::from_str` accepts a leading `+`; the canonical form this
        // service emits never has one, and accepting non-canonical spellings
        // on input invites clients to produce them.
        if v.starts_with('+') {
            return Err(de::Error::custom(format!(
                "{v:?} is not a canonical decimal amount (no leading '+')"
            )));
        }
        v.parse::<u64>().map(AtomicU64).map_err(|_| {
            de::Error::custom(format!(
                "{v:?} is not a non-negative decimal integer amount"
            ))
        })
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        Ok(AtomicU64(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        u64::try_from(v)
            .map(AtomicU64)
            .map_err(|_| de::Error::custom(format!("{v} is negative; amount must be >= 0")))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Err(de::Error::custom(format!(
            "{v} is not an integer amount; send atomic amounts as a decimal string"
        )))
    }
}

impl<'de> Deserialize<'de> for AtomicU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AtomicU64Visitor)
    }
}

struct AtomicI64Visitor;

impl Visitor<'_> for AtomicI64Visitor {
    type Value = AtomicI64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an atomic amount as a decimal string (preferred) or a JSON integer")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        // See the unsigned visitor: `-` is meaningful here, `+` is not.
        if v.starts_with('+') {
            return Err(de::Error::custom(format!(
                "{v:?} is not a canonical decimal amount (no leading '+')"
            )));
        }
        v.parse::<i64>()
            .map(AtomicI64)
            .map_err(|_| de::Error::custom(format!("{v:?} is not a decimal integer amount")))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
        Ok(AtomicI64(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
        i64::try_from(v)
            .map(AtomicI64)
            .map_err(|_| de::Error::custom(format!("{v} exceeds the signed amount range")))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Self::Value, E> {
        Err(de::Error::custom(format!(
            "{v} is not an integer amount; send atomic amounts as a decimal string"
        )))
    }
}

impl<'de> Deserialize<'de> for AtomicI64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AtomicI64Visitor)
    }
}

#[cfg(test)]
mod tests;
