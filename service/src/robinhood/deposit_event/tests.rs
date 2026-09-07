//! The decoder's contract, stated as tests.
//!
//! Everything here is deterministic: logs are built by encoding the event
//! the way the contract would, then handed to the production decoder. No
//! network, no fixtures pasted from a block explorer.

use super::*;
use crate::robinhood::testkit::{
    block_hash, deposit_log, encode_deposit_data, tx_hash, DepositParams, BRIDGE, CANONICAL_SCALE,
    DEPOSITOR,
};
use crate::routes::Route;

/// topic0, pinned against a value produced by an independent keccak-256
/// implementation over the same signature string.
///
/// This is the test that catches the SIGNATURE being edited — the
/// constant itself is computed at runtime precisely so it can never be a
/// stale paste, which leaves "did somebody change the event?" as the
/// question this pin answers.
#[test]
fn topic0_matches_the_deployed_event_signature() {
    assert_eq!(
        DEPOSIT_CREATED_SIGNATURE,
        "DepositCreated(uint256,address,uint8,uint256,uint256,bytes)"
    );
    assert_eq!(
        crate::evm::hex::encode_lower(&deposit_created_topic0()),
        "0xda95cbec0e8506ff53e79872faa19696b138e68538e0228071f9675305a9b537"
    );
}

#[test]
fn decodes_a_well_formed_inbound_deposit() {
    let params = DepositParams::valid(7, 100, 250);
    let event = decode_deposit_created(&deposit_log(&params)).expect("decodes");

    assert_eq!(event.obligation_index, 7);
    assert_eq!(event.route, Route::RhnToGlc);
    assert_eq!(event.contract_route_id, 0x02);
    assert_eq!(event.depositor, DEPOSITOR);
    assert_eq!(event.destination, vec![0xde, 0xad, 0xbe, 0xef]);
    assert_eq!(event.canonical_amount.0, 250);
    assert_eq!(event.amount.get(), 250 * CANONICAL_SCALE);
    assert_eq!(event.contract, BRIDGE);
    assert_eq!(event.block_number(), 100);
    assert_eq!(event.block_hash().to_bytes(), block_hash(100, 0));
    assert_eq!(event.log_id().tx_hash.to_bytes(), tx_hash(7));
    assert_eq!(event.log_id().log_index, 0);
}

#[test]
fn accepts_both_inbound_routes_and_only_those() {
    for (route, id) in [(Route::RhnToGlc, 0x02u8), (Route::RhnToSol, 0x04)] {
        let mut params = DepositParams::valid(1, 10, 5);
        params.contract_route_id = id;
        let event = decode_deposit_created(&deposit_log(&params)).expect("inbound route decodes");
        assert_eq!(event.route, route);
    }
}

/// The refusal the whole "disabled routes stay disabled" story rests on:
/// an outbound route in a deposit event is not a deposit to skip, it is
/// evidence the configured address is not the expected contract.
#[test]
fn rejects_outbound_and_unknown_routes() {
    for id in [0x00u8, 0x01, 0x03, 0x05, 0xff] {
        let mut params = DepositParams::valid(1, 10, 5);
        params.contract_route_id = id;
        assert_eq!(
            decode_deposit_created(&deposit_log(&params)),
            Err(DepositDecodeError::NotAnInboundRoute { route: id }),
            "route {id:#04x} must be refused",
        );
    }
}

/// The wire mapping has exactly one source of truth, and this is the
/// assertion that keeps the decoder's view of it aligned with the routes
/// module's.
#[test]
fn inbound_route_mapping_agrees_with_the_route_registry() {
    for route in Route::ALL {
        let id = route.contract_route_id();
        match route {
            Route::RhnToGlc | Route::RhnToSol => {
                assert_eq!(inbound_route_from_contract_id(id.unwrap()), Some(route));
            }
            Route::GlcToRhn | Route::SolToRhn => {
                assert_eq!(inbound_route_from_contract_id(id.unwrap()), None);
            }
            // Solana<->Goldcoin routes have no contract discriminator at
            // all, so there is nothing for this mapping to answer.
            Route::GlcToSol | Route::SolToGlc => assert_eq!(id, None),
        }
    }
}

#[test]
fn refuses_a_log_with_a_different_topic0() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.topics[0] = [0x99; 32];
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::NotDepositCreated)
    );
}

#[test]
fn refuses_a_wrong_topic_count() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.topics.pop();
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::WrongTopicCount { found: 3 })
    );
}

/// A dirty pad means the word is not the left-padded value it claims to
/// be. Guessing at which 20 (or 1) bytes were meant is exactly what a
/// decoder must not do.
#[test]
fn refuses_dirty_topic_padding() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.topics[2][0] = 0x01;
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::DirtyTopicPadding {
            topic: 2,
            kind: "address",
            leading: 12,
        })
    );

    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.topics[3][0] = 0x01;
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::DirtyTopicPadding {
            topic: 3,
            kind: "uint8",
            leading: 31,
        })
    );
}

#[test]
fn refuses_an_obligation_index_wider_than_a_counter_can_be() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.topics[1] = [0xff; 32];
    assert!(matches!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::ObligationIndexTooLarge { .. })
    ));
}

/// Following a hand-built pointer would let one blob be read as several
/// different destinations.
#[test]
fn refuses_a_tail_offset_other_than_0x60() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.data[95] = 0x80;
    assert!(matches!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::UnexpectedTailOffset { expected: 96, .. })
    ));
}

#[test]
fn refuses_a_destination_length_outside_the_contracts_bound() {
    for len in [0usize, MAX_DESTINATION_LEN + 1] {
        let mut params = DepositParams::valid(1, 10, 5);
        params.destination = vec![0x01; len];
        let mut log = deposit_log(&params);
        // `encode_deposit_data` is honest, so for the empty case the blob
        // is already right; for the over-long case it is too, and the
        // length word is what the decoder refuses on.
        log.data = encode_deposit_data(5 * CANONICAL_SCALE, 5, &vec![0x01; len]);
        assert_eq!(
            decode_deposit_created(&log),
            Err(DepositDecodeError::DestinationLengthOutOfRange {
                found: len,
                max: MAX_DESTINATION_LEN,
            }),
            "destination length {len} must be refused",
        );
    }
}

/// Trailing bytes beyond the encoding are something the decoder is not
/// reading. Ignoring them would let one log mean two things.
#[test]
fn refuses_trailing_bytes_beyond_the_encoding() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    let before = log.data.len();
    log.data.extend_from_slice(&[0u8; 32]);
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::WrongDataLength {
            expected: before,
            found: before + 32,
            destination_len: 4,
        })
    );
}

#[test]
fn refuses_dirty_destination_padding() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    let last = log.data.len() - 1;
    log.data[last] = 0x01;
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::DirtyDestinationPadding)
    );
}

#[test]
fn refuses_data_too_short_for_its_head() {
    let mut log = deposit_log(&DepositParams::valid(1, 10, 5));
    log.data.truncate(64);
    assert_eq!(
        decode_deposit_created(&log),
        Err(DepositDecodeError::DataTooShort { found: 64 })
    );
}

#[test]
fn refuses_a_zero_amount() {
    let mut params = DepositParams::valid(1, 10, 5);
    params.amount = 0;
    params.canonical_amount = Some(0);
    assert_eq!(
        decode_deposit_created(&deposit_log(&params)),
        Err(DepositDecodeError::ZeroAmount)
    );
}

/// The contract emits `amount / CANONICAL_SCALE`, so the two words are
/// two independent witnesses to one number. A single corrupted word must
/// not be able to move the amount that gets recorded.
#[test]
fn refuses_a_canonical_amount_that_disagrees_with_the_raw_amount() {
    let mut params = DepositParams::valid(1, 10, 5);
    params.canonical_amount = Some(4);
    assert!(matches!(
        decode_deposit_created(&deposit_log(&params)),
        Err(DepositDecodeError::CanonicalAmountMismatch { .. })
    ));
}

/// An amount that is not a whole multiple of the scale cannot be
/// represented in the ledger's canonical unit, and the contract's
/// `_requireCanonicalAmount` means it cannot occur — so it is refused
/// rather than rounded.
#[test]
fn refuses_an_amount_that_is_not_a_whole_canonical_multiple() {
    let mut params = DepositParams::valid(1, 10, 5);
    params.amount = 5 * CANONICAL_SCALE + 1;
    params.canonical_amount = Some(5);
    assert!(matches!(
        decode_deposit_created(&deposit_log(&params)),
        Err(DepositDecodeError::CanonicalAmountMismatch { .. })
    ));
}

/// Round trip over the full legal destination-length range, including
/// both padding boundaries (32 and 64 bytes need no padding; 33 would if
/// it were legal, and 31 does).
#[test]
fn round_trips_every_legal_destination_length() {
    for len in 1..=MAX_DESTINATION_LEN {
        let mut params = DepositParams::valid(1, 10, 5);
        params.destination = (0..len).map(|i| (i as u8).wrapping_add(1)).collect();
        let event = decode_deposit_created(&deposit_log(&params))
            .unwrap_or_else(|e| panic!("length {len} must decode: {e}"));
        assert_eq!(event.destination, params.destination);
    }
}
