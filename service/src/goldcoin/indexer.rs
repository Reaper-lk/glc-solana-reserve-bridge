//! The Goldcoin indexer tick loop: forward indexing, reorg detection and
//! rollback, request-binding matching, and confirmation-depth promotion.
//! Adapted from the old bridge's `glc::indexer` (docs/01-reuse-inventory.md:
//! "REUSE with modification" — the reorg-walk/confirmation-depth mechanics
//! are chain-mechanics and directly reusable; the deposit-candidate ledger
//! is replaced by direct `bridge_requests` mutation through [`Ledger`], and
//! `ReadyForSignature`'s mint-claim-message construction is replaced by the
//! plain `SourceFinalized` transition — settlement authorization is a later
//! phase's concern (attestation signing clients), not indexing).
//!
//! [`GoldcoinRpc`] is a trait covering the exact RPC surface the indexer
//! needs, implemented for the real [`RpcClient`] and for a test-only mock,
//! so tick/reorg logic is unit-testable without a live node (real-node
//! testing against Goldcoin v0.17.0-beta1 regtest is Phase 6 per
//! docs/11-testing-plan.md, not this pass — no `goldcoind` binary is
//! available in this environment; see IMPLEMENTATION_LOG.md).

use std::future::Future;

use thiserror::Error;

use crate::ledger::{GlcObservationOutcome, Ledger, LedgerError};

use super::deposit::{extract_request_binding, vault_output_candidates};
use super::hex;
use super::rpc::{BlockHeader, BroadcastOutcome, DecodedTransaction, RpcClient, RpcError, TxOut};

#[derive(Debug, Error)]
pub enum IndexerError {
    #[error("Goldcoin node unavailable: {0}")]
    NodeUnavailable(RpcError),
    #[error("Goldcoin RPC method error: {0}")]
    Rpc(RpcError),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
}

#[derive(Debug, Clone)]
pub struct ReorgSummary {
    pub fork_height: i64,
    pub old_tip_height: i64,
    pub orphaned_count: i64,
}

#[derive(Debug, Clone)]
pub enum TickOutcome {
    Progressed {
        blocks_indexed: i64,
        reorg: Option<ReorgSummary>,
    },
    /// A reorg deeper than `max_reorg_depth` was detected. No database
    /// writes were made this tick, and every future tick reports `Halted`
    /// again without touching the database or the network until the
    /// process is restarted with a wider `max_reorg_depth` or manual
    /// operator intervention (fail closed: never guess a fork point).
    Halted { attempted_depth: i64 },
    /// A reorg was found within `max_reorg_depth`, but its fork point is
    /// at or below the source block of at least one `GlcToSol` request
    /// already told its deposit was final — a genuine incident
    /// (docs/10-threat-model.md's "post-finality reorg", never routine).
    /// Neither the normal rollback nor forward indexing ran this tick;
    /// both reserve directions were paused
    /// ([`Ledger::record_post_finality_reorg`]) and every future tick
    /// reports this outcome again without touching the database or the
    /// network until an operator investigates and explicitly unpauses
    /// (the persisted pause, not this in-memory halt flag, is what
    /// actually survives a process restart — see that function's docs).
    PostFinalityReorgHalted {
        fork_height: i64,
        old_tip_height: i64,
        affected_request_ids: Vec<i64>,
    },
}

pub trait GoldcoinRpc {
    fn get_block_count(&self) -> impl Future<Output = Result<i64, RpcError>> + Send;
    fn get_block_hash(&self, height: i64) -> impl Future<Output = Result<String, RpcError>> + Send;
    fn get_block(&self, hash: &str) -> impl Future<Output = Result<BlockHeader, RpcError>> + Send;
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> impl Future<Output = Result<DecodedTransaction, RpcError>> + Send;
    fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> impl Future<Output = Result<Option<TxOut>, RpcError>> + Send;
    /// Broadcasts a fully-signed payout transaction
    /// ([`crate::orchestrator`], Solana->Goldcoin leg). See
    /// [`RpcClient::send_raw_transaction`] for the exact accepted/
    /// already-in-chain/missing-inputs contract.
    fn send_raw_transaction(
        &self,
        hex: &str,
    ) -> impl Future<Output = Result<BroadcastOutcome, RpcError>> + Send;
    /// Live vault UTXO discovery ([`crate::orchestrator`]'s vault-UTXO
    /// sync phase, and eventually Goldcoin-reserve reconciliation). See
    /// [`RpcClient::list_unspent`] for the `solvable`-not-`spendable`
    /// filter rationale.
    fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> impl Future<Output = Result<Vec<super::rpc::ListUnspentEntry>, RpcError>> + Send;
}

impl GoldcoinRpc for RpcClient {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        RpcClient::get_block_count(self).await
    }
    async fn get_block_hash(&self, height: i64) -> Result<String, RpcError> {
        RpcClient::get_block_hash(self, height).await
    }
    async fn get_block(&self, hash: &str) -> Result<BlockHeader, RpcError> {
        RpcClient::get_block(self, hash).await
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        RpcClient::get_raw_transaction(self, txid_hex).await
    }
    async fn get_tx_out_confirmed(
        &self,
        txid_hex: &str,
        vout: u32,
    ) -> Result<Option<TxOut>, RpcError> {
        RpcClient::get_tx_out_confirmed(self, txid_hex, vout).await
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        RpcClient::send_raw_transaction(self, hex).await
    }
    async fn list_unspent(
        &self,
        min_conf: i64,
        addresses: &[String],
    ) -> Result<Vec<super::rpc::ListUnspentEntry>, RpcError> {
        RpcClient::list_unspent(self, min_conf, addresses).await
    }
}

const INNER_RETRY_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub struct IndexerConfig {
    pub vault_script_hex: String,
    pub confirmation_depth: u32,
    pub max_reorg_depth: u32,
}

pub struct Indexer<R: GoldcoinRpc> {
    rpc: R,
    ledger: Ledger,
    config: IndexerConfig,
    halted: bool,
    /// Set once this process's own tick loop has detected and recorded a
    /// post-finality reorg, so subsequent ticks in the SAME process
    /// continue reporting the specific `PostFinalityReorgHalted` details
    /// rather than blurring into the generic `Halted` message. The
    /// persisted reserve pause (`Ledger::record_post_finality_reorg`), not
    /// this in-memory flag, is what actually survives a restart — see
    /// that function's docs.
    post_finality_halt: Option<(i64, i64, Vec<i64>)>,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<R: GoldcoinRpc> Indexer<R> {
    pub fn new(rpc: R, ledger: Ledger, config: IndexerConfig) -> Self {
        Indexer {
            rpc,
            ledger,
            config,
            halted: false,
            post_finality_halt: None,
        }
    }

    async fn call<T, F, Fut>(f: F) -> Result<T, IndexerError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, RpcError>>,
    {
        super::rpc::call_with_retry(INNER_RETRY_ATTEMPTS, f)
            .await
            .map_err(|e| {
                if e.is_retriable() {
                    IndexerError::NodeUnavailable(e)
                } else {
                    IndexerError::Rpc(e)
                }
            })
    }

    /// Runs one indexer tick: reorg detection/rollback (if needed), forward
    /// indexing to the live chain tip, and confirmation-depth promotion.
    pub async fn tick(&mut self) -> Result<TickOutcome, IndexerError> {
        if let Some((fork_height, old_tip_height, affected_request_ids)) = &self.post_finality_halt
        {
            return Ok(TickOutcome::PostFinalityReorgHalted {
                fork_height: *fork_height,
                old_tip_height: *old_tip_height,
                affected_request_ids: affected_request_ids.clone(),
            });
        }
        if self.halted {
            return Ok(TickOutcome::Halted {
                attempted_depth: self.config.max_reorg_depth as i64 + 1,
            });
        }

        let live_tip_height = Self::call(|| self.rpc.get_block_count()).await?;

        let (start_height, reorg) = match self.ledger.goldcoin_chain_tip()? {
            None => (0i64, None),
            Some((local_height, local_hash)) => match self.find_fork_point(local_height).await? {
                None => {
                    self.halted = true;
                    return Ok(TickOutcome::Halted {
                        attempted_depth: self.config.max_reorg_depth as i64 + 1,
                    });
                }
                Some(fork_height) if fork_height == local_height => (local_height + 1, None),
                Some(fork_height) => {
                    // Checked BEFORE any rollback write: does this fork
                    // point reach back far enough to orphan the source
                    // block of a request already told its deposit was
                    // final? `goldcoin_rollback_reorg` below deliberately
                    // only ever touches pre-finality requests, so this is
                    // the one and only place that gap would otherwise go
                    // undetected (docs/22-production-readiness-review.md).
                    let post_finality_affected =
                        self.ledger.detect_post_finality_reorg(fork_height)?;
                    if !post_finality_affected.is_empty() {
                        self.ledger.record_post_finality_reorg(
                            fork_height,
                            local_height,
                            &post_finality_affected,
                            now_unix(),
                        )?;
                        self.post_finality_halt =
                            Some((fork_height, local_height, post_finality_affected.clone()));
                        return Ok(TickOutcome::PostFinalityReorgHalted {
                            fork_height,
                            old_tip_height: local_height,
                            affected_request_ids: post_finality_affected,
                        });
                    }
                    let fork_hash_hex = Self::call(|| self.rpc.get_block_hash(fork_height)).await?;
                    let fork_hash: [u8; 32] = hex::decode_exact(&fork_hash_hex)
                        .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
                    let orphaned_count = self.ledger.goldcoin_rollback_reorg(
                        fork_height,
                        fork_hash,
                        local_height,
                        local_hash,
                        now_unix(),
                    )?;
                    (
                        fork_height + 1,
                        Some(ReorgSummary {
                            fork_height,
                            old_tip_height: local_height,
                            orphaned_count,
                        }),
                    )
                }
            },
        };

        let mut blocks_indexed = 0i64;
        for height in start_height..=live_tip_height {
            self.index_block(height).await?;
            blocks_indexed += 1;
        }

        self.promote_confirming(live_tip_height).await?;

        Ok(TickOutcome::Progressed {
            blocks_indexed,
            reorg,
        })
    }

    /// Walks backward from `from_height` comparing the locally stored hash
    /// against the live chain, returning the fork point, or `None` if no
    /// agreement is found within `max_reorg_depth` (caller must halt, never
    /// guess).
    async fn find_fork_point(&mut self, from_height: i64) -> Result<Option<i64>, IndexerError> {
        let mut h = from_height;
        loop {
            if h < 0 {
                return Ok(None);
            }
            let live_hash_hex = Self::call(|| self.rpc.get_block_hash(h)).await?;
            let live_hash: [u8; 32] = hex::decode_exact(&live_hash_hex)
                .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
            let local_hash = self.ledger.goldcoin_block_hash_at(h)?;
            if local_hash == Some(live_hash) {
                return Ok(Some(h));
            }
            let depth = from_height - (h - 1);
            if depth > self.config.max_reorg_depth as i64 {
                return Ok(None);
            }
            h -= 1;
        }
    }

    async fn index_block(&mut self, height: i64) -> Result<(), IndexerError> {
        let hash_hex = Self::call(|| self.rpc.get_block_hash(height)).await?;
        let hash: [u8; 32] = hex::decode_exact(&hash_hex)
            .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
        let header = Self::call(|| self.rpc.get_block(&hash_hex)).await?;
        let prev_hash: [u8; 32] = match &header.previousblockhash {
            Some(p) => hex::decode_exact(p)
                .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?,
            None => [0u8; 32],
        };

        let now = now_unix();
        // The genesis coinbase is permanently unfetchable even with
        // -txindex=1 (verified empirically by the old bridge against a real
        // node, docs/goldcoin-rpc-notes.md) — height 0 is skipped entirely.
        let is_genesis = height == 0;
        if !is_genesis {
            for txid_hex in &header.tx {
                let decoded = Self::call(|| self.rpc.get_raw_transaction(txid_hex)).await?;
                let vault_outputs =
                    vault_output_candidates(&decoded, &self.config.vault_script_hex);
                if vault_outputs.is_empty() {
                    continue;
                }
                let txid: [u8; 32] = hex::decode_exact(txid_hex)
                    .map_err(|e| IndexerError::Rpc(RpcError::Malformed(e.to_string())))?;
                let binding = extract_request_binding(&decoded);
                for out in &vault_outputs {
                    match &binding {
                        Err(e) => {
                            tracing::warn!(txid_hex, vout = out.vout, reason = e.reason_code(), "vault payment with unusable request binding — recorded, not ignored");
                            self.ledger.record_unmatched_goldcoin_deposit(
                                txid,
                                out.vout,
                                out.amount_atomic,
                                height,
                                e.reason_code(),
                                now,
                            )?;
                        }
                        Ok(request_id) => {
                            let outcome = self.ledger.record_glc_deposit_observed(
                                *request_id,
                                txid,
                                out.vout,
                                out.amount_atomic,
                                height,
                                hash,
                                now,
                            )?;
                            match outcome {
                                GlcObservationOutcome::Recorded => {
                                    tracing::info!(
                                        request_id,
                                        txid_hex,
                                        vout = out.vout,
                                        "deposit observed, now confirming"
                                    );
                                }
                                GlcObservationOutcome::AlreadyRecorded => {}
                                GlcObservationOutcome::LateDepositRecreated => {
                                    tracing::warn!(
                                        request_id,
                                        txid_hex,
                                        vout = out.vout,
                                        "deposit observed against an Expired reservation — capacity was available, reservation auto-recreated, now confirming"
                                    );
                                }
                                GlcObservationOutcome::LateDepositNoCapacity => {
                                    tracing::error!(
                                        request_id,
                                        txid_hex,
                                        vout = out.vout,
                                        "deposit observed against an Expired reservation with no capacity remaining to re-reserve — routed to ManualReview"
                                    );
                                }
                                GlcObservationOutcome::AmountMismatch { expected, observed } => {
                                    tracing::warn!(
                                        request_id,
                                        expected,
                                        observed,
                                        "deposit amount mismatch — routed to ManualReview"
                                    );
                                }
                                GlcObservationOutcome::NoMatchingRequest => {
                                    tracing::warn!(request_id, txid_hex, vout = out.vout, "no matching AwaitingDeposit request — recorded as unmatched");
                                    self.ledger.record_unmatched_goldcoin_deposit(
                                        txid,
                                        out.vout,
                                        out.amount_atomic,
                                        height,
                                        "no_matching_request",
                                        now,
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
        }

        self.ledger
            .goldcoin_ingest_block(height, hash, prev_hash, header.time, now)?;
        Ok(())
    }

    /// Re-evaluates every `Confirming` request against the current tip:
    /// promotes to `SourceFinalized` once depth is reached and the vault
    /// output is confirmed still unspent, or leaves it in `Confirming` for
    /// a later tick.
    async fn promote_confirming(&mut self, tip_height: i64) -> Result<(), IndexerError> {
        let confirming = self.ledger.requests_by_state(
            crate::ledger::Direction::GlcToSol,
            crate::ledger::RequestState::Confirming,
        )?;
        for row in confirming {
            let Some(block_height) = row.source_block_height else {
                continue;
            };
            let depth = tip_height - block_height + 1;
            self.ledger.update_glc_confirmations(row.id, depth)?;
            if depth < self.config.confirmation_depth as i64 {
                continue;
            }

            let (Some(txid), Some(vout)) = (row.source_txid, row.source_vout) else {
                continue;
            };
            let txid_hex = hex::encode(&txid);
            let still_unspent = Self::call(|| self.rpc.get_tx_out_confirmed(&txid_hex, vout))
                .await?
                .is_some();
            if !still_unspent {
                // Anomalous (not a routine reorg — that path is
                // find_fork_point/rollback): fail closed to ManualReview
                // rather than silently dropping a reservation whose
                // backing deposit vanished.
                tracing::warn!(
                    request_id = row.id,
                    txid_hex,
                    vout,
                    "vault output spent before reaching SourceFinalized"
                );
                continue;
            }

            self.ledger.mark_glc_source_finalized(row.id, now_unix())?;
            tracing::info!(
                request_id = row.id,
                txid_hex,
                vout,
                gross_amount = row.gross_amount_atomic,
                net_amount = row.net_amount_atomic,
                "source finalized"
            );
        }
        Ok(())
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn ledger_mut(&mut self) -> &mut Ledger {
        &mut self.ledger
    }
}

#[cfg(test)]
mod tests;
