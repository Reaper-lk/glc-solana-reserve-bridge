//! Minimal RLP encoding — the exact subset an EVM transaction envelope
//! needs, and nothing else.
//!
//! # Why this exists rather than a crate
//!
//! Every transaction this bridge ever broadcasts is one of two fixed
//! shapes ([`super::tx`]): a nine-item legacy list, or a twelve-item
//! EIP-1559 list. Both are built here, item by item, from values this
//! crate already holds as typed primitives. The encoder therefore needs
//! exactly three constructs — a byte string, an unsigned integer, and a
//! list — and each is a dozen lines. Pulling an RLP crate in would add a
//! dependency to a process whose Robinhood safety story is that its
//! dependency surface is auditable line by line, in exchange for code
//! this module can state in full on one screen.
//!
//! # There is deliberately no decoder
//!
//! Nothing in this bridge parses RLP. Transaction receipts and blocks
//! arrive as JSON from the node, already decoded; the only RLP this
//! service ever handles is RLP it produced itself, moments earlier, in
//! this file. A decoder would be unexercised parsing code sitting in the
//! trust path of a signing pipeline.
//!
//! # The rules, transcribed
//!
//! From the Ethereum Yellow Paper, Appendix B:
//!
//! ```text
//! a single byte in [0x00, 0x7f]      -> itself
//! a string of 0..=55 bytes           -> 0x80 + len, then the bytes
//! a string of >55 bytes              -> 0xb7 + len(len), len big-endian, then the bytes
//! a list whose payload is 0..=55     -> 0xc0 + len, then the payload
//! a list whose payload is >55        -> 0xf7 + len(len), len big-endian, then the payload
//! ```
//!
//! # Integers are minimal big-endian, and zero is the empty string
//!
//! RLP has no integer type: an integer is encoded as its shortest
//! big-endian byte string with no leading zero bytes, which makes zero
//! the EMPTY string (`0x80`). This is not a stylistic choice — a
//! non-minimal encoding produces a different transaction hash and, for a
//! signed transaction, a signature over a payload no node will accept.
//! [`encode_uint`] is the only integer path in this module for exactly
//! that reason.

/// Encodes a byte string.
///
/// Note the single-byte special case: a one-byte string whose value is
/// below `0x80` encodes as ITSELF, with no length prefix. Missing it is
/// the classic RLP bug — it produces a valid-looking encoding that hashes
/// differently from every other implementation's.
pub fn encode_bytes(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        return vec![bytes[0]];
    }
    let mut out = encode_length(bytes.len(), 0x80);
    out.extend_from_slice(bytes);
    out
}

/// Encodes an unsigned integer as RLP's minimal big-endian byte string.
///
/// Zero encodes as the empty string (`0x80`), which is what makes an
/// unset `value`, a zero `gasPrice` or a zero `v` round-trip through
/// every node's decoder.
pub fn encode_uint(value: u64) -> Vec<u8> {
    encode_bytes(&minimal_be_u64(value))
}

/// Encodes a 256-bit word as RLP's minimal big-endian byte string —
/// leading zero bytes stripped, exactly as [`encode_uint`] does for a
/// `u64`.
pub fn encode_u256(value: super::u256::EvmU256) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    encode_bytes(&bytes[first..])
}

/// Wraps already-encoded items as an RLP list.
///
/// Takes the items' ENCODINGS, not their values: an RLP list's payload is
/// the concatenation of its items' encodings, so building the items first
/// and framing them here is the shape the format actually has.
pub fn encode_list(items: &[Vec<u8>]) -> Vec<u8> {
    let payload_len: usize = items.iter().map(Vec::len).sum();
    let mut out = encode_length(payload_len, 0xc0);
    for item in items {
        out.extend_from_slice(item);
    }
    out
}

/// The shortest big-endian representation of `value`, with no leading
/// zero bytes. Zero is the EMPTY slice, not `[0x00]`.
pub fn minimal_be_u64(value: u64) -> Vec<u8> {
    if value == 0 {
        return Vec::new();
    }
    let bytes = value.to_be_bytes();
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    bytes[first..].to_vec()
}

/// The length prefix for a string (`offset` 0x80) or a list (`offset`
/// 0xc0).
fn encode_length(len: usize, offset: u8) -> Vec<u8> {
    if len <= 55 {
        return vec![offset + len as u8];
    }
    let len_bytes = minimal_be_u64(len as u64);
    let mut out = Vec::with_capacity(1 + len_bytes.len());
    out.push(offset + 55 + len_bytes.len() as u8);
    out.extend_from_slice(&len_bytes);
    out
}

#[cfg(test)]
mod tests;
