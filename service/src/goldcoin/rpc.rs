//! Typed JSON-RPC client for Goldcoin Core, read/indexing surface only.
//!
//! Reused from the old bridge's `glc::rpc` (docs/01-reuse-inventory.md:
//! "REUSE unchanged... nothing here encodes mint/burn vs reserve
//! semantics"), trimmed to the calls this phase's indexer needs. Vault
//! payout-construction calls (`listunspent`, `signrawtransaction`,
//! `sendrawtransaction`, ...) are deliberately not ported here — they
//! belong to the Goldcoin vault/payout-construction phase (see
//! docs/07-implementation-plan.md Phase 3), not chain observation.
//!
//! Every RPC call distinguishes two failure classes (verified empirically
//! against a real Goldcoin Core node by the old bridge's engineering work,
//! docs/goldcoin-rpc-notes.md):
//! - [`RpcError::Transport`] — connection refused/reset/timeout. Retried
//!   with backoff ([`call_with_retry`]).
//! - [`RpcError::Method`]/[`RpcError::Malformed`] — a definitive answer (or
//!   a definitively wrong-shaped one). Never retried: fail closed rather
//!   than mask a real bug or unknown chain state behind a retry loop.

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("transport error contacting Goldcoin RPC: {0}")]
    Transport(String),
    #[error("Goldcoin RPC method error (code {code}): {message}")]
    Method { code: i64, message: String },
    #[error("malformed JSON-RPC response: {0}")]
    Malformed(String),
}

impl RpcError {
    /// Only [`RpcError::Transport`] is meaningfully retriable. Never treat a
    /// method error, a malformed response, or an absent field as success or
    /// as "assume unchanged" — fail closed (constraint: do not silently
    /// treat unknown chain state as success).
    pub fn is_retriable(&self) -> bool {
        matches!(self, RpcError::Transport(_))
    }
}

#[derive(Debug, Clone)]
pub struct RpcConfig {
    pub url: String,
    pub user: String,
    pub password: String,
    pub connect_timeout_ms: u64,
    pub read_timeout_ms: u64,
}

pub struct RpcClient {
    http: reqwest::Client,
    url: String,
    user: String,
    password: String,
}

pub type BlockTxids = Vec<String>;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct BlockHeader {
    pub hash: String,
    pub confirmations: i64,
    pub height: i64,
    pub time: i64,
    pub previousblockhash: Option<String>,
    pub tx: BlockTxids,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DecodedVout {
    pub value: f64,
    pub n: u32,
    #[serde(rename = "scriptPubKey")]
    pub script_pub_key: DecodedScriptPubKey,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DecodedScriptPubKey {
    pub hex: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub kind: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct DecodedTransaction {
    pub txid: String,
    pub vout: Vec<DecodedVout>,
    /// Absent entirely for an unconfirmed (mempool-only) transaction —
    /// verified empirically; never assume 0 means "just mined."
    #[allow(dead_code)]
    pub confirmations: Option<i64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TxOut {
    #[allow(dead_code)]
    pub confirmations: i64,
}

impl RpcClient {
    pub fn new(cfg: &RpcConfig) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(cfg.connect_timeout_ms))
            .timeout(std::time::Duration::from_millis(cfg.read_timeout_ms))
            .build()
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(RpcClient {
            http,
            url: cfg.url.clone(),
            user: cfg.user.clone(),
            password: cfg.password.clone(),
        })
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({ "jsonrpc": "1.0", "id": "glc-reserve-bridge", "method": method, "params": params });
        let response = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.password))
            .json(&body)
            .send()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let parsed: Value = response
            .json()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        if let Some(error) = parsed.get("error").filter(|e| !e.is_null()) {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)")
                .to_string();
            return Err(RpcError::Method { code, message });
        }
        parsed
            .get("result")
            .cloned()
            .ok_or_else(|| RpcError::Malformed("response has neither result nor error".into()))
    }

    async fn call_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let result = self.call(method, params).await?;
        serde_json::from_value(result).map_err(|e| RpcError::Malformed(e.to_string()))
    }

    pub async fn get_block_count(&self) -> Result<i64, RpcError> {
        self.call_typed("getblockcount", json!([])).await
    }

    pub async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        self.call_typed("getblockhash", json!([height])).await
    }

    /// `verbose=true` only — this RPC version has no numeric verbosity, and
    /// `"tx"` is always bare txid strings.
    pub async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        self.call_typed("getblock", json!([hash, true])).await
    }

    /// Requires `-txindex=1` on the node to resolve arbitrary historical
    /// transactions reliably.
    pub async fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> Result<DecodedTransaction, RpcError> {
        self.call_typed("getrawtransaction", json!([txid_hex, true]))
            .await
    }

    pub async fn get_raw_transaction_hex(&self, txid_hex: &str) -> Result<String, RpcError> {
        self.call_typed("getrawtransaction", json!([txid_hex, false]))
            .await
    }

    /// `include_mempool=false`: confirms genuinely on-chain-and-unspent.
    pub async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<TxOut>, RpcError> {
        let result = self
            .call("gettxout", json!([txid_hex, vout, false]))
            .await?;
        if result.is_null() {
            return Ok(None);
        }
        serde_json::from_value(result)
            .map(Some)
            .map_err(|e| RpcError::Malformed(e.to_string()))
    }
}

/// Bounded exponential-backoff retry for transient transport blips only.
/// Sustained outages are the outer indexer tick loop's job (fail closed:
/// the tick simply reports the node unavailable and tries again next tick,
/// never assumes success).
pub async fn call_with_retry<T, F, Fut>(max_attempts: u32, mut f: F) -> Result<T, RpcError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RpcError>>,
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn method_error_is_not_retriable() {
        assert!(!RpcError::Method {
            code: -1,
            message: "boom".into()
        }
        .is_retriable());
    }

    #[test]
    fn transport_error_is_retriable() {
        assert!(RpcError::Transport("connection refused".into()).is_retriable());
    }

    #[tokio::test]
    async fn call_with_retry_retries_transport_errors_and_eventually_succeeds() {
        let attempts = AtomicU32::new(0);
        let result: Result<i32, RpcError> = call_with_retry(5, || {
            let n = attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(RpcError::Transport("connection refused".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn call_with_retry_does_not_retry_method_errors() {
        let attempts = AtomicU32::new(0);
        let result: Result<i32, RpcError> = call_with_retry(5, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(RpcError::Method {
                    code: -1,
                    message: "nope".into(),
                })
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "method errors must fail closed, not retry"
        );
    }

    #[tokio::test]
    async fn call_with_retry_gives_up_after_max_attempts() {
        let attempts = AtomicU32::new(0);
        let result: Result<i32, RpcError> = call_with_retry(3, || {
            attempts.fetch_add(1, Ordering::SeqCst);
            async move { Err(RpcError::Transport("still down".into())) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}
