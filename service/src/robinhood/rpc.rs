//! The Ethereum JSON-RPC client: the observation half (Phase E) and the
//! settlement half (Phase F).
//!
//! # The whole method surface, and why the split is enforced by TYPES
//!
//! | Method | Trait | Why it is needed |
//! |---|---|---|
//! | `eth_chainId` | [`EvmRpc`] | Prove the endpoint is the network the operator configured, every tick |
//! | `eth_blockNumber` | [`EvmRpc`] | The head, from which finality depth is measured |
//! | `eth_getBlockByNumber` | [`EvmRpc`] | A block's hash, for the scan anchor and the reorg walk |
//! | `eth_getLogs` | [`EvmRpc`] | The deposits themselves |
//! | `eth_call` | [`EvmCallRpc`] | Contract state: token, route enablement, pauses, signer epoch, obligations, balances |
//! | `eth_getTransactionCount` | [`EvmSubmitRpc`] | Nonce reconciliation at `"pending"` |
//! | `eth_estimateGas` | [`EvmSubmitRpc`] | A gas limit that is measured, not guessed |
//! | `eth_gasPrice` | [`EvmSubmitRpc`] | Legacy-envelope fee |
//! | `eth_maxPriorityFeePerGas` | [`EvmSubmitRpc`] | EIP-1559 tip |
//! | `eth_getBalance` | [`EvmSubmitRpc`] | Can the submitter afford the gas it is about to commit to |
//! | `eth_sendRawTransaction` | [`EvmSubmitRpc`] | The broadcast |
//! | `eth_getTransactionReceipt` | [`EvmSubmitRpc`] | Included, and did it SUCCEED or REVERT |
//!
//! Phase E's whole safety story was that this client could not broadcast,
//! because the method was absent from the type. Phase F needs to
//! broadcast, so the property is preserved a different way: the surface
//! is split across three traits, and the indexer's own type bound is
//! still [`EvmRpc`] alone. The deposit indexer therefore still cannot
//! call `eth_sendRawTransaction` — not by convention, but because the
//! trait it is generic over does not have the method. Widening it would
//! mean changing the indexer's bound, which is a visible, reviewable
//! diff.
//!
//! [`EvmCallRpc`] is deliberately separate from [`EvmSubmitRpc`] for the
//! same reason: reading contract state is what the preflight and every
//! pre-broadcast gate do, and those must be usable — and testable — by
//! code that has no ability to send anything at all.
//!
//! There is still deliberately no `eth_sendTransaction`, no `eth_sign`
//! and no `personal_*`. Those ask the NODE to hold a key and sign on this
//! service's behalf, which is exactly the custody arrangement this bridge
//! does not have. Every transaction is signed in-process (or by a remote
//! signer) and handed to the node already signed.
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
use crate::evm::{EvmAddress, EvmChainId, EvmU256};

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
/// no key, no nonce, no signer, no account. It implements all three
/// traits; which of them a given caller may use is decided by that
/// caller's own type bound, not by this type.
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

// =====================================================================
// Phase F: contract reads
// =====================================================================

/// One `eth_call`: the address to call, and the calldata.
///
/// There is no `from`, no `value` and no `gas` field. Every call this
/// service makes is a plain `view` read of a contract this service does
/// not own, and supplying a caller identity would only matter for a
/// contract whose views depend on `msg.sender` — which
/// `GlcRobinhoodBridge`'s do not. Omitting the fields means there is no
/// way to accidentally state-change through this path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmCall {
    pub to: EvmAddress,
    pub data: Vec<u8>,
}

/// The read-only contract-state surface.
///
/// Separate from [`EvmSubmitRpc`] so that the preflight, the pre-broadcast
/// gates and every operator-facing view can be written — and tested —
/// against a client that provably cannot send a transaction.
pub trait EvmCallRpc {
    /// `eth_call` against the given block tag.
    ///
    /// The block tag is a PARAMETER rather than always `"latest"` because
    /// a pre-broadcast gate and a post-receipt verification are asking
    /// different questions: "is this route open right now" versus "what
    /// did this contract look like in the block my transaction landed in".
    fn call(
        &self,
        call: &EvmCall,
        block: EvmBlockTag,
    ) -> impl Future<Output = Result<Vec<u8>, EvmRpcError>> + Send;

    /// `eth_getCode` — whether there is a contract at this address at all.
    ///
    /// Needed because `eth_call` against an address with no code does not
    /// fail: it returns empty data, which every ABI decoder then reports
    /// as a malformed return. Distinguishing "there is nothing deployed
    /// here" from "the deployed thing has a different ABI" is the
    /// difference between a wrong config value and a wrong contract, and
    /// an operator needs to be told which.
    fn code_at(
        &self,
        address: EvmAddress,
        block: EvmBlockTag,
    ) -> impl Future<Output = Result<Vec<u8>, EvmRpcError>> + Send;
}

/// Which block a read is evaluated against.
///
/// `Latest` is the node's current head. `Number` pins a specific height,
/// which is what a post-receipt verification uses so that it reads the
/// state the transaction actually executed against rather than whatever
/// the chain has moved on to.
///
/// `"pending"` is deliberately NOT expressible here. A pending-state read
/// describes a block that may never exist, and treating one as a gate
/// would mean authorizing a transfer against state that never happened.
/// The one place `"pending"` is legitimate is the nonce count, which has
/// its own dedicated method and its own dedicated reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvmBlockTag {
    Latest,
    Number(u64),
}

impl EvmBlockTag {
    fn as_param(self) -> Value {
        match self {
            EvmBlockTag::Latest => json!("latest"),
            EvmBlockTag::Number(n) => json!(encode_quantity_u64(n)),
        }
    }
}

// =====================================================================
// Phase F: transaction submission
// =====================================================================

/// One transaction receipt, decoded only as far as this service uses it.
///
/// `status` is the field that matters and it is NOT optional here: a
/// pre-Byzantium receipt has no status field, and a receipt without one
/// cannot answer "did this succeed", so it is refused as malformed rather
/// than assumed successful. Assuming success on a missing status is how a
/// reverted settlement gets recorded as a completed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmReceipt {
    pub tx_hash: EvmTxHash,
    /// `true` = executed successfully, `false` = REVERTED. A reverted
    /// transaction still consumed its nonce and still cost gas; it is not
    /// a transaction that did not happen.
    pub success: bool,
    pub block_number: u64,
    pub block_hash: EvmBlockHash,
    pub gas_used: u64,
    /// The logs this transaction emitted, in the same raw shape
    /// `eth_getLogs` returns — so the event-presence check that confirms
    /// an operation actually did what it claimed uses the SAME decoder
    /// the indexer does, not a second one.
    pub logs: Vec<EvmRawLog>,
}

/// What the node said about a broadcast.
///
/// Every variant here is a DEFINITIVE answer about one specific signed
/// transaction, and each one has a different correct response — which is
/// why they are separate variants rather than an error string the caller
/// has to pattern-match on prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvmBroadcastOutcome {
    /// The node accepted it into its mempool and returned its hash.
    Accepted { tx_hash: EvmTxHash },
    /// The node already has this exact transaction. Indistinguishable
    /// from `Accepted` in every way that matters: the transaction is out
    /// there, under the nonce that was allocated for it. This is the
    /// EXPECTED answer when re-broadcasting after a crash, and treating
    /// it as an error would turn routine recovery into an alarm.
    AlreadyKnown,
    /// The nonce is below the account's current count: the chain has
    /// already moved past it. Either this transaction (or a replacement)
    /// was mined, or a different transaction consumed the nonce. NEVER a
    /// reason to allocate a fresh nonce and retry — the caller must go
    /// look at what actually happened at that nonce.
    NonceTooLow,
    /// A replacement was offered for a nonce that already has a pending
    /// transaction, at a fee the node considers insufficient. The
    /// original is still in flight.
    ReplacementUnderpriced,
    /// The node refused for some other definitive reason (intrinsic gas
    /// too low, insufficient funds, fee cap below base fee, ...). Carries
    /// the node's own message; never retried blindly.
    Rejected { code: i64, message: String },
}

/// The transaction-submission surface.
///
/// Held separately from [`EvmCallRpc`] so that the ONE trait bound that
/// grants the ability to broadcast is visible in every signature that has
/// it, and absent from every signature that does not.
pub trait EvmSubmitRpc {
    /// `eth_getTransactionCount(address, "pending")`.
    ///
    /// `"pending"` specifically, and this is the one place it is correct:
    /// the question is "what is the next nonce nobody has claimed",
    /// and a `"latest"` count would exclude this service's own
    /// already-broadcast, not-yet-mined transactions — producing a nonce
    /// that collides with one already in flight.
    ///
    /// It is a RECONCILIATION input, never the allocator. See
    /// [`super::nonce`] for why the allocator reads the ledger instead.
    fn pending_nonce(
        &self,
        address: EvmAddress,
    ) -> impl Future<Output = Result<u64, EvmRpcError>> + Send;

    /// `eth_getBalance(address, "latest")` — the submitter's gas balance.
    fn balance(
        &self,
        address: EvmAddress,
    ) -> impl Future<Output = Result<EvmU256, EvmRpcError>> + Send;

    /// `eth_estimateGas`. Returns the node's estimate for executing
    /// `call` from `from`.
    ///
    /// A gas limit is MEASURED rather than configured because the three
    /// operations differ by a large factor and a single configured
    /// constant would either overpay on every settlement or run out of
    /// gas on a payout. A failing estimate is also the cheapest possible
    /// dry run: `eth_estimateGas` executes the call, so a revert here
    /// means the real transaction would have reverted too — caught before
    /// a nonce was consumed.
    fn estimate_gas(
        &self,
        from: EvmAddress,
        call: &EvmCall,
    ) -> impl Future<Output = Result<u64, EvmRpcError>> + Send;

    /// `eth_gasPrice` — the legacy envelope's per-gas price.
    fn gas_price(&self) -> impl Future<Output = Result<u128, EvmRpcError>> + Send;

    /// `eth_maxPriorityFeePerGas` — the EIP-1559 tip suggestion.
    ///
    /// `None` when the node does not implement the method, which is
    /// itself evidence about the chain's fee market and is reported
    /// rather than defaulted.
    fn max_priority_fee_per_gas(
        &self,
    ) -> impl Future<Output = Result<Option<u128>, EvmRpcError>> + Send;

    /// The `baseFeePerGas` of the latest block, or `None` if the header
    /// has no such field.
    ///
    /// This is the EVIDENCE that decides whether the chain has an
    /// EIP-1559 fee market, and it is why the envelope choice does not
    /// have to be a guess: a header carrying a base fee proves London is
    /// active, and a header without one proves it is not. The configured
    /// envelope is cross-checked against this before anything is
    /// broadcast (`crate::evm::tx`'s module docs).
    fn latest_base_fee(&self) -> impl Future<Output = Result<Option<u128>, EvmRpcError>> + Send;

    /// `eth_sendRawTransaction`.
    fn send_raw_transaction(
        &self,
        raw: &[u8],
    ) -> impl Future<Output = Result<EvmBroadcastOutcome, EvmRpcError>> + Send;

    /// `eth_getTransactionReceipt`. `None` means "not mined yet, as far as
    /// this node knows" — never "it failed", and never "it will not be
    /// mined".
    fn transaction_receipt(
        &self,
        tx_hash: EvmTxHash,
    ) -> impl Future<Output = Result<Option<EvmReceipt>, EvmRpcError>> + Send;
}

/// Classifies a definitive node refusal of a broadcast.
///
/// # Why this matches on prose
///
/// There is no standard error CODE for "already known" or "nonce too
/// low": every client returns `-32000` (or its own variant) with a
/// human-readable message, and the messages differ between geth,
/// erigon, nethermind, reth and every proxy in between. Matching on
/// the message is therefore not laziness, it is the only signal the
/// protocol provides.
///
/// The matching is deliberately GENEROUS on the two outcomes that are
/// safe to over-detect and CONSERVATIVE on everything else:
///
/// - Mistaking a genuine rejection for `AlreadyKnown` would be unsafe,
///   so the phrases matched are ones no other condition produces.
/// - Anything unrecognised becomes [`EvmBroadcastOutcome::Rejected`],
///   which never auto-retries and never reallocates a nonce. An
///   unrecognised message therefore stalls one operation for a human
///   rather than doing something clever with money.
pub(crate) fn classify_broadcast_error(code: i64, message: &str) -> EvmBroadcastOutcome {
    let lower = message.to_ascii_lowercase();
    // geth: "already known"; erigon: "already known"; nethermind:
    // "AlreadyKnown"; some proxies: "transaction already exists".
    // A transaction that is already in the pool under this nonce IS the
    // transaction we sent — the raw bytes are byte-identical, so there is
    // no other transaction it could be.
    if lower.contains("already known")
        || lower.contains("alreadyknown")
        || lower.contains("already exists")
        || lower.contains("known transaction")
        || lower.contains("already imported")
    {
        return EvmBroadcastOutcome::AlreadyKnown;
    }
    if lower.contains("nonce too low") || lower.contains("nonce is too low") {
        return EvmBroadcastOutcome::NonceTooLow;
    }
    if lower.contains("replacement transaction underpriced")
        || lower.contains("replacement underpriced")
    {
        return EvmBroadcastOutcome::ReplacementUnderpriced;
    }
    EvmBroadcastOutcome::Rejected {
        code,
        message: message.to_string(),
    }
}

/// Decodes one transaction receipt object.
///
/// Refuses a receipt whose `status` is absent — see [`EvmReceipt`] — and
/// one whose `blockHash`/`blockNumber` is null, which is what a node
/// returns for a transaction it has seen but not mined. A caller asking
/// for a receipt is asking "is it mined", and a half-populated receipt is
/// not an answer to that question.
pub(crate) fn decode_receipt(value: &Value) -> Result<EvmReceipt, EvmRpcError> {
    let status_raw = field(value, "status")?;
    let status = quantity::parse_quantity_u64(status_raw)
        .map_err(|e| EvmRpcError::Malformed(format!("receipt status ({status_raw:?}): {e}")))?;
    let success = match status {
        0 => false,
        1 => true,
        other => {
            return Err(EvmRpcError::Malformed(format!(
                "receipt status must be 0 or 1, got {other}"
            )))
        }
    };
    let logs_value = value
        .get("logs")
        .and_then(Value::as_array)
        .ok_or_else(|| EvmRpcError::Malformed("receipt has no logs array".to_string()))?;
    let mut logs = Vec::with_capacity(logs_value.len());
    for entry in logs_value {
        logs.push(decode_raw_log(entry)?);
    }
    Ok(EvmReceipt {
        tx_hash: EvmTxHash::from_bytes(hash32(value, "transactionHash")?),
        success,
        block_number: quantity_u64(value, "blockNumber")?,
        block_hash: EvmBlockHash::from_bytes(hash32(value, "blockHash")?),
        gas_used: quantity_u64(value, "gasUsed")?,
        logs,
    })
}

impl EvmRpcClient {
    /// The JSON body of one `eth_call`-style contract read.
    fn call_params(call: &EvmCall, block: EvmBlockTag) -> Value {
        json!([
            {
                "to": call.to.to_checksum_string(),
                "data": quantity::encode_data(&call.data),
            },
            block.as_param()
        ])
    }
}

impl EvmCallRpc for EvmRpcClient {
    async fn call(&self, call: &EvmCall, block: EvmBlockTag) -> Result<Vec<u8>, EvmRpcError> {
        let raw = self
            .call("eth_call", Self::call_params(call, block))
            .await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_call returned {raw}")))?;
        quantity::parse_data(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_call result ({text:?}): {e}")))
    }

    async fn code_at(
        &self,
        address: EvmAddress,
        block: EvmBlockTag,
    ) -> Result<Vec<u8>, EvmRpcError> {
        let raw = self
            .call(
                "eth_getCode",
                json!([address.to_checksum_string(), block.as_param()]),
            )
            .await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_getCode returned {raw}")))?;
        quantity::parse_data(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_getCode result ({text:?}): {e}")))
    }
}

impl EvmSubmitRpc for EvmRpcClient {
    async fn pending_nonce(&self, address: EvmAddress) -> Result<u64, EvmRpcError> {
        let raw = self
            .call(
                "eth_getTransactionCount",
                json!([address.to_checksum_string(), "pending"]),
            )
            .await?;
        let text = raw.as_str().ok_or_else(|| {
            EvmRpcError::Malformed(format!("eth_getTransactionCount returned {raw}"))
        })?;
        quantity::parse_quantity_u64(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_getTransactionCount ({text:?}): {e}")))
    }

    async fn balance(&self, address: EvmAddress) -> Result<EvmU256, EvmRpcError> {
        let raw = self
            .call(
                "eth_getBalance",
                json!([address.to_checksum_string(), "latest"]),
            )
            .await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_getBalance returned {raw}")))?;
        quantity::parse_quantity_u256(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_getBalance ({text:?}): {e}")))
    }

    async fn estimate_gas(&self, from: EvmAddress, call: &EvmCall) -> Result<u64, EvmRpcError> {
        let params = json!([{
            "from": from.to_checksum_string(),
            "to": call.to.to_checksum_string(),
            "data": quantity::encode_data(&call.data),
        }]);
        let raw = self.call("eth_estimateGas", params).await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_estimateGas returned {raw}")))?;
        quantity::parse_quantity_u64(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_estimateGas ({text:?}): {e}")))
    }

    async fn gas_price(&self) -> Result<u128, EvmRpcError> {
        let raw = self.call("eth_gasPrice", json!([])).await?;
        let text = raw
            .as_str()
            .ok_or_else(|| EvmRpcError::Malformed(format!("eth_gasPrice returned {raw}")))?;
        let word = quantity::parse_quantity_u256(text)
            .map_err(|e| EvmRpcError::Malformed(format!("eth_gasPrice ({text:?}): {e}")))?;
        word.try_to_u128()
            .map_err(|e| EvmRpcError::Malformed(format!("eth_gasPrice {text:?} exceeds u128: {e}")))
    }

    async fn max_priority_fee_per_gas(&self) -> Result<Option<u128>, EvmRpcError> {
        match self.call("eth_maxPriorityFeePerGas", json!([])).await {
            Ok(raw) => {
                let text = raw.as_str().ok_or_else(|| {
                    EvmRpcError::Malformed(format!("eth_maxPriorityFeePerGas returned {raw}"))
                })?;
                let word = quantity::parse_quantity_u256(text).map_err(|e| {
                    EvmRpcError::Malformed(format!("eth_maxPriorityFeePerGas ({text:?}): {e}"))
                })?;
                word.try_to_u128().map(Some).map_err(|e| {
                    EvmRpcError::Malformed(format!(
                        "eth_maxPriorityFeePerGas {text:?} exceeds u128: {e}"
                    ))
                })
            }
            // A node that does not implement the method answers with a
            // method error. That is a FACT about the endpoint, reported as
            // `None`, not an outage to retry and not a zero to assume.
            Err(EvmRpcError::Method { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn latest_base_fee(&self) -> Result<Option<u128>, EvmRpcError> {
        let raw = self
            .call("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        if raw.is_null() {
            return Err(EvmRpcError::Malformed(
                "eth_getBlockByNumber(latest) returned null — a chain always has a head".into(),
            ));
        }
        // Absent is the answer, not a failure: a pre-London header simply
        // has no such field, and that is exactly the evidence being sought.
        let Some(text) = raw.get("baseFeePerGas").and_then(Value::as_str) else {
            return Ok(None);
        };
        let word = quantity::parse_quantity_u256(text)
            .map_err(|e| EvmRpcError::Malformed(format!("baseFeePerGas ({text:?}): {e}")))?;
        word.try_to_u128().map(Some).map_err(|e| {
            EvmRpcError::Malformed(format!("baseFeePerGas {text:?} exceeds u128: {e}"))
        })
    }

    async fn send_raw_transaction(&self, raw: &[u8]) -> Result<EvmBroadcastOutcome, EvmRpcError> {
        match self
            .call(
                "eth_sendRawTransaction",
                json!([quantity::encode_data(raw)]),
            )
            .await
        {
            Ok(value) => {
                let text = value.as_str().ok_or_else(|| {
                    EvmRpcError::Malformed(format!("eth_sendRawTransaction returned {value}"))
                })?;
                let hash = quantity::parse_data_exact::<32>(text).map_err(|e| {
                    EvmRpcError::Malformed(format!("eth_sendRawTransaction ({text:?}): {e}"))
                })?;
                Ok(EvmBroadcastOutcome::Accepted {
                    tx_hash: EvmTxHash::from_bytes(hash),
                })
            }
            // A definitive refusal is CLASSIFIED, not propagated: several
            // of them are routine and expected, and a caller that saw only
            // an error string would have to re-derive that distinction.
            Err(EvmRpcError::Method { code, message }) => {
                Ok(classify_broadcast_error(code, &message))
            }
            // A transport failure means the question was never answered.
            // The transaction may or may not have reached the node, so the
            // caller must NOT treat this as "not sent" — it retries the
            // same bytes, which is idempotent.
            Err(e) => Err(e),
        }
    }

    async fn transaction_receipt(
        &self,
        tx_hash: EvmTxHash,
    ) -> Result<Option<EvmReceipt>, EvmRpcError> {
        let raw = self
            .call(
                "eth_getTransactionReceipt",
                json!([quantity::encode_data(tx_hash.as_bytes())]),
            )
            .await?;
        if raw.is_null() {
            return Ok(None);
        }
        let receipt = decode_receipt(&raw)?;
        // The node answering about a different transaction is not an
        // answer to the question that was asked.
        if receipt.tx_hash != tx_hash {
            return Err(EvmRpcError::Malformed(format!(
                "eth_getTransactionReceipt({tx_hash}) returned a receipt for {} instead",
                receipt.tx_hash
            )));
        }
        Ok(Some(receipt))
    }
}
