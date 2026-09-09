//! Robinhood Network chain observation: the EVM JSON-RPC client, the
//! strict `DepositCreated` decoder, the deposit indexer, its health
//! state, and the tick loop that drives it.
//!
//! # What this module can and cannot do
//!
//! It can READ. It opens exactly one kind of connection — an HTTP
//! JSON-RPC endpoint — and calls exactly four methods on it
//! (`eth_chainId`, `eth_blockNumber`, `eth_getBlockByNumber`,
//! `eth_getLogs`). It writes what it sees into the observation tables
//! schema v22 added, and nothing else.
//!
//! It cannot write to the chain. There is no signer, no key material, no
//! transaction builder, no nonce management, and no
//! `eth_sendRawTransaction`/`eth_sendTransaction`/`eth_call` in
//! [`rpc::EvmRpcClient`] — not disabled behind a flag, simply absent from
//! the type. A payout leg would have to add all of that, which is a
//! change no reviewer could miss.
//!
//! It also cannot settle. An observation lands in
//! `robinhood_deposit_observations`, a table with no foreign key to
//! `bridge_requests`, no reserve accounting, and a `settled` column the
//! database pins to `0` (see `crate::ledger::schema`'s v22 migration).
//! The four Robinhood routes stay closed by every gate in
//! [`crate::routes::RouteGate`] while this module runs, and observing a
//! deposit does not touch any of them. That is the point of the phase:
//! **see the deposits, prove the machinery, move nothing.**
//!
//! # Why it is optional, and what "optional" means precisely
//!
//! [`crate::config::Config::robinhood_indexer`] is an `Option`. When it
//! is `None` — which is every existing production config file, none of
//! which has a `[robinhood.indexer]` section — no client is constructed,
//! no task is spawned, and no socket is opened. The existing daemon's
//! startup path is byte-for-byte what it was: the Robinhood task is
//! `Option::map`ped over the config exactly the way the optional admin
//! API and alerting tasks already are, so "not configured" is not a
//! disabled code path, it is an absent one.
//!
//! # Relationship to the rest of the codebase
//!
//! Nothing here is new architecture. The pieces are the ones the Goldcoin
//! and Solana legs already use, applied to an EVM chain:
//!
//! | Concern | Reused from |
//! |---|---|
//! | RPC error classification, retriable vs definitive, backoff | `goldcoin::rpc` |
//! | Trait-over-RPC so the tick loop is testable without a node | `goldcoin::indexer::GoldcoinRpc`, `solana::rpc::SolanaRpc` |
//! | Cursor as "the highest row in a block table" | `goldcoin_indexed_blocks` / `Ledger::goldcoin_chain_tip` |
//! | Walk back to the last agreeing block, never guess a fork | `goldcoin::indexer::Indexer::find_fork_point` |
//! | Confirmation-depth promotion, `head - block + 1` | `goldcoin::indexer::Indexer::promote_confirming` |
//! | Post-finality reorg is an incident, not a rollback | `Ledger::detect_post_finality_reorg` |
//! | Liveness/halt visible outside the indexer's own memory | `ops::indexer_status` |
//! | Backoff on total outage, shutdown only between ticks | `daemon::run` |
//! | Typed, strictly-parsed chain primitives | `crate::evm` |
//! | 18-decimal <-> canonical amount conversion | `crate::amount_conversion::robinhood` |
//!
//! The one genuinely new thing is the ABI decoder, which exists because
//! an EVM log is the first chain observation in this service that arrives
//! as a topic/data split rather than as a typed RPC struct.

pub mod admin;
pub mod auth;
pub mod calls;
pub mod config;
pub mod daemon;
pub mod deposit_event;
pub mod fold;
pub mod governance;
pub mod governance_session;
pub mod health;
pub mod indexer;
pub mod policy;
pub mod preflight;
pub mod public;
pub mod redact;
pub mod refund;
pub mod reserve;
pub mod rpc;
pub mod settlement;
pub mod settlement_config;
pub mod signer;
pub mod submitter;

#[cfg(test)]
pub(crate) mod testkit;

pub use auth::{
    AuthError, BridgeDomain, PayoutAuth, ProtocolChainPair, RefundAuth, SettlementAuth,
};
pub use config::RobinhoodIndexerConfig;
pub use deposit_event::{
    decode_deposit_created, DepositCreatedEvent, DepositDecodeError, DEPOSIT_CREATED_SIGNATURE,
};
pub use fold::{fold_observation, FoldError, FoldOutcome};
pub use governance::{
    limits_from_policy, GovernanceAuth, GovernanceError, GovernancePayload, MinimumOverrides,
};
pub use health::{RobinhoodHealth, RobinhoodHealthSnapshot};
pub use indexer::{RobinhoodIndexer, RobinhoodIndexerError, RobinhoodTickOutcome};
pub use policy::{PolicyMismatch, RobinhoodPolicyBinding, RobinhoodPolicyError};
pub use preflight::{PreflightError, VerifiedDeployment};
pub use public::{
    LiveRobinhoodContractSource, RobinhoodContractSource, RobinhoodContractState,
    RobinhoodContractStatus,
};
pub use redact::Redactor;
pub use refund::{begin_refund, RefundError};
pub use reserve::{ReserveReconciler, ReserveTickOutcome};
pub use rpc::{
    EvmBlockRef, EvmBlockTag, EvmBroadcastOutcome, EvmCall, EvmCallRpc, EvmLogFilter, EvmRawLog,
    EvmReceipt, EvmRpc, EvmRpcClient, EvmRpcError, EvmSubmitRpc,
};
pub use settlement::{SettlementError, SettlementReport, Settler};
pub use settlement_config::{RobinhoodSettlementConfig, RobinhoodSettlementConfigError};
pub use signer::{
    collect_quorum, AuthorizationQuorum, DevEvmAuthSigner, EvmAuthSigner, QuorumError,
    SIGNER_THRESHOLD,
};
pub use submitter::{SubmitError, Submitter, SubmitterKeyError};
