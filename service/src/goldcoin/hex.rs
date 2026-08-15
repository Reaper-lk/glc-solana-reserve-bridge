//! Minimal hex codec (docs/01-reuse-inventory.md: reused unchanged from the
//! old bridge's `glc::hex` — pure, chain-mechanics-agnostic).

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HexError {
    #[error("hex string has odd length {0}")]
    OddLength(usize),
    #[error("expected {expected} hex chars ({expected_bytes} bytes), got {actual}")]
    WrongLength {
        expected: usize,
        expected_bytes: usize,
        actual: usize,
    },
    #[error("invalid hex digit in {0:?}")]
    InvalidDigit(String),
}

pub fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn decode_exact<const N: usize>(s: &str) -> Result<[u8; N], HexError> {
    if !s.len().is_multiple_of(2) {
        return Err(HexError::OddLength(s.len()));
    }
    if s.len() != N * 2 {
        return Err(HexError::WrongLength {
            expected: N * 2,
            expected_bytes: N,
            actual: s.len(),
        });
    }
    let mut out = [0u8; N];
    for i in 0..N {
        let byte_str = &s[i * 2..i * 2 + 2];
        out[i] = u8::from_str_radix(byte_str, 16)
            .map_err(|_| HexError::InvalidDigit(byte_str.to_string()))?;
    }
    Ok(out)
}

pub fn decode_vec(s: &str) -> Result<Vec<u8>, HexError> {
    if !s.len().is_multiple_of(2) {
        return Err(HexError::OddLength(s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for i in (0..s.len()).step_by(2) {
        let byte_str = &s[i..i + 2];
        out.push(
            u8::from_str_radix(byte_str, 16)
                .map_err(|_| HexError::InvalidDigit(byte_str.to_string()))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let bytes = [0xAAu8, 0xBB, 0x00, 0xFF];
        let s = encode(&bytes);
        assert_eq!(s, "aabb00ff");
        assert_eq!(decode_exact::<4>(&s).unwrap(), bytes);
    }

    #[test]
    fn rejects_odd_length() {
        assert_eq!(
            decode_exact::<2>("abc").unwrap_err(),
            HexError::OddLength(3)
        );
    }

    #[test]
    fn rejects_wrong_length() {
        assert_eq!(
            decode_exact::<32>("aabb").unwrap_err(),
            HexError::WrongLength {
                expected: 64,
                expected_bytes: 32,
                actual: 4
            }
        );
    }

    #[test]
    fn decode_vec_handles_arbitrary_even_length() {
        assert_eq!(decode_vec("aabbcc").unwrap(), vec![0xaa, 0xbb, 0xcc]);
        assert!(decode_vec("aabbc").is_err());
    }
}
