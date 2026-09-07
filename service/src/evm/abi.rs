//! The exact subset of Solidity ABI encoding/decoding this bridge needs
//! to call `GlcRobinhoodBridge`, and nothing more.
//!
//! # Scope, stated as a list
//!
//! Encoding: a 4-byte selector, `uint8`/`uint64`/`uint256`/`address`/
//! `bytes32` static words, one dynamic `bytes` argument, and one dynamic
//! `bytes[]` argument (the signature array). Those five shapes cover
//! `executePayout`, `executeRefund` and `executeSettlement` completely.
//!
//! Decoding: a single static word returned by an `eth_call`, plus the
//! four-word `Obligation` struct. Nothing decodes a dynamic return type,
//! because no view this service calls has one.
//!
//! There is no general-purpose encoder here and there must not be one. A
//! general encoder needs a type model, a parser for signature strings and
//! a tuple/array recursion, all of which would be code in the trust path
//! of a fund-moving call that no test in this repository exercises. Each
//! call site below builds its own calldata explicitly, and the resulting
//! bytes are asserted against fixtures.
//!
//! # The head/tail split, in one paragraph
//!
//! Solidity encodes a call's arguments as a HEAD of one 32-byte word per
//! argument followed by a TAIL. A static argument's word is its value. A
//! dynamic argument's word is a byte OFFSET, measured from the start of
//! the argument block (i.e. after the selector), to where its data sits in
//! the tail. Getting that origin wrong — measuring from the start of the
//! calldata including the selector — is the classic mistake, and it
//! produces calldata that decodes to garbage rather than failing loudly.
//! [`Calldata`] therefore never lets a caller compute an offset by hand.

use super::address::EvmAddress;
use super::keccak::keccak256;
use super::u256::EvmU256;

/// One 32-byte ABI word.
pub type Word = [u8; 32];

/// The first four bytes of `keccak256` of a canonical function
/// signature — e.g. `"executeSettlement((bytes32,uint256,uint64,uint64),bytes[])"`.
///
/// The signature string must use canonical type names with no spaces and
/// no parameter names, and must spell a struct argument as the
/// parenthesised tuple of its field types in declaration order. A
/// signature that differs by one character selects a different (almost
/// certainly nonexistent) function, so every selector this crate uses is
/// pinned by a test.
pub fn selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature.as_bytes());
    [hash[0], hash[1], hash[2], hash[3]]
}

/// A `uint256` word.
pub fn word_u256(value: EvmU256) -> Word {
    value.to_be_bytes()
}

/// A `uint8`/`uint64`/`uint128` word: the value right-aligned, zero-padded.
pub fn word_u128(value: u128) -> Word {
    EvmU256::from_u128(value).to_be_bytes()
}

/// An `address` word: 20 bytes right-aligned behind 12 zero bytes.
pub fn word_address(value: EvmAddress) -> Word {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(value.as_bytes());
    word
}

/// A `bytes32` word: the value verbatim.
pub fn word_bytes32(value: [u8; 32]) -> Word {
    value
}

/// A `bool` word.
pub fn word_bool(value: bool) -> Word {
    let mut word = [0u8; 32];
    word[31] = u8::from(value);
    word
}

/// One dynamic argument, held until the head is complete so its offset
/// can be computed rather than guessed.
enum Dynamic {
    /// A `bytes` value: length word, then the payload right-padded to a
    /// multiple of 32.
    Bytes(Vec<u8>),
    /// A `bytes[]` value: a count word, then one offset word per element
    /// (relative to the start of THIS array's own data, not the call's),
    /// then each element encoded as a `bytes`.
    BytesArray(Vec<Vec<u8>>),
}

impl Dynamic {
    fn encode(&self) -> Vec<u8> {
        match self {
            Dynamic::Bytes(bytes) => encode_bytes_value(bytes),
            Dynamic::BytesArray(items) => {
                let mut heads: Vec<Word> = Vec::with_capacity(items.len());
                let mut tail: Vec<u8> = Vec::new();
                // Every element's offset is measured from the first word
                // AFTER the count — i.e. from the start of the offset
                // table itself.
                let mut cursor = items.len() * 32;
                for item in items {
                    heads.push(word_u128(cursor as u128));
                    let encoded = encode_bytes_value(item);
                    cursor += encoded.len();
                    tail.extend_from_slice(&encoded);
                }
                let mut out = Vec::with_capacity(32 + heads.len() * 32 + tail.len());
                out.extend_from_slice(&word_u128(items.len() as u128));
                for head in heads {
                    out.extend_from_slice(&head);
                }
                out.extend_from_slice(&tail);
                out
            }
        }
    }
}

/// A `bytes` value: its length, then its bytes right-padded to a 32-byte
/// boundary. An empty value is one zero word and no padding.
fn encode_bytes_value(bytes: &[u8]) -> Vec<u8> {
    let padded = bytes.len().div_ceil(32) * 32;
    let mut out = Vec::with_capacity(32 + padded);
    out.extend_from_slice(&word_u128(bytes.len() as u128));
    out.extend_from_slice(bytes);
    out.resize(32 + padded, 0);
    out
}

/// An ABI call under construction: a selector, a head of static words and
/// dynamic placeholders, and the tail those placeholders point into.
///
/// Arguments are pushed in declaration order and the offsets are computed
/// at [`Calldata::finish`], when the head's final length is known. A
/// caller therefore cannot write an offset at all, correctly or otherwise.
///
/// A struct argument that contains no dynamic field is encoded INLINE, as
/// its fields in order — which is what Solidity does for a `calldata`
/// struct of static fields, and is why `executeSettlement`'s
/// `SettlementRequest` contributes four head words rather than an offset.
pub struct Calldata {
    selector: [u8; 4],
    /// `None` marks a dynamic argument whose offset is filled in at
    /// `finish`, in the order the dynamics were pushed.
    head: Vec<Option<Word>>,
    dynamics: Vec<Dynamic>,
}

impl Calldata {
    pub fn new(signature: &str) -> Calldata {
        Calldata {
            selector: selector(signature),
            head: Vec::new(),
            dynamics: Vec::new(),
        }
    }

    /// Appends one already-encoded static word.
    pub fn word(mut self, word: Word) -> Calldata {
        self.head.push(Some(word));
        self
    }

    /// Appends a dynamic `bytes[]` argument.
    pub fn bytes_array(mut self, items: Vec<Vec<u8>>) -> Calldata {
        self.head.push(None);
        self.dynamics.push(Dynamic::BytesArray(items));
        self
    }

    /// Appends a dynamic `bytes` argument.
    pub fn bytes(mut self, value: Vec<u8>) -> Calldata {
        self.head.push(None);
        self.dynamics.push(Dynamic::Bytes(value));
        self
    }

    /// Resolves every dynamic offset and produces the final calldata.
    pub fn finish(self) -> Vec<u8> {
        let head_len = self.head.len() * 32;
        let mut encoded_dynamics: Vec<Vec<u8>> = Vec::with_capacity(self.dynamics.len());
        let mut offsets: Vec<Word> = Vec::with_capacity(self.dynamics.len());
        // Offsets are measured from the start of the ARGUMENT block — the
        // byte immediately after the selector — never from the start of
        // the calldata.
        let mut cursor = head_len;
        for dynamic in &self.dynamics {
            offsets.push(word_u128(cursor as u128));
            let encoded = dynamic.encode();
            cursor += encoded.len();
            encoded_dynamics.push(encoded);
        }

        let mut out = Vec::with_capacity(4 + cursor);
        out.extend_from_slice(&self.selector);
        let mut next_offset = 0usize;
        for slot in &self.head {
            match slot {
                Some(word) => out.extend_from_slice(word),
                None => {
                    out.extend_from_slice(&offsets[next_offset]);
                    next_offset += 1;
                }
            }
        }
        for encoded in encoded_dynamics {
            out.extend_from_slice(&encoded);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AbiDecodeError {
    #[error("expected {expected} return word(s) ({} bytes), got {actual} bytes", expected * 32)]
    WrongLength { expected: usize, actual: usize },
    #[error("`{field}` return word {word} is not a valid {kind}")]
    NotA {
        field: &'static str,
        word: usize,
        kind: &'static str,
    },
}

/// Splits an `eth_call` return into exactly `N` words, refusing any other
/// length.
///
/// A short return is the signature of a call that hit a non-contract
/// address (which returns empty rather than reverting), and a long one
/// means the ABI this code assumes is not the ABI that is deployed.
/// Neither is a value to work with.
pub fn return_words<const N: usize>(data: &[u8]) -> Result<[Word; N], AbiDecodeError> {
    if data.len() != N * 32 {
        return Err(AbiDecodeError::WrongLength {
            expected: N,
            actual: data.len(),
        });
    }
    let mut words = [[0u8; 32]; N];
    for (i, word) in words.iter_mut().enumerate() {
        word.copy_from_slice(&data[i * 32..(i + 1) * 32]);
    }
    Ok(words)
}

/// Decodes one returned `address` word, requiring the 12 high bytes to be
/// zero.
///
/// Not tolerance for tolerance's sake: a non-zero high byte means the
/// word is not an address, and silently masking it off would turn a
/// wrong-ABI read into a plausible-looking contract address — exactly the
/// value the token-verification preflight exists to check.
pub fn decode_address(word: &Word, field: &'static str) -> Result<EvmAddress, AbiDecodeError> {
    if word[..12].iter().any(|b| *b != 0) {
        return Err(AbiDecodeError::NotA {
            field,
            word: 0,
            kind: "address (high bytes are not zero)",
        });
    }
    let mut bytes = [0u8; 20];
    bytes.copy_from_slice(&word[12..]);
    Ok(EvmAddress::from_bytes(bytes))
}

/// Decodes one returned `bool` word, requiring it to be exactly 0 or 1.
pub fn decode_bool(word: &Word, field: &'static str) -> Result<bool, AbiDecodeError> {
    if word[..31].iter().any(|b| *b != 0) || word[31] > 1 {
        return Err(AbiDecodeError::NotA {
            field,
            word: 0,
            kind: "bool (not 0 or 1)",
        });
    }
    Ok(word[31] == 1)
}

/// Decodes one returned word as a `u64`, refusing anything wider.
pub fn decode_u64(word: &Word, field: &'static str) -> Result<u64, AbiDecodeError> {
    EvmU256::from_be_bytes(*word)
        .try_to_u64()
        .map_err(|_| AbiDecodeError::NotA {
            field,
            word: 0,
            kind: "uint64 (value exceeds 2^64-1)",
        })
}

/// Decodes one returned word as a `u8`, refusing anything wider — the
/// shape an `enum` or a `decimals()` return has.
pub fn decode_u8(word: &Word, field: &'static str) -> Result<u8, AbiDecodeError> {
    if word[..31].iter().any(|b| *b != 0) {
        return Err(AbiDecodeError::NotA {
            field,
            word: 0,
            kind: "uint8 (value exceeds 255)",
        });
    }
    Ok(word[31])
}

#[cfg(test)]
mod tests;
