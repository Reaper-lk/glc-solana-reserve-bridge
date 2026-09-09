//! Operator-facing number formats: percentages and GLC amounts.
//!
//! # Why this is a module and not a `parse::<f64>()` at the call site
//!
//! Both conversions here turn a number a human typed into a number that
//! decides how much money moves. `6` must become exactly 600 basis
//! points and `20000` must become exactly 2,000,000,000,000 canonical
//! atomic units, and "exactly" is the whole requirement: a float
//! round-trip that lands one unit low is a fee the bridge under-charges
//! for as long as nobody notices.
//!
//! So every conversion below is integer arithmetic over the decimal
//! STRING the operator typed. No floating point appears anywhere in this
//! module, and every widening step is checked.
//!
//! # What is accepted
//!
//! A decimal number with an optional fractional part, optionally grouped
//! with `,` or `_` because operators type `10,000,000`. No sign, no
//! exponent, no whitespace inside, no more fractional digits than the
//! destination unit can represent — `0.001%` is not 0 basis points, it is
//! a value this bridge cannot charge, and rounding it silently is exactly
//! the class of thing that must fail loudly instead.

use crate::amount_conversion::{CanonicalAtomic, BPS_DENOMINATOR};

/// Decimal places in one basis point expressed as a percentage: 1 bp =
/// 0.01%, so a percentage may carry at most two fractional digits.
const PERCENT_DECIMALS: u32 = 2;

/// Decimal places in the canonical accounting unit. 1 GLC = 10^8
/// canonical atomic units.
const GLC_DECIMALS: u32 = 8;

/// Why an operator-typed number was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HumanParseError {
    #[error("{what} must not be empty")]
    Empty { what: &'static str },
    #[error(
        "{what} {input:?} is not a decimal number. Digits and at most one `.` only — no sign, no \
         exponent, no units; `,` and `_` are accepted as digit grouping"
    )]
    NotANumber { what: &'static str, input: String },
    #[error(
        "{what} {input:?} is negative. A negative fee or limit has no meaning, and treating the \
         sign as noise would silently accept the opposite of what was typed"
    )]
    Negative { what: &'static str, input: String },
    #[error(
        "{what} {input:?} has more than {max_decimals} decimal place(s), so it cannot be \
         represented exactly. Rounding it would charge or admit an amount nobody typed"
    )]
    TooPrecise {
        what: &'static str,
        input: String,
        max_decimals: u32,
    },
    #[error("{what} {input:?} is too large to represent")]
    Overflow { what: &'static str, input: String },
}

/// Parses an operator-typed percentage into basis points.
///
/// `"6"` -> 600, `"3"` -> 300, `"1.5"` -> 150, `"0.01"` -> 1.
///
/// The range check (`0 < bps < 10000`) is NOT made here: it belongs to
/// [`crate::chain_policy::ChainPolicy::new`], which is the one place that
/// decides what a valid policy is. This function's only job is an exact
/// conversion, and having it also enforce policy would mean two places
/// could disagree about it.
pub fn parse_fee_percent(input: &str) -> Result<u64, HumanParseError> {
    parse_scaled(input, "fee percentage", PERCENT_DECIMALS)
}

/// Parses an operator-typed GLC amount into canonical 8-decimal atomic
/// units.
///
/// `"20000"` -> 2,000,000,000,000. `"0.00000001"` -> 1.
pub fn parse_glc(input: &str) -> Result<CanonicalAtomic, HumanParseError> {
    parse_scaled(input, "GLC amount", GLC_DECIMALS).map(CanonicalAtomic)
}

/// Renders basis points as the percentage an operator would recognise,
/// with no trailing zeros: 600 -> `6%`, 150 -> `1.5%`, 25 -> `0.25%`.
pub fn format_percent(bps: u64) -> String {
    format!("{}%", format_scaled(bps, PERCENT_DECIMALS, false))
}

/// Renders canonical atomic units as a grouped GLC figure: 2,000,000,000,000
/// -> `20,000 GLC`.
pub fn format_glc(canonical: u64) -> String {
    format!("{} GLC", format_scaled(canonical, GLC_DECIMALS, true))
}

/// One decimal parse for both units, so the two can never disagree about
/// what "too precise" or "not a number" means.
fn parse_scaled(input: &str, what: &'static str, decimals: u32) -> Result<u64, HumanParseError> {
    let cleaned: String = input
        .trim()
        .chars()
        .filter(|c| *c != ',' && *c != '_')
        .collect();
    if cleaned.is_empty() {
        return Err(HumanParseError::Empty { what });
    }
    if cleaned.starts_with('-') {
        return Err(HumanParseError::Negative {
            what,
            input: input.to_string(),
        });
    }

    let (whole, fraction) = match cleaned.split_once('.') {
        None => (cleaned.as_str(), ""),
        Some((w, f)) => (w, f),
    };
    // An empty whole part (".5") is accepted; an empty fraction ("5.")
    // is not, because a trailing point is a typo rather than a number.
    let whole = if whole.is_empty() { "0" } else { whole };
    let malformed = fraction.contains('.')
        || (cleaned.contains('.') && fraction.is_empty())
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !fraction.chars().all(|c| c.is_ascii_digit());
    if malformed {
        return Err(HumanParseError::NotANumber {
            what,
            input: input.to_string(),
        });
    }
    if fraction.len() as u32 > decimals {
        return Err(HumanParseError::TooPrecise {
            what,
            input: input.to_string(),
            max_decimals: decimals,
        });
    }

    let overflow = || HumanParseError::Overflow {
        what,
        input: input.to_string(),
    };
    let scale = 10u64.checked_pow(decimals).ok_or_else(overflow)?;
    let whole: u64 = whole.parse().map_err(|_| overflow())?;
    // Right-pad the fraction to the unit's full precision so "1.5" at two
    // decimals is 50 hundredths, not 5.
    let mut padded = fraction.to_string();
    while (padded.len() as u32) < decimals {
        padded.push('0');
    }
    let fraction: u64 = if padded.is_empty() {
        0
    } else {
        padded.parse().map_err(|_| overflow())?
    };

    whole
        .checked_mul(scale)
        .and_then(|w| w.checked_add(fraction))
        .ok_or_else(overflow)
}

/// Renders a fixed-point integer back to a decimal string, trimming
/// trailing fractional zeros and optionally grouping the whole part.
fn format_scaled(value: u64, decimals: u32, group: bool) -> String {
    let scale = 10u64.pow(decimals);
    let whole = value / scale;
    let fraction = value % scale;

    let whole = if group {
        group_digits(whole)
    } else {
        whole.to_string()
    };
    if fraction == 0 {
        return whole;
    }
    let mut frac = format!("{fraction:0width$}", width = decimals as usize);
    while frac.ends_with('0') {
        frac.pop();
    }
    format!("{whole}.{frac}")
}

/// `1234567` -> `1,234,567`. Display only — never parsed back.
fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The percentage denominator, re-exported so a caller formatting a
/// policy does not have to reach into `amount_conversion` for it.
pub const BPS_PER_HUNDRED_PERCENT: u64 = BPS_DENOMINATOR;

#[cfg(test)]
mod tests;
