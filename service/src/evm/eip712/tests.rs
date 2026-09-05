use super::*;

use crate::evm::hex::encode_lower;
use crate::evm::networks::ROBINHOOD_MAINNET_CHAIN_ID;

/// The domain from EIP-712's own "Ether Mail" example, the vector the EIP
/// publishes alongside its expected hashes.
fn ether_mail_domain() -> Eip712Domain {
    Eip712Domain::new()
        .with_name("Ether Mail")
        .with_version("1")
        .with_chain_id(EvmChainId::new(1).unwrap())
        .with_verifying_contract(
            "0xCcCCccccCCCCcCCCCCCcCcCccCcCCCcCcccccccC"
                .parse()
                .unwrap(),
        )
}

/// The published `domainSeparator` for that domain.
const ETHER_MAIL_DOMAIN_SEPARATOR: &str =
    "0xf2cee375fa42b42143804025fc449deafd50cc031ca257e0b194a650a912090f";

/// The published `hashStruct(message)` for the example `Mail`.
const ETHER_MAIL_STRUCT_HASH: &str =
    "0xc52c0ee5d84264471806290a3f2c4cecfc5490626bf912d01f240d7a274b371e";

/// The published final digest — the bytes that get signed.
const ETHER_MAIL_DIGEST: &str =
    "0xbe609aee343fb3c4b28e1df9e632fca64fcfaede20f02e86244efddf30957bd2";

// --- The published EIP-712 vector ------------------------------------

#[test]
fn reproduces_the_published_ether_mail_type_string() {
    assert_eq!(
        ether_mail_domain().type_string(),
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
    );
}

#[test]
fn reproduces_the_published_ether_mail_domain_separator() {
    assert_eq!(
        encode_lower(&ether_mail_domain().separator()),
        ETHER_MAIL_DOMAIN_SEPARATOR
    );
}

#[test]
fn reproduces_the_published_ether_mail_digest() {
    let separator = ether_mail_domain().separator();
    let struct_hash: [u8; 32] = crate::evm::quantity::parse_data_exact(ETHER_MAIL_STRUCT_HASH)
        .expect("the published struct hash is a 32-byte value");
    assert_eq!(
        encode_lower(&typed_data_hash(&separator, &struct_hash)),
        ETHER_MAIL_DIGEST
    );
}

#[test]
fn the_domain_type_hash_matches_the_hash_of_the_type_string() {
    let domain = ether_mail_domain();
    assert_eq!(
        domain.type_hash(),
        crate::evm::keccak256(domain.type_string().as_bytes())
    );
    // The well-known EIP712Domain typehash for the four-field domain.
    assert_eq!(
        encode_lower(&domain.type_hash()),
        "0x8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f"
    );
}

// --- Field presence changes the type ---------------------------------

#[test]
fn only_the_present_fields_appear_in_the_type_string() {
    assert_eq!(Eip712Domain::new().type_string(), "EIP712Domain()");
    assert_eq!(
        Eip712Domain::new().with_name("A").type_string(),
        "EIP712Domain(string name)"
    );
    assert_eq!(
        Eip712Domain::new()
            .with_chain_id(ROBINHOOD_MAINNET_CHAIN_ID)
            .type_string(),
        "EIP712Domain(uint256 chainId)"
    );
    assert_eq!(
        Eip712Domain::new().with_salt([0u8; 32]).type_string(),
        "EIP712Domain(bytes32 salt)"
    );
    assert_eq!(
        Eip712Domain::new()
            .with_name("A")
            .with_version("1")
            .with_chain_id(ROBINHOOD_MAINNET_CHAIN_ID)
            .with_verifying_contract(crate::evm::EvmAddress::ZERO)
            .with_salt([0u8; 32])
            .type_string(),
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract,bytes32 salt)"
    );
}

#[test]
fn the_field_order_is_the_eips_order_not_the_insertion_order() {
    // Built back-to-front; the type string must still be canonical, because
    // a different order is a different type hash and therefore a different
    // signature.
    let domain = Eip712Domain::new()
        .with_salt([1u8; 32])
        .with_verifying_contract(crate::evm::EvmAddress::ZERO)
        .with_chain_id(ROBINHOOD_MAINNET_CHAIN_ID)
        .with_version("1")
        .with_name("A");
    assert_eq!(
        domain.type_string(),
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract,bytes32 salt)"
    );
}

#[test]
fn adding_a_field_changes_the_separator() {
    let base = Eip712Domain::new().with_name("GlcRobinhoodBridge");
    let with_version = base.clone().with_version("1");
    let with_salt = base.clone().with_salt([0u8; 32]);
    assert_ne!(base.separator(), with_version.separator());
    assert_ne!(base.separator(), with_salt.separator());
    assert_ne!(with_version.separator(), with_salt.separator());
}

// --- The properties the domain exists for ----------------------------

#[test]
fn a_different_chain_id_gives_a_different_separator() {
    // This is what stops a testnet signature being replayed on mainnet.
    let base = Eip712Domain::new()
        .with_name("GlcRobinhoodBridge")
        .with_version("1");
    let mainnet = base
        .clone()
        .with_chain_id(crate::evm::networks::ROBINHOOD_MAINNET_CHAIN_ID);
    let testnet = base.with_chain_id(crate::evm::networks::ROBINHOOD_TESTNET_CHAIN_ID);
    assert_ne!(mainnet.separator(), testnet.separator());
}

#[test]
fn a_different_verifying_contract_gives_a_different_separator() {
    // This is what stops a signature for one deployment being replayed
    // against its successor.
    let base = Eip712Domain::new()
        .with_name("GlcRobinhoodBridge")
        .with_version("1")
        .with_chain_id(ROBINHOOD_MAINNET_CHAIN_ID);
    let first = base.clone().with_verifying_contract(
        "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
            .parse()
            .unwrap(),
    );
    let second = base.with_verifying_contract(
        "0xfB6916095ca1df60bB79Ce92cE3Ea74c37c5d359"
            .parse()
            .unwrap(),
    );
    assert_ne!(first.separator(), second.separator());
}

#[test]
fn the_separator_is_deterministic() {
    let domain = ether_mail_domain();
    assert_eq!(domain.separator(), domain.separator());
    assert_eq!(domain.separator(), ether_mail_domain().separator());
}

// --- The 0x1901 prefix -----------------------------------------------

#[test]
fn the_prefix_is_the_structured_data_one() {
    assert_eq!(EIP712_PREFIX, [0x19, 0x01]);
    // 0x19 0x45 is EIP-191's personal_sign version, a different scheme; a
    // digest built with it must not equal the structured-data digest.
    let separator = [0xaau8; 32];
    let struct_hash = [0xbbu8; 32];
    let correct = typed_data_hash(&separator, &struct_hash);
    let personal_sign =
        crate::evm::keccak::keccak256_concat(&[&[0x19u8, 0x45], &separator, &struct_hash]);
    assert_ne!(correct, personal_sign);
    // ...and so must an unprefixed one.
    let unprefixed = crate::evm::keccak::keccak256_concat(&[&separator, &struct_hash]);
    assert_ne!(correct, unprefixed);
}

#[test]
fn the_digest_depends_on_both_halves_and_on_their_order() {
    let a = [0x01u8; 32];
    let b = [0x02u8; 32];
    assert_ne!(typed_data_hash(&a, &b), typed_data_hash(&b, &a));
    assert_ne!(typed_data_hash(&a, &b), typed_data_hash(&a, &a));
}

// --- Primitive field encoders ----------------------------------------

#[test]
fn encode_address_right_aligns_with_twelve_zero_bytes() {
    // The mistake this pins down: left-aligning the 20 bytes, which encodes
    // a different and enormous value and is invisible without counting.
    let address: crate::evm::EvmAddress = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        .parse()
        .unwrap();
    let word = encode_address(address);
    assert_eq!(word[..12], [0u8; 12], "high 12 bytes must be zero padding");
    assert_eq!(&word[12..], address.as_bytes());
    assert_eq!(
        encode_lower(&word),
        "0x0000000000000000000000005aaeb6053f3e94c9b9a09f33669435e7ef1beaed"
    );
    assert_eq!(encode_address(crate::evm::EvmAddress::ZERO), [0u8; 32]);
}

#[test]
fn encode_uint256_is_the_word_itself() {
    assert_eq!(encode_uint256(EvmU256::ZERO), [0u8; 32]);
    assert_eq!(encode_uint256(EvmU256::MAX), [0xffu8; 32]);
    assert_eq!(
        encode_lower(&encode_uint256(EvmU256::ONE)),
        "0x0000000000000000000000000000000000000000000000000000000000000001"
    );
}

#[test]
fn encode_uint128_left_pads_into_a_word() {
    assert_eq!(encode_uint128(0), [0u8; 32]);
    assert_eq!(encode_uint128(1), encode_uint256(EvmU256::ONE));
    assert_eq!(
        encode_uint128(u128::MAX),
        encode_uint256(EvmU256::from_u128(u128::MAX))
    );
    // 20,000 GLC at 18 decimals, the amount shape this will really carry.
    let amount = 20_000_000_000_000_000_000_000u128;
    assert_eq!(
        encode_uint128(amount),
        EvmU256::from_u128(amount).to_be_bytes()
    );
}

#[test]
fn encode_bool_is_zero_or_one_in_the_last_byte() {
    assert_eq!(encode_bool(false), [0u8; 32]);
    let mut expected_true = [0u8; 32];
    expected_true[31] = 1;
    assert_eq!(encode_bool(true), expected_true);
    assert_eq!(encode_bool(true), encode_uint128(1));
}

#[test]
fn encode_string_and_bytes_hash_their_contents() {
    assert_eq!(encode_string(""), crate::evm::keccak256(b""));
    assert_eq!(
        encode_string("Ether Mail"),
        crate::evm::keccak256(b"Ether Mail")
    );
    assert_eq!(encode_bytes(b"abc"), crate::evm::keccak256(b"abc"));
    assert_eq!(encode_string("abc"), encode_bytes(b"abc"));
    // Different contents, different word — and a string is not padded, so
    // trailing whitespace is a different value.
    assert_ne!(encode_string("abc"), encode_string("abc "));
}

#[test]
fn a_worked_struct_hash_composes_from_the_encoders() {
    // Demonstrates the whole intended pattern for a later phase, without
    // committing to any actual message: declare a typeHash, encode the
    // fields in the declared order, concatenate, hash. No policy here — the
    // "message" is deliberately a made-up shape used only by this test.
    let type_hash = crate::evm::keccak256(b"Example(address to,uint256 amount,bool flag)");
    let to: crate::evm::EvmAddress = "0x5aAeb6053F3E94C9b9A09f33669435E7Ef1BeAed"
        .parse()
        .unwrap();

    let struct_hash = crate::evm::keccak::keccak256_concat(&[
        &type_hash,
        &encode_address(to),
        &encode_uint128(1_000_000_000_000_000_000),
        &encode_bool(true),
    ]);

    let digest = typed_data_hash(&ether_mail_domain().separator(), &struct_hash);
    // Deterministic, 32 bytes, and distinct from the same fields in a
    // different order — which is the property that matters.
    assert_eq!(digest.len(), 32);
    let reordered = crate::evm::keccak::keccak256_concat(&[
        &type_hash,
        &encode_uint128(1_000_000_000_000_000_000),
        &encode_address(to),
        &encode_bool(true),
    ]);
    assert_ne!(struct_hash, reordered);
}

// --- The domain is a plain description -------------------------------

#[test]
fn default_and_new_agree_and_are_empty() {
    assert_eq!(Eip712Domain::new(), Eip712Domain::default());
    let empty = Eip712Domain::new();
    assert!(empty.name.is_none());
    assert!(empty.version.is_none());
    assert!(empty.chain_id.is_none());
    assert!(empty.verifying_contract.is_none());
    assert!(empty.salt.is_none());
}
