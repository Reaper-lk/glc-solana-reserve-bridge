//! Exact decimal-text → fixed-point price parsing (docs/38-elastic-bridge-
//! rate.md, Phase 2B).
//!
//! Every price feed hands this bridge a NUMBER AS TEXT — a JSON string
//! (`"0.000156705"`) or the raw token of a JSON number (`0.0000443670077`,
//! `4.4367e-5`). Going through `f64` would make the price depend on the
//! platform's float parser and formatter; going through this module makes
//! it depend on nothing but the digits. The text is walked once, the
//! integer and fractional digits are accumulated into a `u128`, the
//! exponent (if any) shifts the decimal point, and the result is scaled to
//! [`PRICE_SCALE`] by TRUNCATION — never rounding — so two processes given
//! the same text always agree, and a price never rounds up to more than
//! the market printed.
//!
//! Refused, never coerced: empty text, a sign (a price is never negative,
//! and `+` is noise a feed has no business emitting), anything but digits
//! / one `.` / one exponent, a zero price, a price that does not fit `u64`
//! at twelve decimals (a per-unit price above ~18 million dollars is not a
//! price this bridge will ever see), and more than 38 significant digits
//! (past `u128`).

/// Why a price text was refused. Carries the offending text so a feed
/// failure is diagnosable from the log line alone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PriceParseError {
    #[error("price text {0:?} is empty")]
    Empty(String),
    #[error("price text {0:?} is not a plain non-negative decimal number")]
    NotNumeric(String),
    #[error("price text {0:?} is zero — a zero price is not a rate")]
    Zero(String),
    #[error("price text {0:?} does not fit a u64 at 12 decimals")]
    Overflow(String),
}

/// Parses `text` into a price scaled by [`PRICE_SCALE`], truncating
/// anything beyond the twelfth decimal place.
pub fn parse_price_e12(text: &str) -> Result<u64, PriceParseError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PriceParseError::Empty(text.to_string()));
    }
    let bad = || PriceParseError::NotNumeric(text.to_string());
    // Split off an exponent, if any.
    let (mantissa, exponent): (&str, i32) = match trimmed.find(['e', 'E']) {
        None => (trimmed, 0),
        Some(i) => {
            let (m, e) = trimmed.split_at(i);
            let e = &e[1..];
            let (neg, digits) = match e.strip_prefix('-') {
                Some(d) => (true, d),
                None => (false, e.strip_prefix('+').unwrap_or(e)),
            };
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 4
            {
                return Err(bad());
            }
            let value: i32 = digits.parse().map_err(|_| bad())?;
            (m, if neg { -value } else { value })
        }
    };
    let (int_part, frac_part) = match mantissa.find('.') {
        None => (mantissa, ""),
        Some(i) => (&mantissa[..i], &mantissa[i + 1..]),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(bad());
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }
    // Accumulate every digit into one integer, remembering how many of
    // them sit right of the decimal point; the exponent then moves the
    // point. Leading zeros are skipped so a long fraction of zeros does
    // not count against the 38-digit budget.
    let mut acc: u128 = 0;
    let mut significant = 0usize;
    let mut scale: i32 = 0; // acc represents value * 10^scale
    for (is_frac, b) in int_part
        .bytes()
        .map(|b| (false, b))
        .chain(frac_part.bytes().map(|b| (true, b)))
    {
        let d = u128::from(b - b'0');
        if acc == 0 && d == 0 {
            if is_frac {
                scale -= 1;
            }
            continue;
        }
        significant += 1;
        if significant > 38 {
            return Err(PriceParseError::Overflow(text.to_string()));
        }
        acc = acc * 10 + d;
        if is_frac {
            scale -= 1;
        }
    }
    if acc == 0 {
        return Err(PriceParseError::Zero(text.to_string()));
    }
    // value = acc * 10^(scale + exponent); we want value * 10^12.
    let shift = scale + exponent + 12;
    let scaled: u128 = if shift >= 0 {
        if shift > 38 {
            return Err(PriceParseError::Overflow(text.to_string()));
        }
        acc.checked_mul(10u128.pow(shift as u32))
            .ok_or_else(|| PriceParseError::Overflow(text.to_string()))?
    } else {
        let drop = (-shift) as u32;
        if drop > 38 {
            0
        } else {
            acc / 10u128.pow(drop)
        }
    };
    if scaled == 0 {
        return Err(PriceParseError::Zero(text.to_string()));
    }
    u64::try_from(scaled).map_err(|_| PriceParseError::Overflow(text.to_string()))
}

/// `PRICE_SCALE` re-exported for the tests' readability.
#[cfg(test)]
pub(crate) const SCALE: u64 = super::PRICE_SCALE;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_decimals_parse_exactly_and_truncate_past_twelve_places() {
        assert_eq!(parse_price_e12("1").unwrap(), SCALE);
        assert_eq!(parse_price_e12("1.0").unwrap(), SCALE);
        assert_eq!(parse_price_e12("0.000156705").unwrap(), 156_705_000);
        assert_eq!(parse_price_e12("2515.75").unwrap(), 2_515_750_000_000_000);
        assert_eq!(parse_price_e12("78333.34").unwrap(), 78_333_340_000_000_000);
        // 0.000044367007702948316 -> 0.000044367007 (truncated, never rounded)
        assert_eq!(
            parse_price_e12("0.000044367007702948316").unwrap(),
            44_367_007
        );
        assert_eq!(parse_price_e12("0.0000000000019").unwrap(), 1);
        assert_eq!(parse_price_e12("  0.5  ").unwrap(), 500_000_000_000);
        assert_eq!(parse_price_e12(".5").unwrap(), 500_000_000_000);
        assert_eq!(parse_price_e12("5.").unwrap(), 5 * SCALE);
    }

    #[test]
    fn exponents_move_the_point_without_floating_point() {
        assert_eq!(parse_price_e12("4.4367e-5").unwrap(), 44_367_000);
        assert_eq!(parse_price_e12("4.4367E-05").unwrap(), 44_367_000);
        assert_eq!(parse_price_e12("1e3").unwrap(), 1_000 * SCALE);
        assert_eq!(parse_price_e12("1e+3").unwrap(), 1_000 * SCALE);
        // 123456789e-20 = 1.23456789e-12 -> one unit at 12 places, the rest truncated.
        assert_eq!(parse_price_e12("123456789e-20").unwrap(), 1);
        assert_eq!(parse_price_e12("123456789e-17").unwrap(), 1_234);
    }

    #[test]
    fn everything_that_is_not_a_positive_number_is_refused() {
        for text in [
            "", "   ", "abc", "-1", "+1", "1.2.3", "1e", "1e-", "1,5", "NaN", "inf", "0x10",
            "1e99999",
        ] {
            assert!(parse_price_e12(text).is_err(), "{text:?} must be refused");
        }
        assert!(matches!(
            parse_price_e12("0"),
            Err(PriceParseError::Zero(_))
        ));
        assert!(matches!(
            parse_price_e12("0.000"),
            Err(PriceParseError::Zero(_))
        ));
        // Below one unit at 12 places truncates to zero, which is refused.
        assert!(matches!(
            parse_price_e12("0.0000000000001"),
            Err(PriceParseError::Zero(_))
        ));
        assert!(matches!(
            parse_price_e12("18446744073709551616e-12"),
            Err(PriceParseError::Overflow(_))
        ));
        assert!(matches!(
            parse_price_e12("1e8"),
            Err(PriceParseError::Overflow(_))
        ));
        assert!(matches!(
            parse_price_e12("1234567890123456789012345678901234567890"),
            Err(PriceParseError::Overflow(_))
        ));
    }

    #[test]
    fn the_largest_representable_price_is_the_u64_ceiling() {
        assert_eq!(parse_price_e12("18446744.073709551615").unwrap(), u64::MAX);
        assert!(parse_price_e12("18446744.073709551616").is_err());
    }
}
