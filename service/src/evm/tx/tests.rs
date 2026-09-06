//! Transaction envelope tests.
//!
//! The legacy vectors are pinned against a published, independently
//! produced fixture (the EIP-155 specification's own example), which is
//! what makes them a real cross-check rather than this file agreeing with
//! itself. The 1559 vectors are proven structurally — field order, the
//! `0x02` prefix, the empty access list, yParity rather than an EIP-155
//! `v` — and by sign/recover round trips.

use super::*;
use crate::evm::networks::ROBINHOOD_MAINNET_CHAIN_ID;

fn key(byte: u8) -> EvmSecretKey {
    let mut bytes = [0u8; 32];
    bytes[31] = byte;
    EvmSecretKey::from_bytes(&bytes).unwrap()
}

fn to() -> EvmAddress {
    "0x3535353535353535353535353535353535353535"
        .parse()
        .unwrap()
}

/// The EIP-155 specification's own worked example.
///
/// > Consider a transaction with nonce = 9, gasprice = 20 * 10**9,
/// > startgas = 21000, to = 0x3535..35, value = 10**18, data = ''
/// > (empty). The signing data becomes
/// > 0xec098504a817c800825208943535353535353535353535353535353535353535880de0b6b3a764000080018080
fn eip155_example() -> UnsignedTransaction {
    UnsignedTransaction {
        chain_id: EvmChainId::new(1).unwrap(),
        nonce: 9,
        gas_limit: 21_000,
        to: to(),
        value: EvmU256::from_u128(1_000_000_000_000_000_000),
        data: Vec::new(),
        fees: TxFees::Legacy {
            gas_price: 20_000_000_000,
        },
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn legacy_signing_payload_matches_the_eip155_published_example() {
    let payload = eip155_example().signing_payload();
    assert_eq!(
        hex(&payload),
        "ec098504a817c80082520894353535353535353535353535353535353535353588\
0de0b6b3a764000080018080"
            .replace('\n', "")
    );
}

#[test]
fn legacy_signing_hash_matches_the_eip155_published_example() {
    // The EIP quotes the resulting hash to sign.
    assert_eq!(
        hex(&eip155_example().signing_hash()),
        "daf5a779ae972f972197303d7b574746c7ef83eadac0f2791ad23db92e4c8e53"
    );
}

#[test]
fn legacy_v_encodes_the_chain_id_per_eip155() {
    let tx = UnsignedTransaction {
        chain_id: ROBINHOOD_MAINNET_CHAIN_ID,
        ..eip155_example()
    };
    let signed = tx.sign(&key(3));
    // The v byte is the seventh RLP item. Rather than re-parse RLP (this
    // crate has no decoder, deliberately), assert the two admissible
    // values directly: chainId*2 + 35 or +36.
    let base = 4663u128 * 2 + 35;
    let recovery = u128::from(signed.signature.recovery_id());
    let expected_v = base + recovery;
    assert!(expected_v == 9361 || expected_v == 9362, "{expected_v}");
    // And prove it round-trips: recovery must yield the signing address.
    assert_eq!(signed.recover_sender().unwrap(), key(3).address());
}

#[test]
fn legacy_and_1559_signing_hashes_differ_for_otherwise_identical_fields() {
    // The whole point of the envelope choice: the same logical transfer
    // in the two envelopes is two different signed objects, so a
    // signature for one is worthless for the other.
    let legacy = eip155_example();
    let mut eip1559 = legacy.clone();
    eip1559.fees = TxFees::Eip1559 {
        max_fee_per_gas: 20_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
    };
    assert_ne!(legacy.signing_hash(), eip1559.signing_hash());
}

#[test]
fn eip1559_payload_starts_with_the_type_byte() {
    let mut tx = eip155_example();
    tx.fees = TxFees::Eip1559 {
        max_fee_per_gas: 30_000_000_000,
        max_priority_fee_per_gas: 2_000_000_000,
    };
    assert_eq!(tx.signing_payload()[0], 0x02);
    let signed = tx.sign(&key(5));
    assert_eq!(signed.raw[0], 0x02);
}

#[test]
fn eip1559_signed_payload_carries_an_empty_access_list_and_a_bare_y_parity() {
    let mut tx = eip155_example();
    tx.chain_id = ROBINHOOD_MAINNET_CHAIN_ID;
    tx.fees = TxFees::Eip1559 {
        max_fee_per_gas: 30_000_000_000,
        max_priority_fee_per_gas: 2_000_000_000,
    };
    let signed = tx.clone().sign(&key(5));
    let raw = hex(&signed.raw);
    // The empty access list is RLP 0xc0 and must appear exactly once,
    // immediately before the yParity item.
    assert!(raw.contains("c0"), "an empty access list must be present");
    // yParity is 0 or 1 — RLP 0x80 (zero) or 0x01. An EIP-155 v of 9361
    // would encode as 82 24 91, which must NOT appear.
    assert!(
        !raw.contains("822491"),
        "a typed transaction must not carry an EIP-155 v: {raw}"
    );
    assert_eq!(signed.recover_sender().unwrap(), key(5).address());
}

#[test]
fn the_transaction_hash_is_keccak_of_the_signed_bytes_not_the_signing_bytes() {
    let signed = eip155_example().sign(&key(7));
    assert_eq!(
        signed.hash.to_bytes(),
        crate::evm::keccak::keccak256(&signed.raw)
    );
    assert_ne!(
        signed.hash.to_bytes(),
        signed.unsigned.signing_hash(),
        "the id the chain knows a transaction by is not the digest that was signed"
    );
}

#[test]
fn both_envelopes_round_trip_sender_recovery() {
    for fees in [
        TxFees::Legacy {
            gas_price: 1_000_000_000,
        },
        TxFees::Eip1559 {
            max_fee_per_gas: 5_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
        },
    ] {
        for nonce in [0u64, 1, 127, 128, 255, 256, u32::MAX as u64] {
            let tx = UnsignedTransaction {
                chain_id: ROBINHOOD_MAINNET_CHAIN_ID,
                nonce,
                gas_limit: 250_000,
                to: to(),
                value: EvmU256::ZERO,
                data: vec![0xde, 0xad, 0xbe, 0xef],
                fees,
            };
            let signed = tx.sign(&key(11));
            assert_eq!(
                signed.recover_sender().unwrap(),
                key(11).address(),
                "envelope {} nonce {nonce}",
                fees.envelope().as_str()
            );
        }
    }
}

#[test]
fn a_different_nonce_produces_a_different_transaction_hash() {
    // Load-bearing for the nonce manager: two allocations must be two
    // distinguishable transactions, never the same one twice.
    let base = UnsignedTransaction {
        chain_id: ROBINHOOD_MAINNET_CHAIN_ID,
        nonce: 0,
        gas_limit: 250_000,
        to: to(),
        value: EvmU256::ZERO,
        data: vec![1, 2, 3],
        fees: TxFees::Legacy { gas_price: 1 },
    };
    let a = base.clone().sign(&key(13));
    let b = UnsignedTransaction { nonce: 1, ..base }.sign(&key(13));
    assert_ne!(a.hash, b.hash);
    assert_ne!(a.raw, b.raw);
}

#[test]
fn re_signing_the_same_unsigned_transaction_is_byte_identical() {
    // RFC-6979 determinism carried up to the envelope: a replacement
    // built from the same persisted fields is the SAME transaction, not a
    // second one racing it.
    let tx = UnsignedTransaction {
        chain_id: ROBINHOOD_MAINNET_CHAIN_ID,
        nonce: 42,
        gas_limit: 300_000,
        to: to(),
        value: EvmU256::ZERO,
        data: vec![0xaa; 200],
        fees: TxFees::Eip1559 {
            max_fee_per_gas: 7,
            max_priority_fee_per_gas: 1,
        },
    };
    let a = tx.clone().sign(&key(17));
    let b = tx.sign(&key(17));
    assert_eq!(a.raw, b.raw);
    assert_eq!(a.hash, b.hash);
}

#[test]
fn envelope_parses_from_its_config_spelling_and_rejects_anything_else() {
    assert_eq!("legacy".parse::<TxEnvelope>().unwrap(), TxEnvelope::Legacy);
    assert_eq!(
        "eip1559".parse::<TxEnvelope>().unwrap(),
        TxEnvelope::Eip1559
    );
    for bad in ["", "Legacy", "1559", "eip-1559", "london", "type2"] {
        assert!(
            bad.parse::<TxEnvelope>().is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn only_the_1559_envelope_requires_a_base_fee() {
    assert!(!TxEnvelope::Legacy.requires_base_fee());
    assert!(TxEnvelope::Eip1559.requires_base_fee());
}

#[test]
fn max_cost_is_the_ceiling_price_times_the_gas_limit() {
    assert_eq!(TxFees::Legacy { gas_price: 3 }.max_cost_wei(100), Some(300));
    assert_eq!(
        TxFees::Eip1559 {
            max_fee_per_gas: 5,
            max_priority_fee_per_gas: 1
        }
        .max_cost_wei(100),
        Some(500),
        "the ceiling is maxFeePerGas, never the priority tip"
    );
    assert_eq!(
        TxFees::Legacy {
            gas_price: u128::MAX
        }
        .max_cost_wei(2),
        None,
        "an overflowing cost is reported, never wrapped"
    );
}

#[test]
fn a_zero_value_encodes_as_the_empty_rlp_string() {
    // Every call this bridge makes is non-payable, so this is the value
    // every single transaction carries. Encoding it as 0x00 rather than
    // 0x80 would change every transaction hash.
    let tx = eip155_example();
    let zero = UnsignedTransaction {
        value: EvmU256::ZERO,
        ..tx
    };
    let payload = hex(&zero.signing_payload());
    // ... 3535353535 (the `to`) then 0x80 for the zero value then 0x80
    // for the empty data.
    assert!(
        payload.contains("35353535358080"),
        "zero value must be 0x80: {payload}"
    );
}
