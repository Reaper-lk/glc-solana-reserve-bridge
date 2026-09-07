//! The resolved Robinhood indexer configuration.
//!
//! # Every field is required, and none has a default
//!
//! The whole section is optional — no `[robinhood.indexer]` means no
//! Robinhood indexer at all — but a section that IS present must name
//! every value. There is deliberately no `#[serde(default)]` anywhere in
//! its raw form and no fallback constant here.
//!
//! That is the lesson `crate::chains::robinhood::UNRESOLVED_CHAIN_
//! PARAMETERS` records, applied: a guessed chain parameter is worse than
//! an absent one. A defaulted confirmation depth silently accepts
//! reversible deposits; a defaulted start block silently decides that
//! some span of history is out of scope; a defaulted contract address
//! silently watches the wrong contract. An operator who has not decided
//! these values has not decided to run this indexer.
//!
//! # `expected_token` is recorded but NOT verified in this phase
//!
//! The contract's `TOKEN` is an immutable set at construction, and
//! checking that it equals the configured address needs `eth_call` — a
//! method [`super::rpc::EvmRpcClient`] deliberately does not have (see
//! its module docs). So this field is carried, logged at startup and
//! reported in health state, and that is all it does today.
//!
//! It is here rather than deferred because the value has to be pinned by
//! the operator who deploys, not discovered later by whoever writes the
//! payout leg — and because a config field that exists is a field a
//! reviewer can check against the deployment. Verifying it against the
//! chain is named as an explicit launch blocker; nothing in this phase
//! pretends it has been done.

use crate::evm::{EvmAddress, EvmChainId};

/// Fully resolved and validated; constructing one is
/// [`RobinhoodIndexerConfig::new`]'s job, and `crate::config` is its only
/// production caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodIndexerConfig {
    /// The JSON-RPC endpoint. Read-only usage; see
    /// [`super::rpc::EvmRpcClient`].
    pub rpc_url: String,
    /// The EIP-155 chain id this deployment expects. Checked against
    /// `eth_chainId` on EVERY tick, not once at startup: an endpoint can
    /// be repointed underneath a running process, and a bridge that
    /// noticed only at boot would index a different network for as long
    /// as it stayed up.
    pub chain_id: EvmChainId,
    /// The deployed `GlcRobinhoodBridge`. Also the `source_contract` half
    /// of every observation's durable identity, which is what keeps
    /// obligation indexes from a successor deployment distinct from this
    /// one's.
    pub bridge_contract: EvmAddress,
    /// The ERC-20 GLC the bridge custodies. Recorded only — see the
    /// module docs.
    pub expected_token: EvmAddress,
    /// The block to begin at when the ledger holds no cursor. Required
    /// exactly because there is no safe default: 0 would rescan an entire
    /// chain, and the head would silently declare every earlier deposit
    /// out of scope.
    pub start_block: u64,
    /// Confirmations before an observation is treated as irreversible.
    /// Counted `head - block + 1`, so `1` means "in the head block",
    /// matching `goldcoin::indexer`. Doubles as the reorg-reconciliation
    /// window: a reorg shallower than this is reconciled automatically, a
    /// reorg that reaches deeper has, by definition, contradicted
    /// something this service called final.
    pub confirmation_depth: u64,
    /// Delay between ticks.
    pub poll_interval_ms: u64,
    /// Applied to both the connect and the read phase of every request.
    pub request_timeout_ms: u64,
    /// Hard ceiling on the width of one `eth_getLogs` window. Public
    /// endpoints reject or truncate wide ranges, and a truncated answer
    /// that a client accepted would look exactly like "no deposits in
    /// this range" — so ranges are chunked to a width the operator has
    /// confirmed the endpoint honours, and each chunk is committed with
    /// its own cursor advance.
    pub max_log_block_range: u64,
}

/// Why a `[robinhood.indexer]` section was refused.
///
/// Separate from `crate::config::ConfigError` so this type stays usable
/// (and testable) without the whole config file machinery; `crate::
/// config` maps each variant onto its own `Invalid { field, detail }`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RobinhoodConfigError {
    #[error("rpc_url must not be empty")]
    EmptyRpcUrl,
    #[error(
        "rpc_url {url:?} must be an http:// or https:// endpoint — this client speaks JSON-RPC \
         over HTTP only, and never opens a websocket or a subscription"
    )]
    UnsupportedRpcScheme { url: String },
    #[error("bridge_contract must not be the zero address")]
    ZeroBridgeContract,
    #[error("expected_token must not be the zero address")]
    ZeroExpectedToken,
    #[error(
        "bridge_contract and expected_token are the same address ({address}) — the bridge \
         custodies the token, it is not the token"
    )]
    BridgeIsToken { address: String },
    #[error(
        "confirmation_depth must be at least 1 — a depth of 0 would mark a deposit irreversible \
         before it was in any block"
    )]
    ZeroConfirmationDepth,
    #[error("poll_interval_ms must be at least 1")]
    ZeroPollInterval,
    #[error("request_timeout_ms must be at least 1")]
    ZeroRequestTimeout,
    #[error("max_log_block_range must be at least 1")]
    ZeroMaxLogBlockRange,
}

impl RobinhoodIndexerConfig {
    /// Validates and constructs. Every check below refuses a value that
    /// would be silently harmful rather than obviously wrong — an empty
    /// URL fails fast anyway, but a zero confirmation depth or a bridge
    /// address that is really the token address would run happily and be
    /// wrong.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc_url: String,
        chain_id: EvmChainId,
        bridge_contract: EvmAddress,
        expected_token: EvmAddress,
        start_block: u64,
        confirmation_depth: u64,
        poll_interval_ms: u64,
        request_timeout_ms: u64,
        max_log_block_range: u64,
    ) -> Result<RobinhoodIndexerConfig, RobinhoodConfigError> {
        if rpc_url.trim().is_empty() {
            return Err(RobinhoodConfigError::EmptyRpcUrl);
        }
        if !(rpc_url.starts_with("http://") || rpc_url.starts_with("https://")) {
            return Err(RobinhoodConfigError::UnsupportedRpcScheme { url: rpc_url });
        }
        if bridge_contract.is_zero() {
            return Err(RobinhoodConfigError::ZeroBridgeContract);
        }
        if expected_token.is_zero() {
            return Err(RobinhoodConfigError::ZeroExpectedToken);
        }
        if bridge_contract == expected_token {
            return Err(RobinhoodConfigError::BridgeIsToken {
                address: bridge_contract.to_checksum_string(),
            });
        }
        if confirmation_depth == 0 {
            return Err(RobinhoodConfigError::ZeroConfirmationDepth);
        }
        if poll_interval_ms == 0 {
            return Err(RobinhoodConfigError::ZeroPollInterval);
        }
        if request_timeout_ms == 0 {
            return Err(RobinhoodConfigError::ZeroRequestTimeout);
        }
        if max_log_block_range == 0 {
            return Err(RobinhoodConfigError::ZeroMaxLogBlockRange);
        }
        Ok(RobinhoodIndexerConfig {
            rpc_url,
            chain_id,
            bridge_contract,
            expected_token,
            start_block,
            confirmation_depth,
            poll_interval_ms,
            request_timeout_ms,
            max_log_block_range,
        })
    }
}

#[cfg(test)]
mod tests;
