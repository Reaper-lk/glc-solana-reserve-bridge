use super::*;

use crate::evm::hex::EvmHexError;

/// The four "Normal" EIP-55 test vectors, verbatim from the EIP's own test
/// suite. These are the strongest check in this module: each one's case
/// pattern is a keccak-256 checksum over its lowercase form, so all four
/// passing simultaneously confirms both the checksum algorithm and the
/// underlying keccak-256 implementation.
const EIP55_VECTORS: [&str; 4] = [
    "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed",
    "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359",
    "0xdbF03B407c01E7cD3CBea99509d93f8DDDC8C6FB",
    "0xD1220A0cf47c7B9Be7A2E6BA89F429762e7b9aDb",
];

/// The EIP-55 "All caps" and "All lower" vectors. These carry no checksum
/// information (see the type docs), so they must parse as-is.
const EIP55_ALL_CAPS: [&str; 2] = [
    "0x52908400098527886E0F7030069857D2E4169EE7",
    "0x8617E340B3D01FA5F11F306F4090FD50E238070D",
];
const EIP55_ALL_LOWER: [&str; 2] = [
    "0xde709f2102306220921060314715629080e2fb77",
    "0x27b1fdb04752bbc536007a920d24acb045561c26",
];

// --- Boundary values -------------------------------------------------

#[test]
fn parses_the_zero_address() {
    let text = "0x0000000000000000000000000000000000000000";
    let address: EvmAddress = text.parse().unwrap();
    assert_eq!(address, EvmAddress::ZERO);
    assert_eq!(address.to_bytes(), [0u8; 20]);
    assert!(address.is_zero());
    assert_eq!(address.to_string(), text);
}

#[test]
fn parses_the_all_ff_address() {
    // All-uppercase, so no checksum is claimed and it parses as plain hex.
    let address: EvmAddress = "0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
        .parse()
        .unwrap();
    assert_eq!(address.to_bytes(), [0xffu8; 20]);
    assert!(!address.is_zero());
    assert_eq!(
        address.to_string(),
        "0xffffffffffffffffffffffffffffffffffffffff"
    );
}

#[test]
fn the_zero_address_is_a_real_address_not_a_none() {
    // Pinning the deliberate decision recorded on `EvmAddress::ZERO`: it
    // parses and compares like any other address. Excluding it is a
    // policy each caller applies, not something the parser does.
    assert!("0x0000000000000000000000000000000000000000"
        .parse::<EvmAddress>()
        .is_ok());
    assert!(EvmAddress::ZERO.is_zero());
    assert!(!EvmAddress::from_bytes([0u8; 20]).to_string().is_empty());
}

// --- EIP-55 ----------------------------------------------------------

#[test]
fn accepts_every_published_eip55_checksummed_vector() {
    for vector in EIP55_VECTORS {
        let address: EvmAddress = vector
            .parse()
            .unwrap_or_else(|err| panic!("{vector} should parse: {err}"));
        assert_eq!(address.to_checksum_string(), vector);
    }
}

#[test]
fn computes_the_published_checksum_from_the_lowercase_form() {
    for vector in EIP55_VECTORS {
        let lower = vector.to_lowercase();
        let address: EvmAddress = lower.parse().unwrap();
        assert_eq!(
            address.to_checksum_string(),
            vector,
            "checksumming {lower} must reproduce {vector}"
        );
    }
}

#[test]
fn accepts_the_all_caps_and_all_lower_vectors_unchecked() {
    for vector in EIP55_ALL_CAPS.iter().chain(EIP55_ALL_LOWER.iter()) {
        assert!(
            vector.parse::<EvmAddress>().is_ok(),
            "{vector} carries no checksum information and must be accepted"
        );
    }
}

#[test]
fn rejects_a_mixed_case_address_whose_checksum_is_wrong() {
    // The first vector with two letter digits' cases swapped: same bytes,
    // a case pattern that no longer matches the keccak checksum.
    let corrupted = "0x5AaEb6053F3E94C9b9A09f33669435E7Ef1BeAed";
    let err = corrupted.parse::<EvmAddress>().unwrap_err();
    assert_eq!(
        err,
        EvmAddressError::ChecksumMismatch {
            given: corrupted.to_string(),
            expected: EIP55_VECTORS[0].to_string(),
        }
    );
    // The message names both spellings so the difference is visible.
    let message = err.to_string();
    assert!(message.contains(corrupted), "{message}");
    assert!(message.contains(EIP55_VECTORS[0]), "{message}");
}

#[test]
fn a_mistyped_digit_in_a_checksummed_address_is_caught() {
    // This is what the checksum is for: one digit of a real address
    // changed, which no length or hex check can see.
    let mistyped = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAee";
    assert!(matches!(
        mistyped.parse::<EvmAddress>(),
        Err(EvmAddressError::ChecksumMismatch { .. })
    ));
    // The same digit change in the all-lowercase spelling is undetectable
    // and is accepted, which is exactly why operators should paste the
    // checksummed form.
    assert!(mistyped.to_lowercase().parse::<EvmAddress>().is_ok());
}

#[test]
fn a_checksum_only_matters_when_letters_of_both_cases_are_present() {
    // No letter digits at all: nothing to checksum either way.
    let digits_only = "0x1234567890123456789012345678901234567890";
    assert!(digits_only.parse::<EvmAddress>().is_ok());
}

// --- Malformed shapes ------------------------------------------------

#[test]
fn rejects_a_missing_prefix() {
    let err = "5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        .parse::<EvmAddress>()
        .unwrap_err();
    assert!(matches!(
        err,
        EvmAddressError::Hex(EvmHexError::MissingPrefix { .. })
    ));
}

#[test]
fn rejects_an_uppercase_prefix() {
    assert!(matches!(
        "0X5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed".parse::<EvmAddress>(),
        Err(EvmAddressError::Hex(EvmHexError::MissingPrefix { .. }))
    ));
}

#[test]
fn rejects_a_too_short_address_rather_than_padding_it() {
    // 38 digits: a 19-byte value. A padding implementation would have
    // silently produced a different, valid-looking address.
    assert_eq!(
        "0x00000000000000000000000000000000000000"
            .parse::<EvmAddress>()
            .unwrap_err(),
        EvmAddressError::Hex(EvmHexError::WrongLength {
            expected: 40,
            actual: 38
        })
    );
}

#[test]
fn rejects_a_too_long_address_rather_than_truncating_it() {
    // 42 digits: a 21-byte value. A truncating implementation would have
    // dropped a byte from one end and paid out to the wrong account.
    assert_eq!(
        "0x0000000000000000000000000000000000000000ff"
            .parse::<EvmAddress>()
            .unwrap_err(),
        EvmAddressError::Hex(EvmHexError::WrongLength {
            expected: 40,
            actual: 42
        })
    );
}

#[test]
fn rejects_an_odd_digit_count() {
    assert_eq!(
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAe"
            .parse::<EvmAddress>()
            .unwrap_err(),
        EvmAddressError::Hex(EvmHexError::WrongLength {
            expected: 40,
            actual: 39
        })
    );
}

#[test]
fn rejects_an_empty_string_and_a_bare_prefix() {
    assert!(matches!(
        "".parse::<EvmAddress>(),
        Err(EvmAddressError::Hex(EvmHexError::MissingPrefix { .. }))
    ));
    assert_eq!(
        "0x".parse::<EvmAddress>().unwrap_err(),
        EvmAddressError::Hex(EvmHexError::WrongLength {
            expected: 40,
            actual: 0
        })
    );
}

#[test]
fn rejects_non_hex_characters() {
    // Right length, one character that is not a hex digit.
    assert_eq!(
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAeZ"
            .parse::<EvmAddress>()
            .unwrap_err(),
        EvmAddressError::Hex(EvmHexError::InvalidDigit {
            position: 39,
            found: 'Z'
        })
    );
    // Whitespace, which a trimming parser would have quietly accepted.
    assert!(matches!(
        "0x 5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAe".parse::<EvmAddress>(),
        Err(EvmAddressError::Hex(EvmHexError::InvalidDigit { .. }))
    ));
    // A trailing newline, the classic config-file paste.
    assert!(matches!(
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed\n".parse::<EvmAddress>(),
        Err(EvmAddressError::Hex(EvmHexError::WrongLength {
            expected: 40,
            actual: 41
        }))
    ));
}

#[test]
fn rejects_a_multibyte_character_without_panicking() {
    // A non-ASCII character makes byte length and character count differ;
    // the parser must not index into the middle of it.
    let with_emoji = format!("0x{}{}", "0".repeat(39), '\u{1F600}');
    assert!(with_emoji.parse::<EvmAddress>().is_err());
    let all_multibyte = format!("0x{}", "\u{00e9}".repeat(20));
    assert!(all_multibyte.parse::<EvmAddress>().is_err());
}

// --- Bytes <-> type --------------------------------------------------

#[test]
fn try_from_slice_requires_exactly_twenty_bytes() {
    assert_eq!(
        EvmAddress::try_from_slice(&[0xabu8; 20]).unwrap(),
        EvmAddress::from_bytes([0xabu8; 20])
    );
    assert_eq!(
        EvmAddress::try_from_slice(&[0xabu8; 19]).unwrap_err(),
        EvmAddressError::WrongByteLength { actual: 19 }
    );
    assert_eq!(
        EvmAddress::try_from_slice(&[0xabu8; 21]).unwrap_err(),
        EvmAddressError::WrongByteLength { actual: 21 }
    );
    assert_eq!(
        EvmAddress::try_from_slice(&[]).unwrap_err(),
        EvmAddressError::WrongByteLength { actual: 0 }
    );
    // A 32-byte value — the shape a Solana program id or an ABI-padded
    // word has — must never be silently narrowed to an address.
    assert_eq!(
        EvmAddress::try_from_slice(&[0xabu8; 32]).unwrap_err(),
        EvmAddressError::WrongByteLength { actual: 32 }
    );
}

#[test]
fn as_bytes_and_to_bytes_agree() {
    let address: EvmAddress = EIP55_VECTORS[0].parse().unwrap();
    assert_eq!(*address.as_bytes(), address.to_bytes());
}

// --- Round trips -----------------------------------------------------

#[test]
fn round_trips_through_display_and_through_the_checksummed_form() {
    for vector in EIP55_VECTORS
        .iter()
        .chain(EIP55_ALL_CAPS.iter())
        .chain(EIP55_ALL_LOWER.iter())
    {
        let address: EvmAddress = vector.parse().unwrap();

        let displayed = address.to_string();
        assert_eq!(displayed, displayed.to_lowercase());
        assert_eq!(displayed.parse::<EvmAddress>().unwrap(), address);

        let checksummed = address.to_checksum_string();
        assert_eq!(checksummed.parse::<EvmAddress>().unwrap(), address);

        assert_eq!(
            displayed.len(),
            checksummed.len(),
            "both forms are 2 + 40 characters"
        );
        assert_eq!(displayed.len(), 42);
    }
}

#[test]
fn round_trips_over_the_whole_byte_range() {
    // Exercises every byte value in every position, including the
    // boundaries the checksum's `nibble >= 8` test turns on.
    for seed in 0u16..=255 {
        let mut bytes = [0u8; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (seed as usize).wrapping_add(index * 31) as u8;
        }
        let address = EvmAddress::from_bytes(bytes);
        assert_eq!(address.to_string().parse::<EvmAddress>().unwrap(), address);
        assert_eq!(
            address
                .to_checksum_string()
                .parse::<EvmAddress>()
                .unwrap()
                .to_bytes(),
            bytes
        );
    }
}

#[test]
fn debug_is_readable_and_shows_the_hex_form() {
    let address: EvmAddress = EIP55_VECTORS[0].parse().unwrap();
    assert_eq!(
        format!("{address:?}"),
        "EvmAddress(0x5aaeb6053f3e94c9b9a09f33669435e7ef1beaed)"
    );
}

#[test]
fn ordering_and_hashing_are_over_the_bytes() {
    use std::collections::HashSet;

    let low = EvmAddress::from_bytes([0u8; 20]);
    let high = EvmAddress::from_bytes([0xffu8; 20]);
    assert!(low < high);

    // The same bytes reached through two different spellings are one value.
    let mut set = HashSet::new();
    set.insert(EIP55_VECTORS[0].parse::<EvmAddress>().unwrap());
    set.insert(
        EIP55_VECTORS[0]
            .to_lowercase()
            .parse::<EvmAddress>()
            .unwrap(),
    );
    assert_eq!(set.len(), 1);
}
