//! Strict `0x`-prefixed hex decoding/encoding for EVM values.
//!
//! # Relationship to [`crate::goldcoin::hex`]
//!
//! The byte-level codec is **not** duplicated here: the digit-pair
//! decoding is delegated to [`crate::goldcoin::hex::decode_exact`], whose
//! own module docs already describe it as pure and
//! chain-mechanics-agnostic, and whose lowercase [`encode`] output is
//! exactly the canonical form every type in [`crate::evm`] displays. What
//! this module adds is the part that is genuinely EVM-specific and that
//! the Goldcoin codec deliberately does not have:
//!
//! - The `0x` prefix, which is **mandatory** on every EVM hex string and
//!   is absent from Goldcoin's bare-hex RPC fields.
//! - A length check that runs **before** the digit check, so a wrong-width
//!   value reports the width it should have had rather than "odd number of
//!   digits".
//!
//! (`goldcoin::hex` living under the Goldcoin module while being generic
//! is a pre-existing wrinkle. Promoting it to a crate-level `hex` module
//! would be a sensible follow-up; it is not done here so that this phase's
//! diff stays purely additive.)
//!
//! [`encode`]: crate::goldcoin::hex::encode
//!
//! # Why exactly `0x`, and never `0X`
//!
//! Accepting more than one spelling of the prefix means the same value has
//! more than one canonical form, and every downstream comparison — a
//! config value against a chain read, a dedup key against a stored row —
//! then depends on which spelling happened to arrive. Every EVM tool emits
//! lowercase `0x`; requiring it costs nothing real and removes the
//! question entirely.
//!
//! Hex **digits** are a different matter: case carries no information for
//! a hash, so any case is accepted on input and lowercase is what is
//! emitted. The one exception is an address, where mixed case is EIP-55
//! checksum data — see [`crate::evm::address`].

use crate::goldcoin::hex::{self as bare_hex, HexError};

/// The mandatory prefix on every EVM hex string.
pub const PREFIX: &str = "0x";

/// Why a `0x`-prefixed hex string could not be decoded.
///
/// Carried by the per-type error enums in this module's siblings rather
/// than returned to callers directly, so that an error message names the
/// kind of value that failed ("invalid EVM address: ...") as well as how.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmHexError {
    /// No `0x` prefix, or a prefix in some other spelling such as `0X`.
    #[error("missing the mandatory \"0x\" prefix (found {found:?})")]
    MissingPrefix { found: String },
    /// The prefix was present but the digit count is not the exact count
    /// this value's fixed width requires.
    #[error("expected exactly {expected} hex digit(s) after \"0x\", got {actual}")]
    WrongLength { expected: usize, actual: usize },
    /// A variable-length byte string whose digit count is not even, so it
    /// does not describe whole bytes.
    #[error("hex body has an odd digit count ({0}), so it does not describe whole bytes")]
    OddLength(usize),
    /// A character that is not an ASCII hex digit, with its zero-based
    /// position **within the body** (i.e. after the `0x`).
    #[error("{found:?} at body position {position} is not an ASCII hex digit")]
    InvalidDigit { position: usize, found: char },
}

/// Returns the body after a mandatory, exactly-`0x` prefix.
///
/// The prefix check is `starts_with`, not a case-insensitive compare: see
/// the module docs on why `0X` is rejected. The truncation in the error is
/// only so a pathological input cannot put an unbounded string into a log
/// line.
pub fn strip_prefix(s: &str) -> Result<&str, EvmHexError> {
    s.strip_prefix(PREFIX)
        .ok_or_else(|| EvmHexError::MissingPrefix {
            found: s.chars().take(8).collect(),
        })
}

/// The first non-hex-digit in `body`, as `(zero-based position, char)`.
///
/// Positions are counted in `char`s, which equal bytes for every input
/// that could have been valid; a multi-byte character is itself the
/// failure being reported, so its position is the position of the first
/// offending `char`.
pub fn find_non_hex(body: &str) -> Option<(usize, char)> {
    body.char_indices().find(|(_, c)| !c.is_ascii_hexdigit())
}

/// Decodes exactly `N` bytes from a `0x`-prefixed string of exactly `2 * N`
/// hex digits. Any other digit count is an error — never truncated, never
/// zero-padded, in either direction.
///
/// Accepts upper-, lower- and mixed-case digits: at this layer case is not
/// meaningful. A type for which it *is* meaningful (an address) checks the
/// case pattern itself, on the original string, before calling this.
pub fn decode_fixed<const N: usize>(s: &str) -> Result<[u8; N], EvmHexError> {
    let body = strip_prefix(s)?;
    // Length first, so a 39-digit address reports "expected 40, got 39"
    // rather than the less actionable "odd digit count".
    if body.len() != N * 2 {
        return Err(EvmHexError::WrongLength {
            expected: N * 2,
            actual: body.len(),
        });
    }
    bare_hex::decode_exact::<N>(body).map_err(|err| map_bare(body, err))
}

/// Decodes a `0x`-prefixed, even-digit-count hex string of any length,
/// including the empty body `"0x"` (which is how Ethereum JSON-RPC spells
/// zero-length `DATA`).
pub fn decode_var(s: &str) -> Result<Vec<u8>, EvmHexError> {
    let body = strip_prefix(s)?;
    if !body.len().is_multiple_of(2) {
        return Err(EvmHexError::OddLength(body.len()));
    }
    bare_hex::decode_vec(body).map_err(|err| map_bare(body, err))
}

/// `"0x"` followed by lowercase hex digits, two per byte. The canonical
/// output form for every fixed-width type in [`crate::evm`].
pub fn encode_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(PREFIX.len() + bytes.len() * 2);
    out.push_str(PREFIX);
    out.push_str(&bare_hex::encode(bytes));
    out
}

/// Translates the delegated codec's error into this module's, re-deriving
/// the offending character's position (which the bare codec reports as the
/// digit pair, not the index).
///
/// The length variants are already excluded by the callers' own checks;
/// they are mapped rather than asserted so that a future change to the
/// delegated codec surfaces as an error instead of a panic.
fn map_bare(body: &str, err: HexError) -> EvmHexError {
    match err {
        HexError::InvalidDigit(_) => match find_non_hex(body) {
            Some((position, found)) => EvmHexError::InvalidDigit { position, found },
            // Unreachable: the bare codec only reports InvalidDigit for a
            // pair it could not parse, which is a pair containing a
            // non-hex digit.
            None => EvmHexError::OddLength(body.len()),
        },
        HexError::OddLength(n) => EvmHexError::OddLength(n),
        HexError::WrongLength {
            expected, actual, ..
        } => EvmHexError::WrongLength { expected, actual },
    }
}

#[cfg(test)]
mod tests;
