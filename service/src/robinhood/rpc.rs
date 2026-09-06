//! A minimal Ethereum JSON-RPC client, read-only by construction.
//!
//! # The whole method surface
//!
//! Four methods, and the list is the security property:
//!
//! | Method | Why it is needed |
//! |---|---|
//! | `eth_chainId` | Prove the endpoint is the network the operator configured, every tick |
//! | `eth_blockNumber` | The head, from which finality depth is measured |
//! | `eth_getBlockByNumber` | A block's hash, for the scan anchor and the reorg walk |
//! | `eth_getLogs` | The deposits themselves |
//!
//! There is deliberately no `eth_sendRawTransaction`, no
//! `eth_sendTransaction`, no `eth_sign`, no `personal_*`, and no
//! `eth_call`. Not feature-gated, not behind a config flag — absent. This
//! client cannot broadcast, cannot sign, and cannot even execute a
//! read-only contract call. Adding a payout leg means adding methods
//! here, which is a visible, reviewable diff rather than a flag flip.
//!
//! `eth_getTransactionReceipt` is also absent, and that is worth stating
//! explicitly because it is the method an EVM indexer usually reaches
//! for. It is not needed: `eth_getLogs` already returns, per log, the
//! transaction hash, the log index, the block number AND the block hash —
//! every component of [`crate::evm::EvmLogLocation`]. Fetching the
//! receipt would re-derive facts already in hand, at one extra round trip
//! per deposit, and would introduce a second source for values that must
//! agree.
//!
//! # Error classification, and why it is copied rather than invented
//!
//! Exactly the split [`crate::goldcoin::rpc::RpcError`] draws, for
//! exactly the same reason:
//!
//! - [`EvmRpcError::Transport`] — could not reach the node, or could not
//!   read its answer. The question was never answered, so asking again is
//!   meaningful: retried with backoff.
//! - [`EvmRpcError::Method`] / [`EvmRpcError::Malformed`] — the node
//!   answered, and the answer was an error or was not the shape it must
//!   be. Never retried. Retrying a definitive answer converts a real bug
//!   or a real disagreement about chain state into a silent stall.
//!
//! Unknown chain state is never treated as success anywhere below: a
//! missing block is `None` for the caller to decide about, never an
//! assumed value; a malformed field is an error, never a default.
//!
//! # Why this is hand-rolled rather than an EVM SDK
//!
//! The same judgement `goldcoin::rpc` records for Goldcoin Core, and the
//! same one the service already made by hand-building Solana
//! instructions: four methods over `reqwest` is a few hundred lines with
//! no new dependency, while an EVM client library brings a large
//! transitive graph — including transaction construction and signing —
//! into a process whose entire safety story here is that it cannot
//! construct or sign a transaction. The dependency that lets you sign is
//! a strictly worse fit than the one that cannot.

use std::future::Future;

use serde_json::{json, Value};
use thiserror::Error;

use crate::evm::hash::{EvmBlockHash, EvmTxHash};
use crate::evm::quantity::{self, encode_quantity_u64};
use crate::evm::{EvmAddress, EvmChainId};

/// A 32-byte log topic, exactly as it sits on the wire. Deliberately raw
/// bytes and not [`crate::evm::EvmU256`]: a topic is a slot that may hold
/// a hash, a left-padded address, or a left-padded integer, and giving it
/// a numeric type at the transport layer would invite arithmetic on
/// something that is not always a number. The decoder assigns meaning.
pub type EvmTopic = [u8; 32];

#[derive(Debug, Error)]
pub enum EvmRpcError {
    #[error("transport error contacting the Robinhood EVM RPC: {0}")]
    Transport(String),
    #[error("Robinhood EVM RPC method error (code {code}): {message}")]
    Method { code: i64, message: String },
    #[error("malformed Robinhood EVM JSON-RPC response: {0}")]
    Malformed(String),
}

impl EvmRpcError {
    /// Only [`EvmRpcError::Transport`] is meaningfully retriable — see
    /// this module's docs.
    pub fn is_retriable(&self) -> bool {
        matches!(self, EvmRpcError::Transport(_))
    }
}

/// A block's identity, which is all this service needs from a header.
///
/// Deliberately not the full block: no transactions, no gas fields, no
/// state roots. The reorg walk asks one question — "is the block I
/// anchored on still the block at that height?" — and the answer is the
/// hash. Decoding fields nothing reads would only widen what a malformed
/// response can influence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvmBlockRef {
    pub number: u64,
    pub hash: EvmBlockHash,
    /// Carried because it is what makes a hash chain a chain: it is the
    /// reason a single matching anchor certifies an entire ancestry.
    pub parent_hash: EvmBlockHash,
    pub timestamp: u64,
}

/// One log, decoded only as far as the transport layer can honestly go.
///
/// `topics` and `data` are left as raw bytes: giving them meaning is the
/// decoder's job ([`super::deposit_event`]), and a transport type that
/// guessed at an ABI would be a second, drifting copy of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmRawLog {
    pub address: EvmAddress,
    pub topics: Vec<EvmTopic>,
    pub data: Vec<u8>,
    pub block_number: u64,
    pub block_hash: EvmBlockHash,
    pub tx_hash: EvmTxHash,
    /// Index within the BLOCK, not within the transaction — which is what
    /// `eth_getLogs` reports and what makes `(tx_hash, log_index)` unique.
    /// See [`crate::evm::EvmLogId`].
    pub log_index: u64,
    /// The node's own statement that this log is no longer canonical.
    /// Always `false` for a range query against a healthy node; see
    /// [`super::indexer`] for why a `true` is skipped rather than
    /// recorded, and why skipping it is safe.
    pub removed: bool,
}

/// An `eth_getLogs` filter. Always address-scoped and always topic0-
/// scoped: an unfiltered range query against a busy chain returns
/// everything, and a client that then filters in memory has already paid
/// for — and must then parse — logs it had no business receiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmLogFilter {
    pub from_block: u64,
    pub to_block: u64,
    pub address: EvmAddress,
    pub topic0: EvmTopic,
}

/// The exact RPC surface [`super::indexer::RobinhoodIndexer`] needs.
///
/// A trait for the same reason [`crate::goldcoin::indexer::GoldcoinRpc`]
/// is one: the tick loop's reorg, finality and idempotency logic is the
/// part that must be right, and it has to be exercisable against
/// deterministic, adversarial responses that no live endpoint would
/// produce on demand. Phase E ships with mock implementations only — no
/// test in this crate contacts a real Robinhood node.
pub trait EvmRpc {
    fn chain_id(&self) -> impl Future<Output = Result<EvmChainId, EvmRpcError>> + Send;
    fn block_number(&self) -> impl Future<Output = Result<u64, EvmRpcError>> + Send;
    /// `None` when the node reports no block at that height — never
    /// substituted with a guess, and never conflated with a transport
    /// failure.
    fn block_by_number(
        &self,
        number: u64,
    ) -> impl Future<Output = Result<Option<EvmBlockRef>, EvmRpcError>> + Send;
    fn logs(
        &self,
        filter: &EvmLogFilter,
    ) -> impl Future<Output = Result<Vec<EvmRawLog>, EvmRpcError>> + Send;
}

#[derive(Debug, Clone)]
pub struct EvmRpcConfig {
    pub url: String,
    pub connect_timeout_ms: u64,
    pub read_timeout_ms: u64,
}

/// The real client. Holds an HTTP client and a URL, and nothing else —
/// no key, no nonce, no signer, no account.
pub struct EvmRpcClient {
    http: reqwest::Client,
    url: String,
}

impl EvmRpcClient {
    pub fn new(cfg: &EvmRpcConfig) -> Result<Self, EvmRpcError> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(cfg.connect_timeout_ms))
            .timeout(std::time::Duration::from_millis(cfg.read_timeout_ms))
            .build()
            .map_err(|e| EvmRpcError::Transport(e.to_string()))?;
        Ok(EvmRpcClient {
            http,
            url: cfg.url.clone(),
        })
    }

    /// One JSON-RPC 2.0 round trip.
    ///
    /// Splits "could not reach or read from the endpoint" from "reached
    /// it, but the body is not JSON" the same way `goldcoin::rpc::
    /// RpcClient::call` does, and for the same diagnostic reason recorded
    /// there: `reqwest`'s own `json()` collapses both into one opaque
    /// message whose real cause is only reachable through `source()`.
    async fn call(&self, method: &str, params: Value) -> Result<Value, EvmRpcError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": "glc-reserve-bridge",
            "method": method,
            "params": params,
        });
        let response = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| EvmRpcError::Transport(format!("request failed: {e}")))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| {
            EvmRpcError::Transport(format!("failed reading response body (HTTP {status}): {e}"))
        })?;
        parse_rpc_response(status.as_u16(), &text)
    }
}

/// Turns one JSON-RPC response body into either the `result` value or a
/// typed error.
///
/// A non-2xx status is transport-level ONLY when the body is not a
/// well-formed JSON-RPC envelope: many endpoints return a perfectly good
/// `{"error": ...}` under HTTP 400, and reporting that as a transport
/// failure would make it retriable, which would turn a permanent,
/// definitive refusal ("filter too wide", "method not supported") into an
/// endless retry loop.
pub(crate) fn parse_rpc_response(status: u16, text: &str) -> Result<Value, EvmRpcError> {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            let snippet: String = text.chars().take(300).collect();
            return Err(if (200..300).contains(&status) {
                EvmRpcError::Malformed(format!(
                    "response body is not JSON: {e} (body: {snippet:?})"
                ))
            } else {
                EvmRpcError::Transport(format!(
                    "HTTP {status} with a non-JSON body: {e} (body: {snippet:?})"
                ))
            });
        }
    };

    if let Some(error) = parsed.get("error") {
        // A JSON-RPC error object is a definitive answer even under a
        // non-2xx status; `code` is required by the spec, and a response
        // that omits it is malformed rather than silently code 0.
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .ok_or_else(|| EvmRpcError::Malformed(format!("error object has no code: {error}")))?;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("<no message>")
            .to_string();
        return Err(EvmRpcError::Method { code, message });
    }

    if !(200..300).contains(&status) {
        return Err(EvmRpcError::Transport(format!(
            "HTTP {status} with no JSON-RPC error object"
        )));
    }

    // `result: null` is meaningful for `eth_getBlockByNumber` (no such
    // block) and is passed through as `Value::Null`; an ABSENT `result`
    // is a malformed envelope and is not the same thing.
    match parsed.get("result") {
        Some(result) => Ok(result.clone()),
        None => Err(EvmRpcError::Malformed(
            "response has neither a result nor an error member".to_string(),
        )),
    }
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, EvmRpcError> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| EvmRpcError::Malformed(format!("missing or non-string field {name:?}")))
}

fn quantity_u64(value: &Value, name: &str) -> Result<u64, EvmRpcError> {
    let raw = field(value, name)?;
    quantity::parse_quantity_u64(raw)
        .map_err(|e| EvmRpcError::Malformed(format!("field {name:?} ({raw:?}): {e}")))
}

fn hash32(value: &Value, name: &str) -> Result<[u8; 32], EvmRpcError> {
    let raw = field(value, name)?;
    quantity::parse_data_exact::<32>(raw)
        .map_err(|e| EvmRpcError::Malformed(format!("field {name:?} ({raw:?}): {e}")))
}

/// Decodes one block header object. Public to the crate so the mock RPC
/// in the tests can be fed the same JSON a node would send, rather than a
/// hand-built struct that could not exercise this parsing at all.
pub(crate) fn decode_block_ref(value: &Value) -> Result<EvmBlockRef, EvmRpcError> {
    Ok(EvmBlockRef {
        number: quantity_u64(value, "number")?,
        hash: EvmBlockHash::from_bytes(hash32(value, "hash")?),
        parent_hash: EvmBlockHash::from_bytes(hash32(value, "parentHash")?),
        timestamp: quantity_u64(value, "timestamp")?,
    })
}

/// Decodes one log object.
///
/// Every field is required. A pending log — one whose `blockHash`,
/// `blockNumber`, `transactionHash` or `logIndex` is `null` because it is
/// not yet in a block — is refused as malformed rather than defaulted,
/// because this client only ever asks for closed block ranges and a
/// pending log in that answer means the response does not describe what
/// was asked for.
pub(crate) fn decode_raw_log(value: &Value) -> Result<EvmRawLog, EvmRpcError> {
    let address_raw = field(value, "address")?;
    let address: EvmAddress = address_raw
        .parse()
        .map_err(|e| EvmRpcError::Malformed(format!("log address {address_raw:?}: {e}")))?;

    let topics_value = value
        .get("topics")
        .and_then(Value::as_array)
        .ok_or_else(|| EvmRpcError::Malformed("log has no topics array".to_string()))?;
    let mut topics = Vec::with_capacity(topics_value.len());
    for (i, topic) in topics_value.iter().enumerate() {
        let raw = topic
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("log topic {i} is not a hex string")))?;
        topics.push(
            quantity::parse_data_exact::<32>(raw)
                .map_err(|e| EvmRpcError::Malformed(format!("log topic {i} ({raw:?}): {e}")))?,
        );
    }

    let data_raw = field(value, "data")?;
    let data = quantity::parse_data(data_raw)
        .map_err(|e| EvmRpcError::Malformed(format!("log data: {e}")))?;

    Ok(EvmRawLog {
        address,
        topics,
        data,
        block_number: quantity_u64(value, "blockNumber")?,
        block_hash: EvmBlockHash::from_bytes(hash32(value, "blockHash")?),
        tx_hash: EvmTxHash::from_bytes(hash32(value, "transactionHash")?),
        log_index: quantity_u64(value, "logIndex")?,
        // Absent means false: the field is optional in practice, and a
        // node that omits it is describing a canonical log.
        removed: value
            .get("removed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

impl EvmRpc for EvmRpcClient {
    async fn chain_id(&self) -> Result<EvmChainId, EvmRpcError> {
        let raw = self.call("eth_chainId", json!([])).await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_chainId returned {raw}")))?;
        EvmChainId::from_quantity_hex(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_chainId ({text:?}): {e}")))
    }

    async fn block_number(&self) -> Result<u64, EvmRpcError> {
        let raw = self.call("eth_blockNumber", json!([])).await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_blockNumber returned {raw}")))?;
        quantity::parse_quantity_u64(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_blockNumber ({text:?}): {e}")))
    }

    async fn block_by_number(&self, number: u64) -> Result<Option<EvmBlockRef>, EvmRpcError> {
        // `false`: headers only. This client never needs a transaction
        // body, and asking for full transactions would download the whole
        // block to read one hash.
        let raw = self
            .call(
                "eth_getBlockByNumber",
                json!([encode_quantity_u64(number), false]),
            )
            .await?;
        if raw.is_null() {
            return Ok(None);
        }
        let block = decode_block_ref(&raw)?;
        // The node answering with a different height than was asked for is
        // not a block this service can use for anything, least of all as a
        // reorg anchor.
        if block.number != number {
            return Err(EvmRpcError::Malformed(format!(
                "eth_getBlockByNumber({number}) returned block {} instead",
                block.number
            )));
        }
        Ok(Some(block))
    }

    async fn logs(&self, filter: &EvmLogFilter) -> Result<Vec<EvmRawLog>, EvmRpcError> {
        let params = json!([{
            "fromBlock": encode_quantity_u64(filter.from_block),
            "toBlock": encode_quantity_u64(filter.to_block),
            "address": filter.address.to_checksum_string(),
            "topics": [quantity::encode_data(&filter.topic0)],
        }]);
        let raw = self.call("eth_getLogs", params).await?;
        let array = raw
            .as_array()
            .ok_or_else(|| EvmRpcError::Malformed("eth_getLogs did not return an array".into()))?;
        let mut logs = Vec::with_capacity(array.len());
        for entry in array {
            let log = decode_raw_log(entry)?;
            // A node returning a log from an address that was not asked
            // for, or with a topic0 that was not asked for, is not
            // answering the question. Refused rather than filtered
            // silently, because a client that quietly drops unexpected
            // rows cannot tell a broken filter from an empty range.
            if log.address != filter.address {
                return Err(EvmRpcError::Malformed(format!(
                    "eth_getLogs returned a log from {} but the filter named {}",
                    log.address, filter.address
                )));
            }
            if log.topics.first() != Some(&filter.topic0) {
                return Err(EvmRpcError::Malformed(
                    "eth_getLogs returned a log whose topic0 does not match the filter".into(),
                ));
            }
            if log.block_number < filter.from_block || log.block_number > filter.to_block {
                return Err(EvmRpcError::Malformed(format!(
                    "eth_getLogs returned a log in block {} outside the requested range {}..={}",
                    log.block_number, filter.from_block, filter.to_block
                )));
            }
            logs.push(log);
        }
        Ok(logs)
    }
}

/// Retries `f` while it fails retriably, with the same 200ms-doubling
/// backoff `goldcoin::rpc::call_with_retry` uses. A separate function
/// only because the error types differ; the policy is deliberately
/// identical, so an operator reading either chain's logs sees the same
/// timing.
pub async fn call_with_retry<T, F, Fut>(max_attempts: u32, mut f: F) -> Result<T, EvmRpcError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, EvmRpcError>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if e.is_retriable() && attempt + 1 < max_attempts => {
                let delay_ms = 200u64 * 2u64.pow(attempt);
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests;
