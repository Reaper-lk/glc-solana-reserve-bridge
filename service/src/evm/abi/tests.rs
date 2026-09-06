//! ABI encoding tests. Every selector this crate uses is pinned against
//! an independently computed keccak, and every calldata layout is
//! asserted word by word rather than against a blob this file also
//! produced.

use super::*;

fn addr(byte: u8) -> EvmAddress {
    EvmAddress::from_bytes([byte; 20])
}

#[test]
fn selector_matches_known_erc20_signatures() {
    // Published, universally known ERC-20 selectors — an independent
    // check that the keccak and the four-byte truncation are right.
    assert_eq!(hex(&selector("transfer(address,uint256)")), "a9059cbb");
    assert_eq!(hex(&selector("balanceOf(address)")), "70a08231");
    assert_eq!(hex(&selector("decimals()")), "313ce567");
    assert_eq!(hex(&selector("totalSupply()")), "18160ddd");
    assert_eq!(
        hex(&selector("transferFrom(address,address,uint256)")),
        "23b872dd"
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn address_word_is_right_aligned() {
    let word = word_address(addr(0xab));
    assert_eq!(&word[..12], &[0u8; 12], "high bytes must be zero");
    assert_eq!(&word[12..], &[0xabu8; 20]);
}

#[test]
fn uint_words_are_right_aligned() {
    assert_eq!(word_u128(1)[31], 1);
    assert_eq!(word_u128(1)[..31], [0u8; 31]);
    assert_eq!(word_u128(0x0102)[30..], [0x01, 0x02]);
}

#[test]
fn bool_words_are_zero_or_one() {
    assert_eq!(word_bool(false), [0u8; 32]);
    let t = word_bool(true);
    assert_eq!(t[31], 1);
    assert_eq!(t[..31], [0u8; 31]);
}

#[test]
fn a_call_with_only_static_arguments_is_selector_plus_words() {
    let data = Calldata::new("routeEnabled(uint8)")
        .word(word_u128(2))
        .finish();
    assert_eq!(data.len(), 4 + 32);
    assert_eq!(&data[..4], &selector("routeEnabled(uint8)"));
    assert_eq!(data[35], 2);
}

#[test]
fn a_dynamic_offset_is_measured_from_after_the_selector() {
    // One static word then one dynamic: the dynamic's offset must be
    // 0x40 (two head words), NOT 0x44 (which would include the selector).
    let data = Calldata::new("f(uint256,bytes)")
        .word(word_u128(7))
        .bytes(vec![0xaa, 0xbb])
        .finish();
    let offset_word = &data[36..68];
    assert_eq!(
        EvmU256::from_be_bytes(offset_word.try_into().unwrap())
            .try_to_u64()
            .unwrap(),
        64,
        "the offset must exclude the four selector bytes"
    );
    // At that offset (plus the selector) sits the length word then the data.
    let tail = &data[4 + 64..];
    assert_eq!(
        EvmU256::from_be_bytes(tail[..32].try_into().unwrap())
            .try_to_u64()
            .unwrap(),
        2
    );
    assert_eq!(&tail[32..34], &[0xaa, 0xbb]);
    assert_eq!(tail.len(), 64, "the payload is padded to one full word");
}

#[test]
fn an_empty_bytes_value_is_one_zero_length_word_and_no_padding() {
    let data = Calldata::new("f(bytes)").bytes(Vec::new()).finish();
    assert_eq!(data.len(), 4 + 32 + 32);
    assert_eq!(&data[36..], &[0u8; 32]);
}

#[test]
fn a_bytes_array_encodes_count_then_element_offsets_then_elements() {
    // Two 65-byte signatures, the exact shape every authorized call takes.
    let sig_a = vec![0x11u8; 65];
    let sig_b = vec![0x22u8; 65];
    let data = Calldata::new("f(bytes[])")
        .bytes_array(vec![sig_a.clone(), sig_b.clone()])
        .finish();

    let body = &data[4..];
    // Head: one offset word pointing at 0x20.
    assert_eq!(read_u64(&body[..32]), 32);
    let array = &body[32..];
    // Count.
    assert_eq!(read_u64(&array[..32]), 2);
    // Element offsets, relative to the start of the offset table.
    let off_a = read_u64(&array[32..64]) as usize;
    let off_b = read_u64(&array[64..96]) as usize;
    assert_eq!(off_a, 64, "two offset words precede the first element");
    // Element A occupies a length word plus 96 padded bytes (65 -> 3 words).
    assert_eq!(off_b, 64 + 32 + 96);

    let elem_a = &array[32 + off_a..];
    assert_eq!(read_u64(&elem_a[..32]), 65);
    assert_eq!(&elem_a[32..32 + 65], &sig_a[..]);
    assert_eq!(
        &elem_a[32 + 65..32 + 96],
        &[0u8; 31],
        "the tail of the last word is zero-padded"
    );

    let elem_b = &array[32 + off_b..];
    assert_eq!(read_u64(&elem_b[..32]), 65);
    assert_eq!(&elem_b[32..32 + 65], &sig_b[..]);
}

#[test]
fn an_empty_bytes_array_encodes_as_a_single_zero_count() {
    let data = Calldata::new("f(bytes[])").bytes_array(Vec::new()).finish();
    assert_eq!(data.len(), 4 + 32 + 32);
    assert_eq!(read_u64(&data[36..68]), 0);
}

#[test]
fn a_static_struct_is_encoded_inline_as_its_fields() {
    // `executeSettlement((bytes32,uint256,uint64,uint64),bytes[])`: the
    // struct contributes FOUR head words, then the array's offset. If the
    // struct were (wrongly) treated as dynamic, the offset would be at
    // word 0 instead of word 4.
    let data = Calldata::new("executeSettlement((bytes32,uint256,uint64,uint64),bytes[])")
        .word(word_bytes32([0x5a; 32]))
        .word(word_u128(3))
        .word(word_u128(9))
        .word(word_u128(1_800_000_000))
        .bytes_array(vec![vec![0x01; 65], vec![0x02; 65]])
        .finish();
    let body = &data[4..];
    assert_eq!(&body[..32], &[0x5au8; 32]);
    assert_eq!(read_u64(&body[32..64]), 3);
    assert_eq!(read_u64(&body[64..96]), 9);
    assert_eq!(read_u64(&body[96..128]), 1_800_000_000);
    assert_eq!(read_u64(&body[128..160]), 160, "five head words");
}

fn read_u64(word: &[u8]) -> u64 {
    EvmU256::from_be_bytes(word.try_into().unwrap())
        .try_to_u64()
        .unwrap()
}

// ------------------------------------------------------------ decoding --

#[test]
fn return_words_refuses_a_short_or_long_return() {
    assert_eq!(
        return_words::<1>(&[]),
        Err(AbiDecodeError::WrongLength {
            expected: 1,
            actual: 0
        }),
        "an empty return is what a call to a non-contract address yields"
    );
    assert_eq!(
        return_words::<1>(&[0u8; 64]),
        Err(AbiDecodeError::WrongLength {
            expected: 1,
            actual: 64
        })
    );
    assert!(return_words::<2>(&[0u8; 64]).is_ok());
}

#[test]
fn decode_address_refuses_dirty_high_bytes() {
    let mut word = word_address(addr(0x11));
    assert_eq!(decode_address(&word, "token").unwrap(), addr(0x11));
    word[0] = 1;
    assert!(decode_address(&word, "token").is_err());
}

#[test]
fn decode_bool_refuses_anything_but_zero_or_one() {
    assert_eq!(decode_bool(&word_bool(true), "x"), Ok(true));
    assert_eq!(decode_bool(&word_bool(false), "x"), Ok(false));
    let mut word = [0u8; 32];
    word[31] = 2;
    assert!(decode_bool(&word, "x").is_err());
    word[31] = 1;
    word[0] = 1;
    assert!(decode_bool(&word, "x").is_err());
}

#[test]
fn decode_u64_refuses_a_wider_value() {
    assert_eq!(
        decode_u64(&word_u128(u64::MAX as u128), "e").unwrap(),
        u64::MAX
    );
    assert!(decode_u64(&word_u128(u64::MAX as u128 + 1), "e").is_err());
}

#[test]
fn decode_u8_refuses_a_wider_value() {
    assert_eq!(decode_u8(&word_u128(18), "decimals").unwrap(), 18);
    assert_eq!(decode_u8(&word_u128(255), "decimals").unwrap(), 255);
    assert!(decode_u8(&word_u128(256), "decimals").is_err());
}
