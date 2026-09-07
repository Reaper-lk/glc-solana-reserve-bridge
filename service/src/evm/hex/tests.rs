use super::*;

#[test]
fn strips_exactly_the_lowercase_prefix() {
    assert_eq!(strip_prefix("0xabcd").unwrap(), "abcd");
    assert_eq!(strip_prefix("0x").unwrap(), "");
}

#[test]
fn rejects_an_uppercase_or_absent_prefix() {
    // `0X` is a second spelling of the same value; see the module docs.
    assert_eq!(
        strip_prefix("0XABCD").unwrap_err(),
        EvmHexError::MissingPrefix {
            found: "0XABCD".to_string()
        }
    );
    assert_eq!(
        strip_prefix("abcd").unwrap_err(),
        EvmHexError::MissingPrefix {
            found: "abcd".to_string()
        }
    );
    assert_eq!(
        strip_prefix("").unwrap_err(),
        EvmHexError::MissingPrefix {
            found: String::new()
        }
    );
}

#[test]
fn a_pathological_prefix_error_is_truncated() {
    let long = "z".repeat(4096);
    let EvmHexError::MissingPrefix { found } = strip_prefix(&long).unwrap_err() else {
        panic!("expected a MissingPrefix error");
    };
    assert_eq!(found.chars().count(), 8);
}

#[test]
fn decode_fixed_round_trips_through_encode_lower() {
    let bytes = [0x00u8, 0x01, 0x7f, 0x80, 0xff];
    let text = encode_lower(&bytes);
    assert_eq!(text, "0x00017f80ff");
    assert_eq!(decode_fixed::<5>(&text).unwrap(), bytes);
}

#[test]
fn decode_fixed_accepts_any_digit_case() {
    assert_eq!(decode_fixed::<2>("0xABcd").unwrap(), [0xab, 0xcd]);
    assert_eq!(decode_fixed::<2>("0xabcd").unwrap(), [0xab, 0xcd]);
    assert_eq!(decode_fixed::<2>("0xABCD").unwrap(), [0xab, 0xcd]);
}

#[test]
fn decode_fixed_reports_length_before_digits() {
    // An odd digit count at the wrong width reports the width, which is
    // the actionable half of the problem.
    assert_eq!(
        decode_fixed::<2>("0xabc").unwrap_err(),
        EvmHexError::WrongLength {
            expected: 4,
            actual: 3
        }
    );
    assert_eq!(
        decode_fixed::<2>("0x").unwrap_err(),
        EvmHexError::WrongLength {
            expected: 4,
            actual: 0
        }
    );
    // A body that is BOTH the wrong length and non-hex reports the length,
    // because that is the check that runs first.
    assert_eq!(
        decode_fixed::<2>("0xzz").unwrap_err(),
        EvmHexError::WrongLength {
            expected: 4,
            actual: 2
        }
    );
    // Only once the length is right does the digit check speak.
    assert_eq!(
        decode_fixed::<2>("0xzzzz").unwrap_err(),
        EvmHexError::InvalidDigit {
            position: 0,
            found: 'z'
        }
    );
}

#[test]
fn decode_fixed_reports_the_offending_digit_position() {
    assert_eq!(
        decode_fixed::<2>("0xabcg").unwrap_err(),
        EvmHexError::InvalidDigit {
            position: 3,
            found: 'g'
        }
    );
    assert_eq!(
        decode_fixed::<2>("0x abc").unwrap_err(),
        EvmHexError::InvalidDigit {
            position: 0,
            found: ' '
        }
    );
}

#[test]
fn decode_var_accepts_an_empty_body() {
    assert_eq!(decode_var("0x").unwrap(), Vec::<u8>::new());
    assert_eq!(encode_lower(&[]), "0x");
}

#[test]
fn decode_var_rejects_an_odd_digit_count() {
    assert_eq!(decode_var("0xabc").unwrap_err(), EvmHexError::OddLength(3));
}

#[test]
fn decode_var_round_trips_at_arbitrary_lengths() {
    for len in 0..40usize {
        let bytes: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
        let text = encode_lower(&bytes);
        assert_eq!(decode_var(&text).unwrap(), bytes, "length {len}");
    }
}

#[test]
fn find_non_hex_locates_the_first_offender() {
    assert_eq!(find_non_hex("abcdef0123456789ABCDEF"), None);
    assert_eq!(find_non_hex("abcx"), Some((3, 'x')));
    assert_eq!(find_non_hex("-1"), Some((0, '-')));
    assert_eq!(find_non_hex(""), None);
}
