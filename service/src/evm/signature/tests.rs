use super::*;

use crate::evm::hex::EvmHexError;

/// A structurally valid signature: non-zero `r` and `s`, `v = 27`.
fn valid_bytes() -> [u8; SIGNATURE_BYTES] {
    let mut bytes = [0u8; SIGNATURE_BYTES];
    bytes[..32].fill(0x11);
    bytes[32..64].fill(0x22);
    bytes[64] = 27;
    bytes
}

fn valid_text() -> String {
    format!("0x{}{}1b", "11".repeat(32), "22".repeat(32))
}

// --- Length ----------------------------------------------------------

#[test]
fn accepts_the_correct_length() {
    let signature = EvmSignature::from_bytes(valid_bytes()).unwrap();
    assert_eq!(signature.to_bytes(), valid_bytes());
    assert_eq!(*signature.r(), [0x11u8; 32]);
    assert_eq!(*signature.s(), [0x22u8; 32]);
    assert_eq!(signature.v(), 27);

    let parsed: EvmSignature = valid_text().parse().unwrap();
    assert_eq!(parsed, signature);
}

#[test]
fn rejects_a_short_value() {
    // 64 bytes: an `r || s` pair whose recovery byte was lost. Completing it
    // with a guessed `v` would recover a different, attacker-chosen address.
    let bytes = valid_bytes();
    assert_eq!(
        EvmSignature::try_from_slice(&bytes[..64]).unwrap_err(),
        EvmSignatureError::WrongByteLength { actual: 64 }
    );
    assert_eq!(
        EvmSignature::try_from_slice(&[]).unwrap_err(),
        EvmSignatureError::WrongByteLength { actual: 0 }
    );
    // The textual form of a 64-byte value: 128 digits.
    let short_text = format!("0x{}{}", "11".repeat(32), "22".repeat(32));
    assert_eq!(
        short_text.parse::<EvmSignature>().unwrap_err(),
        EvmSignatureError::Hex(EvmHexError::WrongLength {
            expected: 130,
            actual: 128
        })
    );
}

#[test]
fn rejects_a_long_value() {
    let mut bytes = valid_bytes().to_vec();
    bytes.push(0x00);
    assert_eq!(
        EvmSignature::try_from_slice(&bytes).unwrap_err(),
        EvmSignatureError::WrongByteLength { actual: 66 }
    );
    let long_text = format!("{}00", valid_text());
    assert_eq!(
        long_text.parse::<EvmSignature>().unwrap_err(),
        EvmSignatureError::Hex(EvmHexError::WrongLength {
            expected: 130,
            actual: 132
        })
    );
}

#[test]
fn rejects_an_odd_digit_count() {
    let odd = format!("{}0", valid_text());
    assert_eq!(
        odd.parse::<EvmSignature>().unwrap_err(),
        EvmSignatureError::Hex(EvmHexError::WrongLength {
            expected: 130,
            actual: 131
        })
    );
}

// --- Malformed hex ---------------------------------------------------

#[test]
fn rejects_malformed_hex() {
    let mut bad = valid_text();
    bad.replace_range(10..11, "z");
    assert!(matches!(
        bad.parse::<EvmSignature>(),
        Err(EvmSignatureError::Hex(EvmHexError::InvalidDigit { .. }))
    ));
}

#[test]
fn rejects_a_missing_or_uppercase_prefix() {
    assert!(matches!(
        valid_text()[2..].parse::<EvmSignature>(),
        Err(EvmSignatureError::Hex(EvmHexError::MissingPrefix { .. }))
    ));
    assert!(matches!(
        format!("0X{}", &valid_text()[2..]).parse::<EvmSignature>(),
        Err(EvmSignatureError::Hex(EvmHexError::MissingPrefix { .. }))
    ));
    assert!("".parse::<EvmSignature>().is_err());
    assert!("0x".parse::<EvmSignature>().is_err());
}

#[test]
fn accepts_any_digit_case_and_emits_lowercase() {
    let upper = valid_text().to_uppercase().replace("0X", "0x");
    let parsed: EvmSignature = upper.parse().unwrap();
    assert_eq!(parsed.to_string(), valid_text());
    assert_eq!(parsed.to_string(), parsed.to_string().to_lowercase());
}

// --- Recovery byte ---------------------------------------------------

#[test]
fn accepts_every_compact_recovery_byte() {
    for (v, expected_recovery_id) in [(0u8, 0u8), (1, 1), (27, 0), (28, 1)] {
        let mut bytes = valid_bytes();
        bytes[64] = v;
        let signature = EvmSignature::from_bytes(bytes).unwrap();
        assert_eq!(signature.v(), v, "v must be preserved verbatim");
        assert_eq!(
            signature.recovery_id(),
            expected_recovery_id,
            "v = {v} normalises to {expected_recovery_id}"
        );
    }
}

#[test]
fn rejects_a_recovery_byte_that_is_not_a_compact_one() {
    for v in [2u8, 26, 29, 30, 35, 36, 37, 38, 255] {
        let mut bytes = valid_bytes();
        bytes[64] = v;
        assert_eq!(
            EvmSignature::from_bytes(bytes).unwrap_err(),
            EvmSignatureError::InvalidRecoveryByte { found: v },
            "v = {v} must be rejected"
        );
    }
}

#[test]
fn an_eip155_transaction_v_cannot_be_squeezed_into_this_form() {
    // For Robinhood mainnet (chain id 4663) EIP-155's v is 4663*2+35 = 9361,
    // which does not fit a byte at all; its low byte is 0x91 = 145. A type
    // that accepted any byte would have silently taken that truncation.
    let eip155_v: u64 = 4663 * 2 + 35;
    assert_eq!(eip155_v, 9361);
    assert!(u8::try_from(eip155_v).is_err());
    let mut bytes = valid_bytes();
    bytes[64] = (eip155_v & 0xff) as u8;
    assert_eq!(
        EvmSignature::from_bytes(bytes).unwrap_err(),
        EvmSignatureError::InvalidRecoveryByte { found: 0x91 }
    );
}

// --- Zero components -------------------------------------------------

#[test]
fn rejects_a_zero_r_or_s() {
    let mut zero_r = valid_bytes();
    zero_r[..32].fill(0);
    assert_eq!(
        EvmSignature::from_bytes(zero_r).unwrap_err(),
        EvmSignatureError::ZeroComponent { component: "r" }
    );

    let mut zero_s = valid_bytes();
    zero_s[32..64].fill(0);
    assert_eq!(
        EvmSignature::from_bytes(zero_s).unwrap_err(),
        EvmSignatureError::ZeroComponent { component: "s" }
    );

    // The all-zero signature — the "unsigned" sentinel — is refused, and is
    // refused for `r` first so the message names a single definite cause.
    let all_zero = [0u8; SIGNATURE_BYTES];
    assert_eq!(
        EvmSignature::from_bytes(all_zero).unwrap_err(),
        EvmSignatureError::ZeroComponent { component: "r" }
    );
    // ...including through the textual path.
    assert!(matches!(
        format!("0x{}1b", "00".repeat(64)).parse::<EvmSignature>(),
        Err(EvmSignatureError::ZeroComponent { .. })
    ));
}

#[test]
fn a_single_non_zero_byte_is_enough_to_be_non_zero() {
    let mut bytes = [0u8; SIGNATURE_BYTES];
    bytes[31] = 1; // r = 1
    bytes[63] = 1; // s = 1
    bytes[64] = 28;
    let signature = EvmSignature::from_bytes(bytes).unwrap();
    assert_eq!(signature.recovery_id(), 1);
}

// --- Byte layout -----------------------------------------------------

#[test]
fn the_component_split_is_r_then_s_then_v() {
    // A swapped r/s split is invisible in a round trip, so the layout is
    // asserted against distinguishable component values directly.
    let mut bytes = [0u8; SIGNATURE_BYTES];
    for (index, byte) in bytes.iter_mut().enumerate().take(64) {
        *byte = index as u8 + 1;
    }
    bytes[64] = 28;

    let signature = EvmSignature::from_bytes(bytes).unwrap();
    assert_eq!(signature.r()[0], 1, "r starts at byte 0");
    assert_eq!(signature.r()[31], 32, "r ends at byte 31");
    assert_eq!(signature.s()[0], 33, "s starts at byte 32");
    assert_eq!(signature.s()[31], 64, "s ends at byte 63");
    assert_eq!(signature.v(), 28, "v is the last byte");
}

// --- Round trips -----------------------------------------------------

#[test]
fn round_trips_through_bytes_and_text() {
    for v in [0u8, 1, 27, 28] {
        for seed in [0x01u8, 0x7f, 0xff] {
            let mut bytes = [seed; SIGNATURE_BYTES];
            bytes[64] = v;
            let signature = EvmSignature::from_bytes(bytes).unwrap();

            assert_eq!(signature.to_bytes(), bytes);
            assert_eq!(
                EvmSignature::from_bytes(signature.to_bytes()).unwrap(),
                signature
            );

            let text = signature.to_string();
            assert_eq!(text.len(), 2 + SIGNATURE_HEX_DIGITS);
            assert_eq!(text.len(), 132);
            assert_eq!(text.parse::<EvmSignature>().unwrap(), signature);

            assert_eq!(
                EvmSignature::try_from_slice(&signature.to_bytes()).unwrap(),
                signature
            );
        }
    }
}

#[test]
fn debug_is_readable() {
    let signature = EvmSignature::from_bytes(valid_bytes()).unwrap();
    assert_eq!(
        format!("{signature:?}"),
        format!("EvmSignature({})", valid_text())
    );
}

#[test]
fn equal_signatures_hash_as_one_key() {
    use std::collections::HashSet;

    let mut set = HashSet::new();
    set.insert(EvmSignature::from_bytes(valid_bytes()).unwrap());
    set.insert(valid_text().parse::<EvmSignature>().unwrap());
    set.insert(
        valid_text()
            .to_uppercase()
            .replace("0X", "0x")
            .parse::<EvmSignature>()
            .unwrap(),
    );
    assert_eq!(set.len(), 1);
}
