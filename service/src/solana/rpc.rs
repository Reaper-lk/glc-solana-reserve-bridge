//! Thin Solana JSON-RPC client wrapper, always pinned to `finalized`
//! commitment for indexing reads (docs/03-architecture.md: "the safety
//! invariant... Solana-side reads use `finalized` commitment only" — the
//! old bridge enforced this as a startup error, ADR/threat-model precedent
//! this crate repeats structurally by never exposing a way to request a
//! weaker commitment from the indexer).
//!
//! Adapted from the old bridge's `solana::rpc` (docs/01-reuse-inventory.md:
//! "REUSE with modification" — the trait/mock pattern and bounded-retry
//! shape are reused; account layouts are new (this program's, not the old
//! federated one's) and no `anchor-lang` dependency is introduced here
//! (owner decision R1 in the old bridge, repeated for the same reason:
//! this workspace's async/networking dependency graph must stay
//! independent of the on-chain SBF build — see docs/08-migration-strategy.md).

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::{CommitmentConfig, CommitmentLevel};
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SolanaRpcError {
    #[error("transport error contacting Solana RPC: {0}")]
    Transport(String),
    #[error("Solana RPC returned an error: {0}")]
    Method(String),
    #[error("malformed account data: {0}")]
    Malformed(String),
}

impl SolanaRpcError {
    pub fn is_retriable(&self) -> bool {
        matches!(self, SolanaRpcError::Transport(_))
    }
}

pub trait SolanaRpc {
    fn get_account(
        &self,
        pubkey: &Pubkey,
    ) -> impl std::future::Future<Output = Result<Option<Account>, SolanaRpcError>> + Send;
    fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> impl std::future::Future<Output = Result<Vec<Option<Account>>, SolanaRpcError>> + Send;
    fn get_slot(&self) -> impl std::future::Future<Output = Result<u64, SolanaRpcError>> + Send;

    fn get_latest_blockhash(
        &self,
    ) -> impl std::future::Future<Output = Result<Hash, SolanaRpcError>> + Send;

    /// Submits `tx` (already fully signed). Acceptance is not inclusion —
    /// callers must pair this with [`SolanaRpc::get_signature_status`] and
    /// [`SolanaRpc::is_blockhash_valid`] (see [`crate::solana::confirm`]),
    /// same discipline the old bridge's ADR-0030 exists to enforce: never
    /// report success on acceptance alone.
    fn send_transaction(
        &self,
        tx: &Transaction,
    ) -> impl std::future::Future<Output = Result<Signature, SolanaRpcError>> + Send;

    /// What the cluster knows about `signature` at this client's
    /// (`finalized`) commitment. Three distinct outcomes — collapsing any
    /// pair is how an unconfirmed transaction gets reported a success:
    /// `None` (not yet seen — says nothing about whether it ever will be),
    /// `Some(Ok(()))` (confirmed, succeeded), `Some(Err(_))` (confirmed,
    /// FAILED on chain — the instruction did not take effect).
    fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> impl std::future::Future<Output = Result<Option<Result<(), String>>, SolanaRpcError>> + Send;

    /// Whether `blockhash` can still be used to land a transaction — what
    /// makes bounded confirmation possible without guessing (see
    /// [`crate::solana::confirm`]).
    fn is_blockhash_valid(
        &self,
        blockhash: &Hash,
    ) -> impl std::future::Future<Output = Result<bool, SolanaRpcError>> + Send;
}

pub struct RealSolanaRpc {
    client: RpcClient,
}

impl RealSolanaRpc {
    /// Commitment is always `finalized` in production use; the indexer and
    /// orchestrator built on top of this never override it.
    pub fn new(url: String) -> Self {
        let client = RpcClient::new_with_commitment(
            url,
            CommitmentConfig {
                commitment: CommitmentLevel::Finalized,
            },
        );
        RealSolanaRpc { client }
    }
}

impl SolanaRpc for RealSolanaRpc {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        match self.client.get_account(pubkey).await {
            Ok(acc) => Ok(Some(acc)),
            Err(e) if e.to_string().contains("AccountNotFound") => Ok(None),
            Err(e) => Err(classify_client_error(e)),
        }
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        self.client
            .get_multiple_accounts(pubkeys)
            .await
            .map_err(classify_client_error)
    }

    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        self.client.get_slot().await.map_err(classify_client_error)
    }

    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        self.client
            .get_latest_blockhash()
            .await
            .map_err(classify_client_error)
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature, SolanaRpcError> {
        self.client
            .send_transaction_with_config(
                tx,
                RpcSendTransactionConfig {
                    skip_preflight: false,
                    preflight_commitment: Some(CommitmentLevel::Finalized),
                    ..Default::default()
                },
            )
            .await
            .map_err(classify_client_error)
    }

    async fn get_signature_status(
        &self,
        signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        let statuses = self
            .client
            .get_signature_statuses(&[*signature])
            .await
            .map_err(classify_client_error)?;
        let Some(Some(status)) = statuses.value.into_iter().next() else {
            return Ok(None);
        };
        if !status.satisfies_commitment(CommitmentConfig {
            commitment: CommitmentLevel::Finalized,
        }) {
            return Ok(None);
        }
        Ok(Some(match status.err {
            Some(e) => Err(e.to_string()),
            None => Ok(()),
        }))
    }

    async fn is_blockhash_valid(&self, blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        self.client
            .is_blockhash_valid(
                blockhash,
                CommitmentConfig {
                    commitment: CommitmentLevel::Finalized,
                },
            )
            .await
            .map_err(classify_client_error)
    }
}

fn classify_client_error(e: solana_client::client_error::ClientError) -> SolanaRpcError {
    use solana_client::client_error::ClientErrorKind;
    match e.kind() {
        ClientErrorKind::Io(_) | ClientErrorKind::Reqwest(_) => {
            SolanaRpcError::Transport(e.to_string())
        }
        _ => SolanaRpcError::Method(e.to_string()),
    }
}

pub async fn call_with_retry<T, F, Fut>(max_attempts: u32, mut f: F) -> Result<T, SolanaRpcError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, SolanaRpcError>>,
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
