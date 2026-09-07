//! The Robinhood deposit indexer's tick loop: chain-id verification,
//! reorg reconciliation, chunked log scanning, durable cursor advance,
//! and confirmation-depth finality.
//!
//! # What one tick does, in order
//!
//! 1. **Refuse to run if halted.** The halt is persisted, so a restart
//!    does not clear it.
//! 2. **Verify the chain id**, every tick, not once at startup — an
//!    endpoint can be repointed underneath a running process.
//! 3. **Read the head** (`eth_blockNumber`).
//! 4. **Reconcile the cursor with the live chain.** If the anchored block
//!    is still canonical, nothing happened. If it is not, walk the
//!    retained anchors for one that still matches and roll back to it.
//! 5. **Scan** `cursor+1 ..= head` in bounded chunks, committing each
//!    chunk's observations and its cursor advance in ONE transaction.
//! 6. **Promote** every provisional observation that has reached the
//!    configured confirmation depth.
//!
//! # Why a matching anchor is enough to prove a whole ancestry
//!
//! A block hash commits to its parent hash, which commits to its parent,
//! and so on to genesis. So if the block this service anchored on at
//! height `N` still has the same hash on the live chain, every block at
//! or below `N` that this service ever saw is still canonical — no
//! per-block verification needed, and no complete local copy of the chain
//! either. That is the same property `goldcoin::indexer::find_fork_point`
//! relies on; the only difference is that the anchors here are SPARSE,
//! because an EVM scanner reads log ranges rather than individual blocks
//! and never sees most headers.
//!
//! # The three ways this halts, and why each is a halt
//!
//! Halting is not the default response to trouble. A transport failure,
//! a slow endpoint, a missing block — those are errors: the tick fails,
//! the cursor does not move, and the next tick retries. A halt is
//! reserved for states where retrying cannot help and continuing would
//! record something false.
//!
//! - **`ChainIdMismatch`** — the endpoint is not the network this
//!   deployment is configured for. Continuing would index a different
//!   chain's deposits under this bridge's identity.
//! - **`PostFinalityReorg`** / **`ReorgBeyondRetainedAnchors`** — a block
//!   this service called irreversible was not, or the fork point is
//!   beyond anything it can still prove. Both mean a durable claim was
//!   wrong; neither is guessed past.
//! - **`ObservationConflict`** — two events claim one durable identity
//!   and disagree. See `crate::ledger::robinhood`.
//! - **`UnexpectedContractRoute`** — a `DepositCreated` log named an
//!   outbound or unknown route. `GlcRobinhoodBridge::deposit` reverts
//!   before emitting for those and the event is emitted nowhere else, so
//!   this cannot come from the contract this service thinks it is
//!   watching. Skipping it would mean continuing to index an unknown
//!   contract as if it were the bridge.
//!
//! Every halt is Robinhood-local. It pauses no reserve and closes no
//! route, because the Robinhood routes are already closed and this
//! indexer settles nothing — letting an observation-only fault stop live
//! Solana<->Goldcoin traffic would turn a visibility problem into an
//! outage.
//!
//! # Observation is not admission
//!
//! Everything this module records lands in
//! `robinhood_deposit_observations`. Nothing here calls
//! `Ledger::create_request`, `fold_sol_deposit`, or any reserve
//! operation; none of them would accept a Robinhood route anyway, since
//! [`crate::routes::Route::as_direction`] returns `None` for all four and
//! every value-moving function requires a `Direction`. A deposit on a
//! disabled route is written down and left alone — visible to an
//! operator, executable by nobody.

use std::collections::BTreeMap;
use std::future::Future;

use thiserror::Error;

use crate::evm::hash::EvmBlockHash;
use crate::evm::EvmAddress;
use crate::ledger::{
    Ledger, LedgerError, RobinhoodDepositObservation, RobinhoodHalt, RobinhoodHaltReason,
};

use super::config::RobinhoodIndexerConfig;
use super::deposit_event::{decode_deposit_created, deposit_created_topic0, DepositDecodeError};
use super::health::{RobinhoodHealth, RobinhoodRpcErrorClass};
use super::rpc::{call_with_retry, EvmLogFilter, EvmRpc, EvmRpcError};

/// Matches `goldcoin::indexer`/`solana::indexer`: three attempts per
/// logical call, so a single dropped connection does not fail a whole
/// tick, while a genuinely down endpoint is reported promptly rather than
/// hidden behind a long retry.
const INNER_RETRY_ATTEMPTS: u32 = 3;

#[derive(Debug, Error)]
pub enum RobinhoodIndexerError {
    #[error("Robinhood EVM endpoint unavailable: {0}")]
    NodeUnavailable(EvmRpcError),
    #[error("Robinhood EVM RPC error: {0}")]
    Rpc(EvmRpcError),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    /// A log carrying the `DepositCreated` topic could not be decoded.
    /// Never skipped: the topic filter means the node was asked for
    /// exactly this event, so an undecodable one is a malformed or
    /// hostile response, not an event for somebody else.
    #[error("could not decode a DepositCreated log at {tx_hash}#{log_index}: {source}")]
    Decode {
        tx_hash: String,
        log_index: u64,
        #[source]
        source: DepositDecodeError,
    },
    /// A block the scan needs is absent. The tick fails and retries; the
    /// cursor is never advanced past a block whose identity is unknown.
    #[error("Robinhood block {0} does not exist on the endpoint, though the head is above it")]
    MissingBlock(u64),
    /// A log claimed to be in a block whose live hash is different — the
    /// node served a log from a chain it is no longer on. Transient by
    /// nature (the next tick re-reads), and refused rather than recorded,
    /// because the block hash is part of what the observation asserts.
    #[error(
        "a log reported block hash {log_hash} for block {block}, but the endpoint's block {block} \
         is {live_hash} — refusing to record an observation against a block the node itself does \
         not agree on"
    )]
    LogBlockHashMismatch {
        block: u64,
        log_hash: EvmBlockHash,
        live_hash: EvmBlockHash,
    },
    /// Belt and braces over [`super::rpc::EvmRpc::logs`]'s own filter
    /// check: a decoded event whose emitting contract is not the
    /// configured one.
    #[error("a DepositCreated log came from {found} but the configured bridge is {expected}")]
    ContractMismatch {
        expected: EvmAddress,
        found: EvmAddress,
    },
}

/// What a reorg reconciliation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodReorgSummary {
    pub fork_block: u64,
    pub old_cursor_block: u64,
    /// Provisional observations tombstoned. Zero is ordinary — most
    /// reorgs orphan no deposit at all.
    pub orphaned_observations: i64,
}

impl RobinhoodReorgSummary {
    pub fn depth_blocks(&self) -> u64 {
        self.old_cursor_block.saturating_sub(self.fork_block)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RobinhoodTickOutcome {
    /// The indexer is stopped and did nothing. Reported every tick, with
    /// the same reason, until an operator clears the persisted halt.
    Halted {
        reason: RobinhoodHaltReason,
        detail: String,
    },
    Progressed {
        head: u64,
        /// The cursor AFTER this tick. `None` only when the configured
        /// start block is still above the head, so nothing has been
        /// scanned yet.
        cursor: Option<u64>,
        blocks_scanned: u64,
        /// New observations written this tick.
        recorded: u32,
        /// Re-observations that matched an existing row exactly — the
        /// normal result of a rescan or a restart, and the visible proof
        /// that idempotency did its job.
        already_recorded: u32,
        /// Observations promoted to `Final` this tick.
        finalized: usize,
        reorg: Option<RobinhoodReorgSummary>,
    },
}

/// Watches one `GlcRobinhoodBridge` deployment.
///
/// Generic over [`EvmRpc`] for the same reason the Goldcoin indexer is
/// generic over its RPC trait: every property worth testing here — reorg
/// reconciliation, finality, idempotency, halting — needs responses no
/// live endpoint will produce on demand.
pub struct RobinhoodIndexer<R: EvmRpc> {
    #[cfg(test)]
    pub(crate) rpc: R,
    #[cfg(not(test))]
    rpc: R,
    #[cfg(test)]
    pub(crate) ledger: Ledger,
    #[cfg(not(test))]
    ledger: Ledger,
    config: RobinhoodIndexerConfig,
    health: std::sync::Arc<RobinhoodHealth>,
}

impl<R: EvmRpc> RobinhoodIndexer<R> {
    pub fn new(
        rpc: R,
        ledger: Ledger,
        config: RobinhoodIndexerConfig,
        health: std::sync::Arc<RobinhoodHealth>,
    ) -> Self {
        RobinhoodIndexer {
            rpc,
            ledger,
            config,
            health,
        }
    }

    pub fn config(&self) -> &RobinhoodIndexerConfig {
        &self.config
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn health(&self) -> &std::sync::Arc<RobinhoodHealth> {
        &self.health
    }

    /// Mutable ledger access for the loop's own tests, which need to set
    /// up a persisted halt before running it. Test builds only — the
    /// production API is deliberately read-only about the ledger, so the
    /// tick loop stays the only thing that writes observations.
    #[cfg(test)]
    pub(crate) fn ledger_mut_for_tests(&mut self) -> &mut Ledger {
        &mut self.ledger
    }

    /// Retries a retriable RPC failure, then classifies what is left —
    /// the same wrapper, with the same attempt count and the same
    /// unavailable/definitive split, that `goldcoin::indexer` and
    /// `solana::indexer` use.
    async fn call<T, F, Fut>(f: F) -> Result<T, RobinhoodIndexerError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, EvmRpcError>>,
    {
        call_with_retry(INNER_RETRY_ATTEMPTS, f).await.map_err(|e| {
            if e.is_retriable() {
                RobinhoodIndexerError::NodeUnavailable(e)
            } else {
                RobinhoodIndexerError::Rpc(e)
            }
        })
    }

    /// One tick. See the module docs for the order and the halt rules.
    pub async fn tick(&mut self, now: i64) -> Result<RobinhoodTickOutcome, RobinhoodIndexerError> {
        if let Some(halt) = self.ledger.robinhood_halt()? {
            self.health.set_halt(Some(halt.clone()));
            return Ok(RobinhoodTickOutcome::Halted {
                reason: halt.reason,
                detail: halt.detail,
            });
        }
        self.health.set_halt(None);

        // 1. The endpoint must be the configured network — checked before
        // anything is read FROM it, and on every tick.
        let observed_chain_id = match Self::call(|| self.rpc.chain_id()).await {
            Ok(id) => id,
            Err(e) => {
                report_rpc_error(&self.health, &e, now);
                return Err(e);
            }
        };
        if observed_chain_id != self.config.chain_id {
            return self.halt(
                RobinhoodHaltReason::ChainIdMismatch,
                format!(
                    "endpoint reports chain id {} but this deployment is configured for {}",
                    observed_chain_id, self.config.chain_id
                ),
                now,
            );
        }

        let head = match Self::call(|| self.rpc.block_number()).await {
            Ok(head) => head,
            Err(e) => {
                report_rpc_error(&self.health, &e, now);
                return Err(e);
            }
        };

        // 2. Reconcile the cursor with the live chain before reading
        // anything new, so a rescan never collides with rows a reorg has
        // already invalidated.
        let cursor = self.ledger.robinhood_scan_cursor()?;
        let (scan_from, reorg) = match cursor {
            // No cursor: the configured start block, never a guess and
            // never "wherever the head happens to be". `RobinhoodIndexer
            // Config` makes the field mandatory precisely so this branch
            // has something to use.
            None => (self.config.start_block, None),
            Some((cursor_block, cursor_hash)) => {
                match self.canonical_hash(cursor_block, now).await? {
                    Some(live) if live == EvmBlockHash::from_bytes(cursor_hash) => {
                        (cursor_block.saturating_add(1), None)
                    }
                    // Either the height now holds a different block, or
                    // the chain no longer reaches it. Both are reorgs as
                    // far as this cursor is concerned.
                    _ => match self.reconcile_reorg(cursor_block, cursor_hash, now).await? {
                        ReorgReconciliation::Halted(outcome) => return Ok(outcome),
                        ReorgReconciliation::RolledBack {
                            fork_block,
                            summary,
                        } => (fork_block.saturating_add(1), Some(summary)),
                    },
                }
            }
        };

        // 3. Scan forward in bounded chunks.
        let mut blocks_scanned = 0u64;
        let mut recorded = 0u32;
        let mut already_recorded = 0u32;
        let mut from = scan_from;
        while from <= head {
            let to = from
                .saturating_add(self.config.max_log_block_range - 1)
                .min(head);
            match self.scan_chunk(from, to, head, now).await {
                Ok(applied) => {
                    recorded += applied.recorded;
                    already_recorded += applied.already_recorded;
                }
                Err(ScanFailure::Halt(outcome)) => return Ok(outcome),
                Err(ScanFailure::Error(e)) => return Err(e),
            }
            blocks_scanned += to - from + 1;
            from = to.saturating_add(1);
        }

        // 4. Finality. Purely a ledger operation: step 2 has already
        // established that the cursor's block — and therefore its whole
        // ancestry, which is where every observation lives — is canonical.
        let finalized =
            self.ledger
                .robinhood_promote_final(head, self.config.confirmation_depth, now)?;

        let cursor_after = self.ledger.robinhood_scan_cursor()?.map(|(block, _)| block);
        self.health.record_tick(
            observed_chain_id,
            head,
            finalized_frontier(head, self.config.confirmation_depth),
            cursor_after,
            self.ledger.robinhood_observation_summary()?,
            now,
        );

        Ok(RobinhoodTickOutcome::Progressed {
            head,
            cursor: cursor_after,
            blocks_scanned,
            recorded,
            already_recorded,
            finalized: finalized.len(),
            reorg,
        })
    }

    /// Reads and applies one `[from, to]` chunk. The observations and the
    /// cursor advance to `to` are one commit — see
    /// [`Ledger::robinhood_apply_scan_range`].
    async fn scan_chunk(
        &mut self,
        from: u64,
        to: u64,
        head: u64,
        now: i64,
    ) -> Result<crate::ledger::RobinhoodRangeApplied, ScanFailure> {
        let filter = EvmLogFilter {
            from_block: from,
            to_block: to,
            address: self.config.bridge_contract,
            topic0: deposit_created_topic0(),
        };
        let logs = match Self::call(|| self.rpc.logs(&filter)).await {
            Ok(logs) => logs,
            Err(e) => {
                report_rpc_error(&self.health, &e, now);
                return Err(ScanFailure::Error(e));
            }
        };

        let mut observations = Vec::new();
        let mut deposit_blocks: BTreeMap<u64, EvmBlockHash> = BTreeMap::new();
        for log in &logs {
            if log.removed {
                // The node is telling us this log is not canonical. It is
                // not recorded — and nothing is lost by that: if the block
                // really did reorg, the next tick's anchor check rolls the
                // cursor back and rescans; if it did not, the log will be
                // returned again without the flag.
                tracing::warn!(
                    tx_hash = %log.tx_hash,
                    log_index = log.log_index,
                    block = log.block_number,
                    "Robinhood endpoint returned a removed log for a closed range — skipped, \
                     the next tick's reorg check is what decides canonicality"
                );
                continue;
            }
            let event = match decode_deposit_created(log) {
                Ok(event) => event,
                // Cannot happen under a topic0 filter, but a node that
                // ignored the filter is answering a different question;
                // the log is not this service's business either way.
                Err(DepositDecodeError::NotDepositCreated) => continue,
                Err(DepositDecodeError::NotAnInboundRoute { route }) => {
                    return Err(ScanFailure::Halt(
                        self.halt(
                            RobinhoodHaltReason::UnexpectedContractRoute,
                            format!(
                                "log {}#{} names contract route {route:#04x}, which \
                                 GlcRobinhoodBridge cannot emit a deposit for — the configured \
                                 address is not the contract this service expects",
                                log.tx_hash, log.log_index
                            ),
                            now,
                        )
                        .map_err(ScanFailure::Error)?,
                    ));
                }
                Err(source) => {
                    return Err(ScanFailure::Error(RobinhoodIndexerError::Decode {
                        tx_hash: log.tx_hash.to_string(),
                        log_index: log.log_index,
                        source,
                    }))
                }
            };
            if event.contract != self.config.bridge_contract {
                return Err(ScanFailure::Error(
                    RobinhoodIndexerError::ContractMismatch {
                        expected: self.config.bridge_contract,
                        found: event.contract,
                    },
                ));
            }
            deposit_blocks.insert(event.block_number(), event.block_hash());
            observations.push(RobinhoodDepositObservation {
                source_contract: event.contract.to_bytes(),
                obligation_index: event.obligation_index,
                route: event.route,
                depositor: event.depositor.to_bytes(),
                destination: event.destination.clone(),
                amount_robinhood_atomic: event.amount_word.to_be_bytes(),
                amount_canonical_atomic: event.canonical_amount.0,
                tx_hash: event.location.id.tx_hash.to_bytes(),
                log_index: event.location.id.log_index,
                block_number: event.block_number(),
                block_hash: event.block_hash().to_bytes(),
            });
        }

        // Every block that held a deposit is re-read and its hash checked
        // against what the log claimed. Deposits are rare, so this costs
        // one call per block that actually mattered — and the block hash
        // is part of what the observation asserts, so recording one the
        // endpoint does not itself stand behind is not acceptable.
        let mut anchors: Vec<(u64, [u8; 32])> = Vec::with_capacity(deposit_blocks.len());
        for (block, claimed) in &deposit_blocks {
            let live = self
                .canonical_hash(*block, now)
                .await
                .map_err(ScanFailure::Error)?
                .ok_or_else(|| ScanFailure::Error(RobinhoodIndexerError::MissingBlock(*block)))?;
            if live != *claimed {
                return Err(ScanFailure::Error(
                    RobinhoodIndexerError::LogBlockHashMismatch {
                        block: *block,
                        log_hash: *claimed,
                        live_hash: live,
                    },
                ));
            }
            anchors.push((*block, live.to_bytes()));
        }

        // The STABILITY anchor: the block at the finality frontier. It is
        // the deepest point a routine reorg can legitimately fork at (a
        // fork below it would contradict something already called final),
        // so it is the deepest rollback target that ever needs to exist —
        // and without it, a fresh catch-up that scanned the whole chain in
        // one wide range would hold only the anchors of its own last
        // block and its deposits, leaving an ordinary shallow reorg with
        // nothing to roll back to.
        //
        // Clamped to the configured start block so a rollback can never
        // take the scanner below the history this deployment declared in
        // scope, and skipped when the frontier is above this chunk,
        // because nothing above the cursor has been scanned yet.
        let stability_anchor = finalized_frontier(head, self.config.confirmation_depth)
            .unwrap_or(0)
            .max(self.config.start_block);
        if stability_anchor < to {
            let hash = self
                .canonical_hash(stability_anchor, now)
                .await
                .map_err(ScanFailure::Error)?
                .ok_or_else(|| {
                    ScanFailure::Error(RobinhoodIndexerError::MissingBlock(stability_anchor))
                })?;
            anchors.push((stability_anchor, hash.to_bytes()));
        }

        let cursor_hash = self
            .canonical_hash(to, now)
            .await
            .map_err(ScanFailure::Error)?
            .ok_or_else(|| ScanFailure::Error(RobinhoodIndexerError::MissingBlock(to)))?;

        match self.ledger.robinhood_apply_scan_range(
            &observations,
            &anchors,
            to,
            cursor_hash.to_bytes(),
            self.config.confirmation_depth,
            now,
        ) {
            Ok(applied) => Ok(applied),
            // The range's transaction is already rolled back, so the
            // cursor did not move and nothing was half-written.
            Err(LedgerError::RobinhoodObservationConflict(conflict)) => Err(ScanFailure::Halt(
                self.halt(
                    RobinhoodHaltReason::ObservationConflict,
                    conflict.to_string(),
                    now,
                )
                .map_err(ScanFailure::Error)?,
            )),
            Err(LedgerError::RobinhoodPostFinalityReorg {
                fork_block,
                finalized_above,
            }) => Err(ScanFailure::Halt(
                self.halt(
                    RobinhoodHaltReason::PostFinalityReorg,
                    format!(
                        "applying a scan range would have contradicted {finalized_above} \
                         finalized observation(s) above block {fork_block}"
                    ),
                    now,
                )
                .map_err(ScanFailure::Error)?,
            )),
            Err(e) => Err(ScanFailure::Error(RobinhoodIndexerError::Ledger(e))),
        }
    }

    /// Walks the retained anchors, newest first, for one that still
    /// matches the live chain, and rolls back to it.
    ///
    /// The result is the newest anchor that still agrees — which is at or
    /// BELOW the true fork point, never above it. Rolling back further
    /// than strictly necessary is safe (the rescan is idempotent, and any
    /// provisional sighting it tombstones is simply re-observed), while
    /// rolling back too little would leave a row describing a block that
    /// is no longer on the chain. Anchors are dense in steady state — one
    /// per tick — so the over-rollback is only ever material right after
    /// a wide initial catch-up.
    async fn reconcile_reorg(
        &mut self,
        cursor_block: u64,
        cursor_hash: [u8; 32],
        now: i64,
    ) -> Result<ReorgReconciliation, RobinhoodIndexerError> {
        let anchors = self.ledger.robinhood_scan_anchors_desc()?;
        let mut fork: Option<(u64, EvmBlockHash)> = None;
        for (block, stored) in &anchors {
            if let Some(live) = self.canonical_hash(*block, now).await? {
                if live == EvmBlockHash::from_bytes(*stored) {
                    fork = Some((*block, live));
                    break;
                }
            }
        }

        let Some((fork_block, fork_hash)) = fork else {
            // No retained anchor agrees with the live chain, so the fork
            // point is older than anything this service can still prove.
            // Never guessed at: the pruning rule deliberately keeps one
            // anchor below the finality window precisely so this case is
            // detectable rather than silently unfalsifiable.
            let deepest = anchors.last().map(|(b, _)| *b).unwrap_or(cursor_block);
            // Even the deepest anchor was contradicted, so everything at
            // or above it has been rewritten. If any of that was already
            // recorded as final, the more specific — and more serious —
            // diagnosis is the post-finality one, and that is what the
            // operator is told.
            let finalized_above = self
                .ledger
                .robinhood_final_observations_above(deepest.saturating_sub(1))?;
            if !finalized_above.is_empty() {
                return Ok(ReorgReconciliation::Halted(self.halt(
                    RobinhoodHaltReason::PostFinalityReorg,
                    format!(
                        "a reorg reaching at least back to block {deepest} orphans obligation(s) \
                         {finalized_above:?}, already recorded as final"
                    ),
                    now,
                )?));
            }
            return Ok(ReorgReconciliation::Halted(self.halt(
                RobinhoodHaltReason::ReorgBeyondRetainedAnchors,
                format!(
                    "none of the {} retained scan anchors (down to block {deepest}) still \
                         matches the live chain — the fork point is deeper than this service can \
                         prove",
                    anchors.len(),
                ),
                now,
            )?));
        };

        // Checked BEFORE any rollback write, exactly as the Goldcoin
        // indexer checks `detect_post_finality_reorg` before
        // `goldcoin_rollback_reorg`: the rollback only ever touches
        // provisional rows, so this is the one place the gap would
        // otherwise go unnoticed.
        let finalized_above = self.ledger.robinhood_final_observations_above(fork_block)?;
        if !finalized_above.is_empty() {
            return Ok(ReorgReconciliation::Halted(
                self.halt(
                    RobinhoodHaltReason::PostFinalityReorg,
                    format!(
                        "a reorg back to block {fork_block} orphans obligation(s) {finalized_above:?}, \
                         already recorded as final"
                    ),
                    now,
                )?,
            ));
        }

        let orphaned = self.ledger.robinhood_rollback_reorg(
            fork_block,
            fork_hash.to_bytes(),
            cursor_block,
            cursor_hash,
            now,
        )?;
        let summary = RobinhoodReorgSummary {
            fork_block,
            old_cursor_block: cursor_block,
            orphaned_observations: orphaned,
        };
        self.health.record_reorg(summary.depth_blocks());
        tracing::warn!(
            fork_block,
            old_cursor_block = cursor_block,
            orphaned_observations = orphaned,
            "Robinhood reorg reconciled — provisional observations above the fork were \
             tombstoned and the scan cursor rolled back"
        );
        Ok(ReorgReconciliation::RolledBack {
            fork_block,
            summary,
        })
    }

    /// The live hash at `block`, or `None` if the endpoint has no block
    /// there. Never falls back to a cached or assumed value.
    ///
    /// Takes `&mut self` although it mutates nothing. An `&self` future
    /// would have to hold a shared reference to the whole indexer across
    /// an await, which makes the future `Send` only if the indexer is
    /// `Sync` — and it is not, because it owns a [`Ledger`] (a rusqlite
    /// `Connection` is `Send` but not `Sync`). An exclusive borrow needs
    /// only `Send`, so this is what lets the tick loop be spawned as a
    /// task at all.
    async fn canonical_hash(
        &mut self,
        block: u64,
        now: i64,
    ) -> Result<Option<EvmBlockHash>, RobinhoodIndexerError> {
        let result = Self::call(|| self.rpc.block_by_number(block)).await;
        match result {
            Ok(found) => Ok(found.map(|block| block.hash)),
            Err(e) => {
                report_rpc_error(&self.health, &e, now);
                Err(e)
            }
        }
    }

    /// Persists a halt and returns the tick outcome that reports it.
    ///
    /// Deliberately not `async`: it must be callable from the decode and
    /// apply paths, which are not, and it performs no I/O — the halt is a
    /// single local ledger write.
    fn halt(
        &mut self,
        reason: RobinhoodHaltReason,
        detail: String,
        now: i64,
    ) -> Result<RobinhoodTickOutcome, RobinhoodIndexerError> {
        tracing::error!(
            reason = reason.as_str(),
            detail = %detail,
            "Robinhood indexer halted — it will not resume until an operator clears the \
             persisted halt; no reserve is paused and no route state changed"
        );
        self.ledger
            .robinhood_record_halt(reason, detail.as_str(), now)?;
        // Re-read rather than reconstruct: the first halt wins, so what
        // was actually stored may predate this one.
        let stored = self.ledger.robinhood_halt()?;
        self.health.set_halt(stored.clone());
        let RobinhoodHalt { reason, detail, .. } = stored.unwrap_or(RobinhoodHalt {
            reason,
            detail,
            halted_at: now,
        });
        Ok(RobinhoodTickOutcome::Halted { reason, detail })
    }
}

/// The highest block that is at or beyond `confirmation_depth`
/// confirmations against `head`, counted `head - block + 1` — the same
/// arithmetic `goldcoin::indexer::promote_confirming` uses. `None` when
/// the chain is not yet that tall.
pub fn finalized_frontier(head: u64, confirmation_depth: u64) -> Option<u64> {
    let depth = confirmation_depth.max(1);
    head.saturating_add(1).checked_sub(depth)
}

/// Publishes an RPC failure, classified by the error's own TYPE.
///
/// The class — not a substring of the message — is what decides whether
/// the endpoint was reached, so the answer cannot be changed by anything
/// a node or a dependency writes into an error string. The message itself
/// is redacted on the way in by [`RobinhoodHealth::record_error`]; this
/// function deliberately does no redaction of its own, so there is one
/// filter rather than two that can disagree.
fn report_rpc_error(health: &RobinhoodHealth, error: &RobinhoodIndexerError, now: i64) {
    let class = classify(error);
    health.record_error(class, &error.to_string(), now);
}

/// Maps an indexer error onto the class an operator triages on.
///
/// Exhaustive rather than wildcarded: a new error variant must be given a
/// class deliberately, because falling back to a generic one would make a
/// new failure mode indistinguishable from an old one on the health
/// surface.
fn classify(error: &RobinhoodIndexerError) -> RobinhoodRpcErrorClass {
    match error {
        RobinhoodIndexerError::NodeUnavailable(_) => RobinhoodRpcErrorClass::Transport,
        RobinhoodIndexerError::Rpc(inner) => match inner {
            // A `Transport` reaching here rather than through
            // `NodeUnavailable` still means the endpoint was not reached.
            EvmRpcError::Transport(_) => RobinhoodRpcErrorClass::Transport,
            EvmRpcError::Method { .. } => RobinhoodRpcErrorClass::RpcMethod,
            EvmRpcError::Malformed(_) => RobinhoodRpcErrorClass::MalformedResponse,
        },
        RobinhoodIndexerError::Ledger(_) => RobinhoodRpcErrorClass::Ledger,
        RobinhoodIndexerError::Decode { .. } => RobinhoodRpcErrorClass::Decode,
        RobinhoodIndexerError::MissingBlock(_)
        | RobinhoodIndexerError::LogBlockHashMismatch { .. }
        | RobinhoodIndexerError::ContractMismatch { .. } => {
            RobinhoodRpcErrorClass::ChainDisagreement
        }
    }
}

/// Which of the two ways a chunk scan can fail: an ordinary error to
/// propagate, or a halt that is itself the tick's outcome.
enum ScanFailure {
    Halt(RobinhoodTickOutcome),
    Error(RobinhoodIndexerError),
}

/// The outcome of [`RobinhoodIndexer::reconcile_reorg`].
enum ReorgReconciliation {
    Halted(RobinhoodTickOutcome),
    RolledBack {
        fork_block: u64,
        summary: RobinhoodReorgSummary,
    },
}

#[cfg(test)]
mod tests;
