//! Transport-layer parsing and classification. No socket is opened: every
//! test drives the same pure functions the live client uses on the bodies
//! a node would return.

use super::*;
use serde_json::json;

#[test]
fn only_transport_errors_are_retriable() {
    assert!(EvmRpcError::Transport("connection refused".into()).is_retriable());
    assert!(!EvmRpcError::Method {
        code: -32000,
        message: "filter too wide".into()
    }
    .is_retriable());
    assert!(!EvmRpcError::Malformed("nonsense".into()).is_retriable());
}

#[test]
fn extracts_the_result_member() {
    let value =
        parse_rpc_response(200, r#"{"jsonrpc":"2.0","id":1,"result":"0x1234"}"#).expect("parses");
    assert_eq!(value.as_str(), Some("0x1234"));
}

/// A null result is meaningful (`eth_getBlockByNumber` on an unknown
/// height) and must reach the caller; an ABSENT result is a broken
/// envelope and must not be confused with it.
#[test]
fn distinguishes_a_null_result_from_a_missing_one() {
    let value =
        parse_rpc_response(200, r#"{"jsonrpc":"2.0","id":1,"result":null}"#).expect("parses");
    assert!(value.is_null());

    assert!(matches!(
        parse_rpc_response(200, r#"{"jsonrpc":"2.0","id":1}"#),
        Err(EvmRpcError::Malformed(_))
    ));
}

/// Many endpoints return a perfectly good JSON-RPC error under HTTP 400.
/// Classifying that as transport would make a permanent refusal retriable
/// forever.
#[test]
fn a_json_rpc_error_under_a_non_2xx_status_is_still_definitive() {
    let error = parse_rpc_response(
        400,
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"query returned more than 10000 results"}}"#,
    )
    .expect_err("is an error");
    assert!(matches!(error, EvmRpcError::Method { code: -32005, .. }));
    assert!(!error.is_retriable());
}

#[test]
fn a_non_json_body_under_a_non_2xx_status_is_transport() {
    let error = parse_rpc_response(502, "<html>bad gateway</html>").expect_err("is an error");
    assert!(matches!(error, EvmRpcError::Transport(_)));
    assert!(error.is_retriable());
}

/// A 200 with a body that is not JSON is the endpoint answering
/// nonsense, not a network problem — retrying it forever would hide a
/// misconfigured URL.
#[test]
fn a_non_json_body_under_200_is_malformed_not_transport() {
    let error = parse_rpc_response(200, "not json").expect_err("is an error");
    assert!(matches!(error, EvmRpcError::Malformed(_)));
    assert!(!error.is_retriable());
}

#[test]
fn an_error_object_without_a_code_is_malformed() {
    assert!(matches!(
        parse_rpc_response(200, r#"{"error":{"message":"boom"}}"#),
        Err(EvmRpcError::Malformed(_))
    ));
}

#[test]
fn decodes_a_block_header() {
    let block = decode_block_ref(&json!({
        "number": "0x64",
        "hash": format!("0x{}", "11".repeat(32)),
        "parentHash": format!("0x{}", "22".repeat(32)),
        "timestamp": "0x65000000",
    }))
    .expect("decodes");
    assert_eq!(block.number, 100);
    assert_eq!(block.hash.to_bytes(), [0x11; 32]);
    assert_eq!(block.parent_hash.to_bytes(), [0x22; 32]);
}

#[test]
fn refuses_a_header_with_a_missing_or_short_field() {
    assert!(matches!(
        decode_block_ref(&json!({
            "number": "0x64",
            "parentHash": format!("0x{}", "22".repeat(32)),
            "timestamp": "0x1",
        })),
        Err(EvmRpcError::Malformed(_))
    ));
    // A hash that is not exactly 32 bytes is never padded to reach it.
    assert!(matches!(
        decode_block_ref(&json!({
            "number": "0x64",
            "hash": "0x1111",
            "parentHash": format!("0x{}", "22".repeat(32)),
            "timestamp": "0x1",
        })),
        Err(EvmRpcError::Malformed(_))
    ));
}

fn log_json() -> serde_json::Value {
    json!({
        "address": format!("0x{}", "11".repeat(20)),
        "topics": [format!("0x{}", "aa".repeat(32))],
        "data": "0x1234",
        "blockNumber": "0x10",
        "blockHash": format!("0x{}", "bb".repeat(32)),
        "transactionHash": format!("0x{}", "cc".repeat(32)),
        "logIndex": "0x3",
    })
}

#[test]
fn decodes_a_log() {
    let log = decode_raw_log(&log_json()).expect("decodes");
    assert_eq!(log.address.to_bytes(), [0x11; 20]);
    assert_eq!(log.topics, vec![[0xaa; 32]]);
    assert_eq!(log.data, vec![0x12, 0x34]);
    assert_eq!(log.block_number, 16);
    assert_eq!(log.log_index, 3);
    // Absent `removed` means a canonical log.
    assert!(!log.removed);
}

/// A pending log has null block fields. This client only ever asks for
/// closed ranges, so one appearing in the answer means the response does
/// not describe what was asked for.
#[test]
fn refuses_a_pending_log() {
    let mut value = log_json();
    value["blockHash"] = serde_json::Value::Null;
    assert!(matches!(
        decode_raw_log(&value),
        Err(EvmRpcError::Malformed(_))
    ));
}

#[test]
fn refuses_a_log_topic_that_is_not_a_32_byte_word() {
    let mut value = log_json();
    value["topics"] = json!(["0xaabb"]);
    assert!(matches!(
        decode_raw_log(&value),
        Err(EvmRpcError::Malformed(_))
    ));
}

#[tokio::test]
async fn call_with_retry_retries_transport_and_gives_up_on_method_errors() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let attempts = AtomicU32::new(0);
    let result: Result<u32, EvmRpcError> = call_with_retry(5, || {
        let n = attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if n < 2 {
                Err(EvmRpcError::Transport("connection refused".into()))
            } else {
                Ok(n)
            }
        }
    })
    .await;
    assert_eq!(result.expect("eventually succeeds"), 2);

    let attempts = AtomicU32::new(0);
    let result: Result<u32, EvmRpcError> = call_with_retry(5, || {
        attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            Err(EvmRpcError::Method {
                code: -32601,
                message: "method not found".into(),
            })
        }
    })
    .await;
    assert!(result.is_err());
    // Tried exactly once: a definitive answer is not retried.
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// Phase F gave this client the ability to broadcast, so the Phase-E
/// guarantee ("this type cannot send") is gone and is not coming back.
/// What replaces it is narrower and equally structural: the client must
/// never ask the NODE to hold a key and sign on this service's behalf.
///
/// Those methods — `eth_sendTransaction`, `eth_sign`,
/// `eth_signTransaction`, `personal_*` — describe a custody arrangement
/// this bridge does not have. Every transaction it sends is signed
/// in-process or by a remote signer and handed over already signed.
///
/// Asserted against the source text because the guarantee is the ABSENCE
/// of code, which no runtime check can observe.
#[test]
fn the_client_never_asks_the_node_to_sign_on_its_behalf() {
    let source = include_str!("../rpc.rs");
    for forbidden in [
        "eth_sendTransaction",
        "eth_sign",
        "eth_signTransaction",
        "personal_sendTransaction",
        "personal_sign",
        "personal_unlockAccount",
        "eth_accounts",
    ] {
        // The module docs name some of these to explain their absence, so
        // matching is restricted to a JSON-RPC method position: a quoted
        // string, which is the only way one could actually be invoked.
        assert!(
            !source.contains(&format!("\"{forbidden}\"")),
            "{forbidden} must not be callable from this client",
        );
    }
}

/// The Phase-E property, preserved where it still applies: the DEPOSIT
/// INDEXER is generic over [`EvmRpc`] alone, and that trait has no
/// broadcast method. Observing deposits therefore still cannot send a
/// transaction — not by convention, but because the trait the indexer is
/// bound by does not have the method.
#[test]
fn the_observation_trait_still_cannot_broadcast_or_read_contract_state() {
    let source = include_str!("../rpc.rs");
    let trait_start = source
        .find("pub trait EvmRpc {")
        .expect("the observation trait must exist");
    let trait_end = source[trait_start..]
        .find("\n}")
        .map(|offset| trait_start + offset)
        .expect("the observation trait must be closed");
    let body = &source[trait_start..trait_end];
    for forbidden in [
        "send_raw_transaction",
        "pending_nonce",
        "estimate_gas",
        "transaction_receipt",
        "call",
        "code_at",
    ] {
        assert!(
            !body.contains(forbidden),
            "EvmRpc must not gain {forbidden}: the deposit indexer is bound by this trait, and \
             widening it would silently give an observation-only component the ability to act",
        );
    }
}
