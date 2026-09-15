//! The two wide-integer operations the live bridge rate needs and `u128`
//! cannot do on its own: a full 128×128→256-bit product, and a 256÷128-bit
//! division. Both are plain schoolbook arithmetic, exact, and deterministic
//! — no floating point, no external crate. Used by the band computation
//! (`|a·d − b·c| · 10 000 / (b·c)` over four `u64` prices) and by the
//! Uniswap v4 price derivation (`ETH_usd_e12 · 2^96 / sqrtPriceX96`,
//! twice).

/// A 256-bit unsigned integer as two `u128` limbs, most significant first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct U256 {
    pub hi: u128,
    pub lo: u128,
}

impl U256 {
    pub const ZERO: U256 = U256 { hi: 0, lo: 0 };

    pub const fn from_u128(lo: u128) -> U256 {
        U256 { hi: 0, lo }
    }

    /// The full product of two `u128`s — never overflows.
    pub fn mul_u128(a: u128, b: u128) -> U256 {
        let (a_hi, a_lo) = (a >> 64, a & u128::from(u64::MAX));
        let (b_hi, b_lo) = (b >> 64, b & u128::from(u64::MAX));
        let ll = a_lo * b_lo;
        let lh = a_lo * b_hi;
        let hl = a_hi * b_lo;
        let hh = a_hi * b_hi;
        // Sum the cross terms in a wider accumulator to catch every carry.
        let mid = (ll >> 64) + (lh & u128::from(u64::MAX)) + (hl & u128::from(u64::MAX));
        let lo = (mid << 64) | (ll & u128::from(u64::MAX));
        let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
        U256 { hi, lo }
    }

    /// `self << shift` for `shift < 256`; the caller guarantees no bits
    /// are shifted out (checked in debug builds).
    pub fn shift_left(self, shift: u32) -> U256 {
        if shift == 0 {
            return self;
        }
        if shift >= 128 {
            debug_assert_eq!(self.hi, 0);
            debug_assert!(shift == 128 || self.lo >> (256 - shift) == 0);
            return U256 {
                hi: self.lo << (shift - 128),
                lo: 0,
            };
        }
        debug_assert_eq!(self.hi >> (128 - shift), 0);
        U256 {
            hi: (self.hi << shift) | (self.lo >> (128 - shift)),
            lo: self.lo << shift,
        }
    }

    fn bit(&self, i: u32) -> bool {
        if i >= 128 {
            (self.hi >> (i - 128)) & 1 == 1
        } else {
            (self.lo >> i) & 1 == 1
        }
    }

    /// `floor(self / divisor)` when the quotient fits a `u128`, else
    /// `None`. `divisor` must be nonzero (checked).
    pub fn div_u128(self, divisor: u128) -> Option<u128> {
        if divisor == 0 {
            return None;
        }
        // Binary long division: the remainder never exceeds the divisor,
        // so it fits a u128 with one guard bit handled by the compare.
        let mut remainder: u128 = 0;
        let mut quotient: u128 = 0;
        for i in (0..256).rev() {
            // remainder = remainder * 2 + bit; a remainder of >= 2^127
            // doubling would overflow, but remainder < divisor <= 2^128-1
            // so remainder*2 + 1 < 2^129: detect that top bit explicitly.
            let carry = remainder >> 127;
            remainder = (remainder << 1) | u128::from(self.bit(i));
            let fits = carry == 1 || remainder >= divisor;
            if fits {
                remainder = remainder.wrapping_sub(divisor);
                if i >= 128 {
                    // A quotient bit at or above 128 does not fit.
                    return None;
                }
                quotient |= 1u128 << i;
            }
        }
        Some(quotient)
    }
}

/// `floor(a * b / d)` over `u128`s, exact, or `None` if the result does
/// not fit a `u128` (or `d == 0`).
pub fn mul_div(a: u128, b: u128, d: u128) -> Option<u128> {
    U256::mul_u128(a, b).div_u128(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_product_of_two_u128s_is_exact() {
        assert_eq!(U256::mul_u128(0, u128::MAX), U256::ZERO);
        assert_eq!(U256::mul_u128(1, 7), U256::from_u128(7));
        // (2^128 - 1)^2 = 2^256 - 2^129 + 1
        assert_eq!(
            U256::mul_u128(u128::MAX, u128::MAX),
            U256 {
                hi: u128::MAX - 1,
                lo: 1
            }
        );
        assert_eq!(
            U256::mul_u128(1 << 100, 1 << 100),
            U256 { hi: 1 << 72, lo: 0 }
        );
    }

    #[test]
    fn division_is_exact_and_refuses_a_quotient_past_u128() {
        assert_eq!(U256::mul_u128(u128::MAX, 3).div_u128(3), Some(u128::MAX));
        assert_eq!(U256::mul_u128(u128::MAX, 3).div_u128(2), None);
        assert_eq!(U256::from_u128(100).div_u128(0), None);
        assert_eq!(mul_div(10, 10, 3), Some(33));
        assert_eq!(mul_div(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        assert_eq!(mul_div(1 << 120, 1 << 120, 1 << 113), Some(1 << 127));
        assert_eq!(mul_div(1 << 120, 1 << 120, 1 << 112), None);
    }

    #[test]
    fn shift_moves_bits_across_the_limb_boundary() {
        assert_eq!(U256::from_u128(1).shift_left(128), U256 { hi: 1, lo: 0 });
        assert_eq!(
            U256::from_u128(1).shift_left(200),
            U256 { hi: 1 << 72, lo: 0 }
        );
        assert_eq!(
            U256::from_u128(3).shift_left(127),
            U256 {
                hi: 1,
                lo: 1 << 127
            }
        );
        assert_eq!(U256::from_u128(5).shift_left(0), U256::from_u128(5));
    }

    #[test]
    fn pseudo_random_cross_check_against_u128_arithmetic() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..5_000 {
            let a = u128::from(next());
            let b = u128::from(next());
            let d = u128::from(next() | 1);
            // Products of two u64s fit u128 exactly: compare directly.
            assert_eq!(U256::mul_u128(a, b), U256::from_u128(a * b));
            assert_eq!(mul_div(a, b, d), Some(a * b / d));
        }
    }
}
