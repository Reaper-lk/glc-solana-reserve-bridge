//! RLP encoding tests, pinned to the Yellow Paper's own published
//! examples plus the exact shapes [`crate::evm::tx`] builds.

use super::*;
use crate::evm::u256::EvmU256;

#[test]
fn single_low_byte_encodes_as_itself() {
    // The special case that is easiest to get wrong: no length prefix.
    for byte in 0x00u8..0x80 {
        assert_eq!(encode_bytes(&[byte]), vec![byte], "byte {byte:#04x}");
    }
}

#[test]
fn single_high_byte_gets_a_length_prefix() {
    assert_eq!(encode_bytes(&[0x80]), vec![0x81, 0x80]);
    assert_eq!(encode_bytes(&[0xff]), vec![0x81, 0xff]);
}

#[test]
fn empty_string_is_0x80() {
    assert_eq!(encode_bytes(&[]), vec![0x80]);
}

#[test]
fn yellow_paper_dog_example() {
    // "dog" -> 0x83 'd' 'o' 'g'
    assert_eq!(encode_bytes(b"dog"), vec![0x83, b'd', b'o', b'g']);
}

#[test]
fn yellow_paper_cat_dog_list_example() {
    // [ "cat", "dog" ] -> 0xc8 0x83 c a t 0x83 d o g
    let list = encode_list(&[encode_bytes(b"cat"), encode_bytes(b"dog")]);
    assert_eq!(
        list,
        vec![0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']
    );
}

#[test]
fn empty_list_is_0xc0() {
    assert_eq!(encode_list(&[]), vec![0xc0]);
}

#[test]
fn fifty_five_byte_string_uses_the_short_form_and_fifty_six_uses_the_long_one() {
    // The exact boundary the two string forms meet at.
    let short = vec![0xaau8; 55];
    let encoded = encode_bytes(&short);
    assert_eq!(encoded[0], 0x80 + 55);
    assert_eq!(encoded.len(), 56);

    let long = vec![0xaau8; 56];
    let encoded = encode_bytes(&long);
    assert_eq!(encoded[0], 0xb7 + 1, "one length byte");
    assert_eq!(encoded[1], 56);
    assert_eq!(encoded.len(), 58);
}

#[test]
fn yellow_paper_long_string_example() {
    // The 56-character Lorem Ipsum example: 0xb8 0x38 then the bytes.
    let s = b"Lorem ipsum dolor sit amet, consectetur adipisicing elit";
    assert_eq!(s.len(), 56);
    let encoded = encode_bytes(s);
    assert_eq!(&encoded[..2], &[0xb8, 0x38]);
    assert_eq!(&encoded[2..], &s[..]);
}

#[test]
fn zero_encodes_as_the_empty_string_not_a_zero_byte() {
    // The single most consequential rule in this module: a transaction
    // with `value: 0` encoded as 0x00 instead of 0x80 hashes differently
    // and is rejected by every node.
    assert_eq!(encode_uint(0), vec![0x80]);
    assert_eq!(encode_u256(EvmU256::ZERO), vec![0x80]);
    assert_eq!(minimal_be_u64(0), Vec::<u8>::new());
}

#[test]
fn integers_are_minimal_big_endian() {
    assert_eq!(encode_uint(1), vec![0x01]);
    assert_eq!(encode_uint(0x7f), vec![0x7f]);
    assert_eq!(encode_uint(0x80), vec![0x81, 0x80]);
    assert_eq!(encode_uint(1024), vec![0x82, 0x04, 0x00]);
    assert_eq!(encode_uint(0x0102_0304), vec![0x84, 0x01, 0x02, 0x03, 0x04]);
    assert_eq!(
        encode_uint(u64::MAX),
        vec![0x88, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    );
}

#[test]
fn chain_id_4663_encodes_as_two_minimal_bytes() {
    // The value that goes into every Robinhood mainnet transaction.
    assert_eq!(encode_uint(4663), vec![0x82, 0x12, 0x37]);
}

#[test]
fn u256_strips_leading_zero_bytes() {
    assert_eq!(encode_u256(EvmU256::from_u64(1)), vec![0x01]);
    assert_eq!(encode_u256(EvmU256::from_u64(256)), vec![0x82, 0x01, 0x00]);
    // A full-width word keeps all 32 bytes and takes the 0xa0 prefix.
    let max = encode_u256(EvmU256::MAX);
    assert_eq!(max[0], 0x80 + 32);
    assert_eq!(max.len(), 33);
}

#[test]
fn u256_and_u64_agree_wherever_both_can_represent_the_value() {
    for value in [
        0u64,
        1,
        0x7f,
        0x80,
        255,
        256,
        4663,
        u32::MAX as u64,
        u64::MAX,
    ] {
        assert_eq!(
            encode_uint(value),
            encode_u256(EvmU256::from_u64(value)),
            "value {value}"
        );
    }
}

#[test]
fn a_long_list_uses_the_long_form_header() {
    // A list whose payload exceeds 55 bytes — the shape a real calldata-
    // bearing transaction always has.
    let big = encode_bytes(&[0x11u8; 100]);
    let list = encode_list(&[big]);
    assert_eq!(list[0], 0xf7 + 1);
    assert_eq!(list[1], 102, "100 payload bytes + a 2-byte string header");
}

#[test]
fn nested_lists_frame_their_children_encodings() {
    // [ [], [[]] ] -> 0xc3 0xc0 0xc1 0xc0
    let inner_empty = encode_list(&[]);
    let inner_nested = encode_list(&[encode_list(&[])]);
    assert_eq!(
        encode_list(&[inner_empty, inner_nested]),
        vec![0xc3, 0xc0, 0xc1, 0xc0]
    );
}
