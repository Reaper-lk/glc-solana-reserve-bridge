//! Robinhood Chain atomic amounts, and their exact conversion to and from
//! the ledger's canonical accounting unit (Phase 2 / Phase A,
//! docs/robinhood-chain-bridge-architecture-handoff.md §8 "Amount
//! conversion" and §13's "must land first" note).
//!
//! # Why this module exists at all
//!
//! Every other amount in this service fits a `u64`. Robinhood's GLC token
//! has **18 decimals**, and at 18 decimals a `u64` tops out at 18.4467
//! GLC — the designed 20,000 GLC per-transfer limit overflows it by three
//! orders of magnitude, and the 100,000 GLC rolling limit by four. A
//! Robinhood-native amount therefore CANNOT share a representation with
//! [`CanonicalAtomic`] or [`crate::amount_conversion::SolanaAtomic`], and
//! any code path that lets an 18-decimal value reach the canonical `u64`
//! pipeline is a silent-truncation bug waiting to happen (risk R2 in the
//! handoff's register).
//!
//! [`RobinhoodAtomic`] is that separate representation: a `u128` that
//! implements no arithmetic, comparison or conversion against the
//! canonical types except through the two explicit, exactness-checked
//! functions below. Rust's type system, not a runtime check, is what
//! makes `RobinhoodAtomic(5) + CanonicalAtomic(5)` a compile error — the
//! same discipline `CanonicalAtomic`/`SolanaAtomic` already use in the
//! parent module, extended to a third unit.
//!
//! # The conversion policy
//!
//! ```text
//! SCALE = 10^(ROBINHOOD_DECIMALS - GOLDCOIN_DECIMALS) = 10^(18 - 8) = 10^10
//!
//! canonical -> robinhood (widening):   R = C * SCALE           always exact
//! robinhood -> canonical (narrowing):  require R % SCALE == 0
//!                                      C = R / SCALE, must fit u64
//! ABI boundary:                        require u256 <= u128::MAX
//! ```
//!
//! This is the identical exactness policy the parent module applies to
//! Goldcoin<->Solana, for the identical reason: rounding down would
//! permanently strand the depositor's entitlement to the remainder inside
//! the reserve, and rounding up would create GLC that was never
//! deposited. Both directions fail closed instead. Nothing here rounds,
//! truncates, saturates, or casts with `as`.
//!
//! # Why widening cannot fail
//!
//! The largest canonical amount representable at all is `u64::MAX`, and
//!
//! ```text
//! u64::MAX * SCALE = 18_446_744_073_709_551_615 * 10^10
//!                  = 184_467_440_737_095_516_150_000_000_000
//!                  ~ 1.8447 * 10^29
//!
//! u128::MAX        = 340_282_366_920_938_463_463_374_607_431_768_211_455
//!                  ~ 3.4028 * 10^38
//! ```
//!
//! so the worst case sits about 1.845 * 10^9 times below the ceiling.
//! Widening is therefore total over the whole `u64` domain. It is still
//! written with checked arithmetic and still returns a `Result`: the
//! proof depends on two constants, and a future decimals change that
//! invalidated it must surface as a rejected conversion rather than a
//! wrapped one. [`tests`] proves the property both symbolically (at the
//! exact `u64::MAX` boundary) and by sampling.
//!
//! Narrowing and ABI decode are the only conversions that can genuinely
//! fail on real values, and both do so loudly.
//!
//! # `EvmU256` and the ABI boundary
//!
//! The EVM ABI and `eth_*` JSON-RPC speak 256-bit words; this service's
//! accounting does not, and must not. [`EvmU256`] is a deliberately inert
//! 32-byte big-endian container that exists only to hold an ABI word long
//! enough to check it and narrow it. It has no arithmetic: the one
//! operation the bridge ever needs on an inbound word is "does this fit
//! in a `RobinhoodAtomic`, yes or no", and the one operation it needs
//! outbound is lossless widening. When the real EVM RPC/ABI stack lands
//! (a later phase — this one adds no chain client and no dependency), it
//! plugs in through [`EvmU256::from_be_bytes`]/[`EvmU256::to_be_bytes`],
//! which is the representation every EVM library agrees on.

use std::fmt;

use super::{CanonicalAtomic, GOLDCOIN_DECIMALS};

/// The Robinhood Chain GLC token's precision. Asserted against the live
/// on-chain `decimals()` at daemon startup — see
/// [`ensure_robinhood_decimals`] — rather than trusted blindly, exactly
/// as the Solana side reads `reserve_mint.decimals` live. Unlike the
/// Solana mint's decimals this one is NOT threaded through the
/// conversion functions as a parameter: an 18-decimal token is what makes
/// the separate `u128` unit necessary in the first place, so a different
/// value does not mean "convert differently", it means "this is not the
/// asset this code models" and the daemon must refuse to start.
pub const ROBINHOOD_DECIMALS: u32 = 18;

/// `10^(ROBINHOOD_DECIMALS - GOLDCOIN_DECIMALS)` = `10^10`: the exact
/// factor between one canonical atomic unit (1e-8 GLC) and one Robinhood
/// atomic unit (1e-18 GLC). Written out longhand and cross-checked
/// against `10u128.pow(...)` in [`tests`] so a typo cannot silently
/// mis-scale every amount the bridge ever moves.
pub const CANONICAL_TO_ROBINHOOD_SCALE: u128 = 10_000_000_000;

/// Everything that can go wrong converting between the canonical
/// accounting unit, the Robinhood-native unit, and the EVM ABI word.
///
/// Kept separate from the parent module's [`ConversionError`] rather than
/// bolted onto it: that enum's variants are all `u64`-shaped and describe
/// the Goldcoin<->Solana decimal pair, and widening it with `u128`
/// payloads would change an error surface that existing settlement code
/// already matches on. Same `thiserror` style, same fail-closed intent,
/// distinct unit domain.
///
/// [`ConversionError`]: crate::amount_conversion::ConversionError
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RobinhoodConversionError {
    /// Widening a canonical amount overflowed `u128`. Unreachable for
    /// every `u64` input at the current decimals (see the module docs'
    /// proof); reachable only if `ROBINHOOD_DECIMALS`/`GOLDCOIN_DECIMALS`
    /// change. Reported rather than asserted so such a change fails a
    /// conversion instead of wrapping one.
    #[error(
        "canonical amount {canonical} atomic unit(s) overflows u128 when widened by \
         10^{scale_exponent} to Robinhood's 18-decimal precision"
    )]
    WideningOverflow { canonical: u64, scale_exponent: u32 },

    /// The Robinhood amount is not a whole multiple of
    /// [`CANONICAL_TO_ROBINHOOD_SCALE`], so it has no exact
    /// representation in canonical 8-decimal units. Rejected, never
    /// rounded: the `remainder` is real user value that neither
    /// direction of rounding can move honestly.
    #[error(
        "Robinhood amount {robinhood} atomic unit(s) (18 dp) cannot be represented exactly \
         in canonical 8-decimal units — converting would silently strand/create {remainder} \
         atomic unit(s) of precision the canonical ledger cannot express"
    )]
    NotExactlyRepresentable { robinhood: u128, remainder: u128 },

    /// The Robinhood amount is an exact multiple of the scale, but the
    /// resulting canonical quantity is larger than the canonical `u64`
    /// unit can hold. `u128::MAX / SCALE` is about 3.4 * 10^28, some 1.8
    /// billion times `u64::MAX`, so this is genuinely reachable from a
    /// hostile or malfunctioning chain read and must not truncate.
    #[error(
        "Robinhood amount {robinhood} atomic unit(s) (18 dp) scales down to {canonical}, \
         which exceeds the canonical ledger's u64 atomic-unit range"
    )]
    CanonicalOverflow { robinhood: u128, canonical: u128 },

    /// An inbound 256-bit ABI word does not fit a `u128`. Never
    /// truncated to the low 128 bits — a word this large is either a
    /// different asset's units, a decoding error, or an attack, and none
    /// of those should become a payable amount.
    #[error(
        "EVM word {value} exceeds u128::MAX and cannot be decoded as a Robinhood atomic \
         amount without truncation"
    )]
    U256ExceedsU128 { value: EvmU256 },

    /// Checked `u128` arithmetic on two Robinhood amounts overflowed or
    /// underflowed. Mirrors the parent module's
    /// `ConversionError::Overflow` role for the canonical types.
    #[error("Robinhood atomic arithmetic overflowed: {lhs} {op} {rhs}")]
    ArithmeticOverflow {
        lhs: u128,
        op: &'static str,
        rhs: u128,
    },

    /// The live on-chain `decimals()` is not [`ROBINHOOD_DECIMALS`]. See
    /// that constant's docs for why this is a refuse-to-start condition
    /// and not a "convert differently" one.
    #[error(
        "Robinhood token reports {observed} decimals, but this bridge's amount model is \
         built for exactly {expected} — refusing to treat a differently-scaled token as GLC"
    )]
    DecimalsMismatch { observed: u8, expected: u32 },
}

/// Asserts that a live, on-chain-read `decimals()` matches the precision
/// this module's whole conversion policy is derived from. Called at
/// startup by whatever eventually reads the token contract; the amount
/// model itself never takes decimals as a parameter, so this is the one
/// and only place the assumption is checked.
pub fn ensure_robinhood_decimals(observed: u8) -> Result<(), RobinhoodConversionError> {
    if u32::from(observed) == ROBINHOOD_DECIMALS {
        Ok(())
    } else {
        Err(RobinhoodConversionError::DecimalsMismatch {
            observed,
            expected: ROBINHOOD_DECIMALS,
        })
    }
}

/// An amount in Robinhood Chain's native atomic unit: 1e-18 GLC, held in
/// a `u128`.
///
/// The inner value is **private**, unlike [`CanonicalAtomic`] and
/// `SolanaAtomic`, which expose theirs. The asymmetry is deliberate and
/// is the point of the type. Those two units are numerically
/// interchangeable in the sense that a bare `u64` from either is at least
/// the right ORDER of magnitude if it is ever mixed up; a Robinhood value
/// is 10^10 times larger than the canonical value it represents, so a
/// tuple-struct field that let any call site write
/// `RobinhoodAtomic(canonical.0)` would reintroduce exactly the
/// unit-confusion defect the type exists to prevent, and would do it in a
/// way that looks correct on the page. Construction goes through
/// [`RobinhoodAtomic::new`] (explicitly "I already have 18-decimal
/// units"), [`RobinhoodAtomic::from_canonical`], or
/// [`RobinhoodAtomic::try_from_u256`]; readout goes through
/// [`RobinhoodAtomic::get`].
///
/// Derives only the traits that cannot lose or reinterpret value: no
/// `Default` (an implicit zero amount is never what accounting wants),
/// no `From`/`Into` against any integer type, no arithmetic operator
/// impls — the checked helpers below are the only arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RobinhoodAtomic(u128);

impl RobinhoodAtomic {
    /// Zero, in Robinhood atomic units.
    pub const ZERO: RobinhoodAtomic = RobinhoodAtomic(0);

    /// Wraps a value that is ALREADY in Robinhood 18-decimal atomic
    /// units. Every call site must be able to justify that claim from a
    /// chain read or an ABI decode; a canonical `u64` is not one, and
    /// must go through [`RobinhoodAtomic::from_canonical`].
    pub const fn new(robinhood_atomic: u128) -> RobinhoodAtomic {
        RobinhoodAtomic(robinhood_atomic)
    }

    /// The underlying 18-decimal atomic value. Use only where a raw
    /// integer is genuinely required (ABI encoding, logging, tests) —
    /// never as a step towards canonical accounting, which is what
    /// [`RobinhoodAtomic::to_canonical`] is for.
    pub const fn get(self) -> u128 {
        self.0
    }

    /// Widens a canonical 8-decimal amount into Robinhood's 18-decimal
    /// unit. Always exact; cannot overflow for any `u64` input at the
    /// current decimals (module docs). The single implementation —
    /// [`CanonicalAtomic::to_robinhood`] is a typed wrapper around this,
    /// not a second copy, following the parent module's
    /// `CanonicalAtomic::to_solana` precedent.
    pub fn from_canonical(
        canonical: CanonicalAtomic,
    ) -> Result<RobinhoodAtomic, RobinhoodConversionError> {
        u128::from(canonical.0)
            .checked_mul(CANONICAL_TO_ROBINHOOD_SCALE)
            .map(RobinhoodAtomic)
            .ok_or(RobinhoodConversionError::WideningOverflow {
                canonical: canonical.0,
                scale_exponent: ROBINHOOD_DECIMALS - GOLDCOIN_DECIMALS,
            })
    }

    /// Narrows to the ledger's canonical 8-decimal unit. Exact only when
    /// the amount is a whole multiple of
    /// [`CANONICAL_TO_ROBINHOOD_SCALE`] AND the quotient fits `u64`;
    /// otherwise rejected. Never rounds, never truncates.
    pub fn to_canonical(self) -> Result<CanonicalAtomic, RobinhoodConversionError> {
        let remainder = self.0 % CANONICAL_TO_ROBINHOOD_SCALE;
        if remainder != 0 {
            return Err(RobinhoodConversionError::NotExactlyRepresentable {
                robinhood: self.0,
                remainder,
            });
        }
        let canonical = self.0 / CANONICAL_TO_ROBINHOOD_SCALE;
        u64::try_from(canonical).map(CanonicalAtomic).map_err(|_| {
            RobinhoodConversionError::CanonicalOverflow {
                robinhood: self.0,
                canonical,
            }
        })
    }

    /// Decodes an inbound 256-bit ABI word. Rejects anything above
    /// `u128::MAX` rather than truncating to the low 128 bits.
    pub fn try_from_u256(word: EvmU256) -> Result<RobinhoodAtomic, RobinhoodConversionError> {
        word.try_to_u128().map(RobinhoodAtomic)
    }

    /// Widens to a 256-bit ABI word for outbound encoding. Lossless and
    /// infallible by construction — every `u128` is a `u256`.
    pub const fn to_u256(self) -> EvmU256 {
        EvmU256::from_u128(self.0)
    }

    /// Checked addition within the Robinhood unit. Mirrors
    /// `CanonicalAtomic::checked_add`; there is deliberately no `Add`
    /// impl, so every accumulation is visibly fallible.
    pub fn checked_add(
        self,
        rhs: RobinhoodAtomic,
    ) -> Result<RobinhoodAtomic, RobinhoodConversionError> {
        self.0.checked_add(rhs.0).map(RobinhoodAtomic).ok_or(
            RobinhoodConversionError::ArithmeticOverflow {
                lhs: self.0,
                op: "+",
                rhs: rhs.0,
            },
        )
    }

    /// Checked subtraction within the Robinhood unit.
    pub fn checked_sub(
        self,
        rhs: RobinhoodAtomic,
    ) -> Result<RobinhoodAtomic, RobinhoodConversionError> {
        self.0.checked_sub(rhs.0).map(RobinhoodAtomic).ok_or(
            RobinhoodConversionError::ArithmeticOverflow {
                lhs: self.0,
                op: "-",
                rhs: rhs.0,
            },
        )
    }
}

impl fmt::Display for RobinhoodAtomic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl CanonicalAtomic {
    /// Converts to Robinhood Chain's 18-decimal atomic unit. Delegates to
    /// [`RobinhoodAtomic::from_canonical`] — the one, canonical widening
    /// implementation; this is a typed wrapper around it, not a second
    /// one, exactly as [`CanonicalAtomic::to_solana`] wraps
    /// `goldcoin_to_solana_atomic`.
    pub fn to_robinhood(self) -> Result<RobinhoodAtomic, RobinhoodConversionError> {
        RobinhoodAtomic::from_canonical(self)
    }
}

/// A 256-bit unsigned EVM ABI word, stored big-endian — the byte order
/// the ABI, `eth_getLogs` topics/data, and every EVM library already
/// agree on.
///
/// Intentionally inert: no arithmetic, no `From<u64>`-style implicit
/// widening from canonical units, no parsing. It exists to carry an ABI
/// word across the boundary and be checked, and nothing else. See the
/// module docs for why the bridge does not want a general-purpose
/// big-integer type in its accounting.
///
/// `PartialOrd`/`Ord` derive over a big-endian byte array and so order
/// numerically, which [`tests`] pins down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvmU256([u8; 32]);

impl EvmU256 {
    /// Zero.
    pub const ZERO: EvmU256 = EvmU256([0u8; 32]);

    /// Wraps 32 big-endian bytes as they came off the wire.
    pub const fn from_be_bytes(bytes: [u8; 32]) -> EvmU256 {
        EvmU256(bytes)
    }

    /// The 32 big-endian bytes, for ABI encoding.
    pub const fn to_be_bytes(self) -> [u8; 32] {
        self.0
    }

    /// Lossless widening of a `u128`. Infallible by construction.
    pub const fn from_u128(value: u128) -> EvmU256 {
        let low = value.to_be_bytes();
        let mut bytes = [0u8; 32];
        let mut i = 0;
        // Right-align the 16-byte value in the 32-byte word; the high
        // 16 bytes stay zero. A `while` loop rather than iterators
        // because this is a `const fn`.
        while i < 16 {
            bytes[16 + i] = low[i];
            i += 1;
        }
        EvmU256(bytes)
    }

    /// Narrows to a `u128`, rejecting any word whose high 128 bits are
    /// set. Never truncates and never casts.
    pub fn try_to_u128(self) -> Result<u128, RobinhoodConversionError> {
        let (high, low) = self.0.split_at(16);
        if high.iter().any(|byte| *byte != 0) {
            return Err(RobinhoodConversionError::U256ExceedsU128 { value: self });
        }
        let mut buf = [0u8; 16];
        buf.copy_from_slice(low);
        Ok(u128::from_be_bytes(buf))
    }
}

impl fmt::Display for EvmU256 {
    /// Full-width `0x`-prefixed hex — all 32 bytes, never abbreviated, so
    /// an error message identifies the offending word unambiguously.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("0x")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
