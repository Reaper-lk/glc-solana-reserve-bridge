//! The orchestrator: wires the indexers, reconciliation, and the internal
//! signer groups into a single per-tick sweep that drives every
//! `bridge_requests` row forward (docs/03-architecture.md,
//! docs/07-implementation-plan.md Phase 4).
//!
//! # Non-blocking, poll-driven, never a single point of trust
//!
//! Every settlement step that touches a chain (submitting
//! `release_from_reserve`/`record_goldcoin_completion` to Solana,
//! broadcasting a Goldcoin payout) is split into a "submit" phase and a
//! separate "poll for confirmation" phase, run as two ordinary tick
//! phases rather than one call that blocks waiting for confirmation. This
//! matches how the rest of this codebase already treats chain state
//! (the Goldcoin/Solana indexers are themselves poll loops) and keeps a
//! single tick bounded and fast — no phase within a tick blocks on chain
//! finality.
//!
//! The orchestrator itself holds no threshold authority: every release and
//! every completion record requires collecting `attestation_threshold`
//! independent [`crate::signing::attestation`] signatures (ed25519) or
//! `vault_threshold` independent [`crate::signing::goldcoin_vault`]
//! partial signatures (secp256k1) — each produced by a signer that
//! re-derives its own claim from the ledger and a live chain read, never
//! from a value the orchestrator hands it. The orchestrator only
//! sequences these independent signers and submits what they jointly
//! produce; it cannot single-handedly authorize anything
//! (docs/02-trust-model.md: 2-of-3 internal threshold custody, not
//! federation).
//!
//! # DEV/TEST KEY POSTURE ONLY
//!
//! The orchestrator holds every [`DevVaultSigner`]/[`DevAttestationSigner`]
//! in this same process for local development and testing — see those
//! modules' docs. Production custody domains are a distinct, later,
//! explicitly-approved piece of work (docs/12-management-decisions.md
//! item 2).
//!
//! # Per-request failure never aborts a tick
//!
//! [`Orchestrator::tick`] processes every eligible request in every phase;
//! a failure attesting/signing/submitting/broadcasting for one request is
//! recorded in [`TickReport::errors`] and that request simply does not
//! progress this tick (retried next tick) — it never stops other requests
//! or other phases from being processed (fail-closed per request, not
//! fail-closed for the whole service).

use std::sync::Arc;

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction as SolanaTransaction;

use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::indexer::{GoldcoinRpc, Indexer, TickOutcome as GoldcoinTickOutcome};
use crate::goldcoin::multisig::{self, PartialSignature};
use crate::goldcoin::rpc::BroadcastOutcome;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{Direction, Ledger, LedgerError, RequestState, ReserveDirection};
use crate::ops::indexer_status::IndexerStatus;
use crate::reconciliation::{self, ReconciliationReport};
use crate::signing::attestation::{
    self, independently_attest_completion, independently_attest_release, AttestationError,
    DevAttestationSigner,
};
use crate::signing::goldcoin_vault::{
    independently_sign, DevLedgerPayoutSource, DevVaultSigner, SigningError,
};
use crate::solana::accounts;
use crate::solana::ed25519;
use crate::solana::indexer::{SolanaIndexer, SolanaTickOutcome};
use crate::solana::instructions;
use crate::solana::rpc::{SolanaRpc, SolanaRpcError};

#[derive(Debug, thiserror::Error)]
pub enum OrchestratorError {
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("attestation error: {0}")]
    Attestation(#[from] AttestationError),
    #[error("vault signing error: {0}")]
    Signing(#[from] SigningError),
    #[error("multisig assembly error: {0}")]
    Multisig(#[from] multisig::MultisigError),
    #[error("solana rpc error: {0}")]
    SolanaRpc(#[from] SolanaRpcError),
    #[error("goldcoin rpc error: {0}")]
    GoldcoinRpc(#[from] crate::goldcoin::rpc::RpcError),
    #[error("attestation signers disagree on the message for request {0} — refusing to proceed")]
    InconsistentAttestation(i64),
    #[error("Goldcoin payout broadcast for request {0} reports missing inputs — vault UTXO conflict, needs operator attention")]
    PayoutBroadcastConflict(i64),
    #[error("request {0} is missing a field required to build its settlement transaction")]
    IncompleteRequest(i64),
}

#[derive(Debug, Clone)]
pub struct OrchestratorConfig {
    pub attestation_threshold: usize,
    pub vault_threshold: usize,
    pub required_goldcoin_confirmations: i64,
    pub fee_rate_per_kb: u64,
    pub dust_threshold: u64,
    pub max_inputs: usize,
    pub reconciliation_tolerance: u64,
    /// Minimum confirmations for a vault output to be synced into
    /// `vault_utxos` and become eligible for coin selection (see
    /// `tick_vault_utxos`).
    pub vault_min_confirmations: i64,
}

#[derive(Debug, Default)]
pub struct TickReport {
    pub goldcoin_indexer: Option<Result<GoldcoinTickOutcome, String>>,
    pub solana_indexer: Option<Result<SolanaTickOutcome, String>>,
    pub expired_reservations: u32,
    pub solana_reconciliation: Option<Result<ReconciliationReport, String>>,
    pub goldcoin_reconciliation: Option<Result<ReconciliationReport, String>>,
    pub releases_submitted: u32,
    pub releases_confirmed: u32,
    pub payouts_built: u32,
    pub payouts_confirmed: u32,
    pub completions_submitted: u32,
    pub completions_confirmed: u32,
    pub errors: Vec<String>,
}

pub struct Orchestrator<GR: GoldcoinRpc, SR: SolanaRpc> {
    goldcoin_indexer: Indexer<GR>,
    solana_indexer: SolanaIndexer<SR>,
    ledger: Ledger,
    goldcoin_rpc: GR,
    solana_rpc: SR,
    vault: MultisigVault,
    vault_signers: Vec<DevVaultSigner>,
    attestation_signers: Vec<DevAttestationSigner>,
    /// Fee payer / transaction submitter for `release_from_reserve` and
    /// `record_goldcoin_completion`. Distinct from the attestation
    /// signers: paying fees and submitting a transaction is not a
    /// custody-authority action — the authority is entirely in the
    /// attestation signatures the transaction carries.
    submitter: Keypair,
    config: OrchestratorConfig,
    /// Updated from this tick loop's own `TickOutcome`/`SolanaTickOutcome`
    /// every tick — what `ops::health`'s `/health` endpoint polls between
    /// ticks to see a halted/stalled indexer that would otherwise be
    /// invisible (see `ops::indexer_status` module docs).
    goldcoin_indexer_status: Arc<IndexerStatus>,
    solana_indexer_status: Arc<IndexerStatus>,
}

impl<GR: GoldcoinRpc, SR: SolanaRpc> Orchestrator<GR, SR> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        goldcoin_indexer: Indexer<GR>,
        solana_indexer: SolanaIndexer<SR>,
        ledger: Ledger,
        goldcoin_rpc: GR,
        solana_rpc: SR,
        vault: MultisigVault,
        vault_signers: Vec<DevVaultSigner>,
        attestation_signers: Vec<DevAttestationSigner>,
        submitter: Keypair,
        config: OrchestratorConfig,
        now: i64,
    ) -> Self {
        Orchestrator {
            goldcoin_indexer,
            solana_indexer,
            ledger,
            goldcoin_rpc,
            solana_rpc,
            vault,
            vault_signers,
            attestation_signers,
            submitter,
            config,
            goldcoin_indexer_status: Arc::new(IndexerStatus::new(now)),
            solana_indexer_status: Arc::new(IndexerStatus::new(now)),
        }
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// Shared liveness status for the Goldcoin indexer — clone the `Arc`
    /// into an `ops::collector::OpsCollector` to expose it over `/health`.
    pub fn goldcoin_indexer_status(&self) -> Arc<IndexerStatus> {
        Arc::clone(&self.goldcoin_indexer_status)
    }

    /// Shared liveness status for the Solana indexer — see
    /// [`Orchestrator::goldcoin_indexer_status`].
    pub fn solana_indexer_status(&self) -> Arc<IndexerStatus> {
        Arc::clone(&self.solana_indexer_status)
    }

    pub async fn tick(&mut self, now: i64) -> TickReport {
        let mut report = TickReport::default();

        let goldcoin_outcome = self.goldcoin_indexer.tick().await;
        match &goldcoin_outcome {
            Ok(GoldcoinTickOutcome::Progressed { reorg, .. }) => {
                self.goldcoin_indexer_status.record_tick(now);
                if let Some(reorg) = reorg {
                    self.goldcoin_indexer_status
                        .record_reorg(reorg.old_tip_height - reorg.fork_height);
                }
            }
            Ok(GoldcoinTickOutcome::Halted { attempted_depth }) => {
                self.goldcoin_indexer_status.record_halt(*attempted_depth);
            }
            Err(_) => {} // transient failure; staleness accumulates, retried next tick
        }
        report.goldcoin_indexer = Some(goldcoin_outcome.map_err(|e| e.to_string()));

        let solana_outcome = self.solana_indexer.tick().await;
        if solana_outcome.is_ok() {
            self.solana_indexer_status.record_tick(now);
        }
        report.solana_indexer = Some(solana_outcome.map_err(|e| e.to_string()));

        report.expired_reservations = match self.ledger.expire_reservations(now) {
            Ok(n) => n,
            Err(e) => {
                report.errors.push(format!("expire_reservations: {e}"));
                0
            }
        };

        self.tick_release_settlements(now, &mut report).await;
        self.tick_release_confirmations(now, &mut report).await;
        self.tick_vault_utxos(now, &mut report).await;
        self.tick_goldcoin_payouts(now, &mut report).await;
        self.tick_goldcoin_payout_confirmations(now, &mut report)
            .await;
        self.tick_goldcoin_completions(now, &mut report).await;
        self.tick_goldcoin_completion_confirmations(now, &mut report)
            .await;

        // Deliberately last: reconciliation compares the live on-chain
        // balance against this service's own bookkeeping
        // (`reserved_liquidity`/`pending_obligations`), and a settlement
        // that lands on-chain is only reflected in that bookkeeping once
        // `tick_release_confirmations`/`tick_goldcoin_completion_confirmations`
        // above have run — a real bug this phase's real-node testing
        // caught: running reconciliation first meant the very first
        // successful release's on-chain balance drop was compared against
        // still-unadjusted bookkeeping and misclassified as an unexplained
        // breach, auto-pausing the reserve after its first legitimate
        // settlement. Running reconciliation after every phase that can
        // move the ledger's own committed/settled totals keeps the
        // comparison honest. Both directions run every tick — Goldcoin's
        // reserve gets exactly the same automatic breach detection as
        // Solana's, not a weaker check (see `tick_goldcoin_reconciliation`).
        report.solana_reconciliation = self.tick_solana_reconciliation(now).await;
        report.goldcoin_reconciliation = self.tick_goldcoin_reconciliation(now).await;

        report
    }

    // --------------------------------------------------------- reconciliation --

    /// Solana-reserve reconciliation: the reserve authority's SPL token
    /// account balance is a clean, already-available live read.
    async fn tick_solana_reconciliation(
        &mut self,
        now: i64,
    ) -> Option<Result<ReconciliationReport, String>> {
        let config = match attestation::fetch_bridge_config(&self.solana_rpc).await {
            Ok(c) => c,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::SolanaReserve,
                    &format!("could not read bridge_config: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let reserve_authority = accounts::reserve_authority_pda();
        let ata =
            accounts::associated_token_address(&reserve_authority, &config.reserve_token_mint);
        let account = match self.solana_rpc.get_account(&ata).await {
            Ok(a) => a,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::SolanaReserve,
                    &format!("could not read reserve token account: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let Some(account) = account else {
            let _ = reconciliation::record_skipped(
                &mut self.ledger,
                ReserveDirection::SolanaReserve,
                "reserve token account does not exist yet",
                now,
            );
            return None;
        };
        let observed_balance = match accounts::decode_token_account_amount(&account.data) {
            Ok(b) => b,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::SolanaReserve,
                    &format!("malformed reserve token account: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        Some(
            reconciliation::reconcile(
                &mut self.ledger,
                ReserveDirection::SolanaReserve,
                observed_balance,
                self.config.reconciliation_tolerance,
                now,
            )
            .map_err(|e| e.to_string()),
        )
    }

    /// Goldcoin-reserve reconciliation: sums a fresh, independent
    /// `listunspent` read against the vault address. Deliberately not the
    /// ledger's own `vault_utxos` cache — `tick_vault_utxos` (run earlier
    /// this same tick) populates that cache from the same kind of read,
    /// and reconciling against a cache this service itself just wrote
    /// would never catch a bug in that write path. Reading the chain
    /// directly here, independent of the service's own bookkeeping, is
    /// the same discipline `tick_solana_reconciliation` already follows
    /// by reading the SPL token account directly rather than trusting a
    /// cached balance.
    async fn tick_goldcoin_reconciliation(
        &mut self,
        now: i64,
    ) -> Option<Result<ReconciliationReport, String>> {
        let addresses = vec![self.vault.address().to_string()];
        let entries = match self
            .goldcoin_rpc
            .list_unspent(self.config.vault_min_confirmations, &addresses)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                let _ = reconciliation::record_skipped(
                    &mut self.ledger,
                    ReserveDirection::GoldcoinReserve,
                    &format!("could not read vault UTXOs: {e}"),
                    now,
                );
                return Some(Err(e.to_string()));
            }
        };
        let observed_balance: u64 = entries
            .iter()
            .filter(|e| e.solvable)
            .map(|e| crate::goldcoin::deposit::glc_to_atomic(e.amount))
            .sum();
        Some(
            reconciliation::reconcile(
                &mut self.ledger,
                ReserveDirection::GoldcoinReserve,
                observed_balance,
                self.config.reconciliation_tolerance,
                now,
            )
            .map_err(|e| e.to_string()),
        )
    }

    // -------------------------------------------------- GlcToSol: release --

    async fn tick_release_settlements(&mut self, now: i64, report: &mut TickReport) {
        let requests = match self
            .ledger
            .requests_by_state(Direction::GlcToSol, RequestState::SourceFinalized)
        {
            Ok(r) => r,
            Err(e) => {
                report
                    .errors
                    .push(format!("requests_by_state(GlcToSol, SourceFinalized): {e}"));
                return;
            }
        };
        for request in requests {
            match self.submit_release(request.id, now).await {
                Ok(()) => report.releases_submitted += 1,
                Err(e) => report
                    .errors
                    .push(format!("release request {}: {e}", request.id)),
            }
        }
    }

    async fn submit_release(&mut self, request_id: i64, now: i64) -> Result<(), OrchestratorError> {
        let mut sigs = Vec::with_capacity(self.config.attestation_threshold);
        let mut message = None;
        for signer in self
            .attestation_signers
            .iter()
            .take(self.config.attestation_threshold)
        {
            let (pubkey, signature, msg) =
                independently_attest_release(signer, &self.ledger, &self.solana_rpc, request_id)
                    .await?;
            if let Some(prev) = &message {
                if prev != &msg {
                    return Err(OrchestratorError::InconsistentAttestation(request_id));
                }
            } else {
                message = Some(msg);
            }
            log_signature_grant(
                &mut self.ledger,
                "attestation",
                &pubkey.to_string(),
                request_id,
                now,
            );
            sigs.push((pubkey, signature));
        }
        let message = message.ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        self.ledger
            .record_attestation(request_id, "release", &message, now)?;

        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        let txid = request
            .source_txid
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let vout = request
            .source_vout
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let recipient = Pubkey::try_from(request.recipient.as_slice())
            .map_err(|_| OrchestratorError::IncompleteRequest(request_id))?;

        let key_set = attestation::fetch_attestation_key_set(&self.solana_rpc).await?;
        let config = attestation::fetch_bridge_config(&self.solana_rpc).await?;

        let proof_ix = ed25519::build_attestation_proof(&sigs, &message);
        let release_ix = instructions::release_from_reserve(
            &self.submitter.pubkey(),
            &config.reserve_token_mint,
            &recipient,
            txid,
            vout,
            request.amount_atomic,
            key_set.epoch,
        );
        let blockhash = self.solana_rpc.get_latest_blockhash().await?;
        let tx = SolanaTransaction::new_signed_with_payer(
            &[proof_ix, release_ix],
            Some(&self.submitter.pubkey()),
            &[&self.submitter],
            blockhash,
        );
        let signature = self.solana_rpc.send_transaction(&tx).await?;
        self.ledger
            .record_release_submitted(request_id, signature_bytes(&signature), now)?;
        Ok(())
    }

    async fn tick_release_confirmations(&mut self, now: i64, report: &mut TickReport) {
        let requests = match self
            .ledger
            .requests_by_state(Direction::GlcToSol, RequestState::DestinationSubmitted)
        {
            Ok(r) => r,
            Err(e) => {
                report.errors.push(format!(
                    "requests_by_state(GlcToSol, DestinationSubmitted): {e}"
                ));
                return;
            }
        };
        for request in requests {
            let destination_txid = match self.ledger.get_destination_txid(request.id) {
                Ok(v) => v,
                Err(e) => {
                    report
                        .errors
                        .push(format!("release request {}: {e}", request.id));
                    continue;
                }
            };
            let Some(sig_bytes) = destination_txid.and_then(|v| <[u8; 64]>::try_from(v).ok())
            else {
                report.errors.push(format!(
                    "release request {}: missing/malformed destination_txid",
                    request.id
                ));
                continue;
            };
            let signature = Signature::from(sig_bytes);
            match self.solana_rpc.get_signature_status(&signature).await {
                Ok(Some(Ok(()))) => match self.ledger.mark_release_confirmed(request.id, now) {
                    Ok(()) => report.releases_confirmed += 1,
                    Err(e) => report
                        .errors
                        .push(format!("release request {}: {e}", request.id)),
                },
                Ok(Some(Err(reason))) => report.errors.push(format!(
                    "release request {} REJECTED on chain: {reason}",
                    request.id
                )),
                Ok(None) => {} // still pending; retried next tick
                Err(e) => report
                    .errors
                    .push(format!("release request {}: {e}", request.id)),
            }
        }
    }

    // ------------------------------------------------- SolToGlc: payout --

    /// Syncs the vault's live UTXO set into the ledger via `listunspent`
    /// (`solvable`, not `spendable` — docs/goldcoin-rpc-notes.md) so coin
    /// selection has real chain data to select from. Nothing else in this
    /// codebase populates `vault_utxos` from a live read — without this
    /// phase, `tick_goldcoin_payouts` would always fail with "insufficient
    /// funds" against a real node (a real gap Phase 6 real-node testing
    /// caught: every existing test seeded `vault_utxos` directly).
    async fn tick_vault_utxos(&mut self, now: i64, report: &mut TickReport) {
        let addresses = vec![self.vault.address().to_string()];
        let entries = match self
            .goldcoin_rpc
            .list_unspent(self.config.vault_min_confirmations, &addresses)
            .await
        {
            Ok(e) => e,
            Err(e) => {
                report.errors.push(format!("list_unspent: {e}"));
                return;
            }
        };
        let observed: Vec<(VaultUtxo, i64, String)> = entries
            .iter()
            .filter(|e| e.solvable)
            .filter_map(|e| {
                let txid: [u8; 32] = crate::goldcoin::hex::decode_exact(&e.txid).ok()?;
                Some((
                    VaultUtxo {
                        txid,
                        vout: e.vout,
                        amount_atomic: crate::goldcoin::deposit::glc_to_atomic(e.amount),
                    },
                    e.confirmations,
                    e.script_pub_key.clone(),
                ))
            })
            .collect();
        if let Err(e) =
            self.ledger
                .sync_vault_utxos(&observed, self.config.vault_min_confirmations, now)
        {
            report.errors.push(format!("sync_vault_utxos: {e}"));
        }
    }

    async fn tick_goldcoin_payouts(&mut self, now: i64, report: &mut TickReport) {
        let requests = match self
            .ledger
            .requests_by_state(Direction::SolToGlc, RequestState::SourceFinalized)
        {
            Ok(r) => r,
            Err(e) => {
                report
                    .errors
                    .push(format!("requests_by_state(SolToGlc, SourceFinalized): {e}"));
                return;
            }
        };
        for request in requests {
            match self.ledger.get_goldcoin_payout(request.id) {
                Ok(Some(_)) => continue, // a previous attempt already exists; needs operator attention if stuck
                Ok(None) => {}
                Err(e) => {
                    report
                        .errors
                        .push(format!("payout request {}: {e}", request.id));
                    continue;
                }
            }
            match self.build_and_broadcast_payout(request.id, now).await {
                Ok(()) => report.payouts_built += 1,
                Err(e) => report
                    .errors
                    .push(format!("payout request {}: {e}", request.id)),
            }
        }
    }

    async fn build_and_broadcast_payout(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let threshold = self.config.vault_threshold;
        let source = DevLedgerPayoutSource {
            ledger: &self.ledger,
        };

        let (first_partial, plan, mut tx) = independently_sign(
            &self.vault_signers[0],
            &self.vault,
            &source,
            request_id,
            0,
            self.config.fee_rate_per_kb,
            self.config.dust_threshold,
            self.config.max_inputs,
        )?;
        let mut partials: Vec<Vec<PartialSignature>> = vec![vec![first_partial]];
        for input_index in 1..plan.inputs.len() {
            let (partial, _, _) = independently_sign(
                &self.vault_signers[0],
                &self.vault,
                &source,
                request_id,
                input_index,
                self.config.fee_rate_per_kb,
                self.config.dust_threshold,
                self.config.max_inputs,
            )?;
            partials.push(vec![partial]);
        }
        for signer in &self.vault_signers[1..threshold] {
            for (input_index, slot) in partials.iter_mut().enumerate() {
                let (partial, _, _) = independently_sign(
                    signer,
                    &self.vault,
                    &source,
                    request_id,
                    input_index,
                    self.config.fee_rate_per_kb,
                    self.config.dust_threshold,
                    self.config.max_inputs,
                )?;
                slot.push(partial);
            }
        }
        for signer in &self.vault_signers[..threshold] {
            log_signature_grant(
                &mut self.ledger,
                "goldcoin_payout",
                &crate::goldcoin::hex::encode(&signer.pubkey),
                request_id,
                now,
            );
        }

        let commitment_hash: [u8; 32] = Sha256::digest(tx.serialize()).into();
        let unsigned_hex = crate::goldcoin::hex::encode(&tx.serialize());

        self.ledger
            .reserve_vault_utxos(request_id, &plan.inputs, now)?;
        self.ledger.record_goldcoin_payout_built(
            request_id,
            &plan,
            commitment_hash,
            &unsigned_hex,
            now,
        )?;

        for (input_index, input_partials) in partials.iter().enumerate() {
            let sighash = tx.sighash_all(input_index, &self.vault.redeem_script());
            tx.inputs[input_index].script_sig =
                multisig::assemble(&self.vault, &sighash, input_partials)?;
        }
        let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
        self.ledger
            .record_goldcoin_payout_signed(request_id, &signed_hex, now)?;

        match self.goldcoin_rpc.send_raw_transaction(&signed_hex).await? {
            BroadcastOutcome::Accepted { .. } | BroadcastOutcome::AlreadyInChain => {
                self.ledger
                    .record_goldcoin_payout_broadcast(request_id, tx.txid(), now)?;
                Ok(())
            }
            BroadcastOutcome::MissingInputs => {
                Err(OrchestratorError::PayoutBroadcastConflict(request_id))
            }
        }
    }

    async fn tick_goldcoin_payout_confirmations(&mut self, now: i64, report: &mut TickReport) {
        let Some((tip_height, _)) = self.ledger.goldcoin_chain_tip().unwrap_or(None) else {
            return; // no indexed Goldcoin tip yet
        };
        let request_ids = match self.ledger.goldcoin_payouts_in_state("Broadcast") {
            Ok(ids) => ids,
            Err(e) => {
                report
                    .errors
                    .push(format!("goldcoin_payouts_in_state(Broadcast): {e}"));
                return;
            }
        };
        for request_id in request_ids {
            let payout = match self.ledger.get_goldcoin_payout(request_id) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    report
                        .errors
                        .push(format!("payout request {request_id}: {e}"));
                    continue;
                }
            };
            let Some(txid) = payout.txid else { continue };
            let txid_hex = crate::goldcoin::hex::encode(&txid);
            let decoded = match self.goldcoin_rpc.get_raw_transaction(&txid_hex).await {
                Ok(d) => d,
                Err(e) => {
                    report
                        .errors
                        .push(format!("payout request {request_id}: {e}"));
                    continue;
                }
            };
            let confirmations = decoded.confirmations.unwrap_or(0);
            match self.ledger.update_goldcoin_payout_confirmations(
                request_id,
                confirmations,
                tip_height,
                self.config.required_goldcoin_confirmations,
                now,
            ) {
                Ok(()) => {
                    if confirmations >= self.config.required_goldcoin_confirmations {
                        report.payouts_confirmed += 1;
                    }
                }
                Err(e) => report
                    .errors
                    .push(format!("payout request {request_id}: {e}")),
            }
        }
    }

    // -------------------------------------------- SolToGlc: completion --

    async fn tick_goldcoin_completions(&mut self, now: i64, report: &mut TickReport) {
        let request_ids = match self.ledger.goldcoin_payouts_in_state("Confirmed") {
            Ok(ids) => ids,
            Err(e) => {
                report
                    .errors
                    .push(format!("goldcoin_payouts_in_state(Confirmed): {e}"));
                return;
            }
        };
        for request_id in request_ids {
            let already_submitted = match self.ledger.get_goldcoin_payout(request_id) {
                Ok(Some(p)) => p.onchain_completion_signature.is_some(),
                Ok(None) => continue,
                Err(e) => {
                    report
                        .errors
                        .push(format!("completion request {request_id}: {e}"));
                    continue;
                }
            };
            if already_submitted {
                continue;
            }
            match self.submit_completion(request_id, now).await {
                Ok(()) => report.completions_submitted += 1,
                Err(e) => report
                    .errors
                    .push(format!("completion request {request_id}: {e}")),
            }
        }
    }

    async fn submit_completion(
        &mut self,
        request_id: i64,
        now: i64,
    ) -> Result<(), OrchestratorError> {
        let mut sigs = Vec::with_capacity(self.config.attestation_threshold);
        let mut message = None;
        for signer in self
            .attestation_signers
            .iter()
            .take(self.config.attestation_threshold)
        {
            let (pubkey, signature, msg) =
                independently_attest_completion(signer, &self.ledger, &self.solana_rpc, request_id)
                    .await?;
            if let Some(prev) = &message {
                if prev != &msg {
                    return Err(OrchestratorError::InconsistentAttestation(request_id));
                }
            } else {
                message = Some(msg);
            }
            log_signature_grant(
                &mut self.ledger,
                "attestation",
                &pubkey.to_string(),
                request_id,
                now,
            );
            sigs.push((pubkey, signature));
        }
        let message = message.ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        self.ledger
            .record_attestation(request_id, "completion", &message, now)?;

        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(LedgerError::RequestNotFound(request_id))?;
        let obligation_index = request
            .source_obligation_index
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let payout = self
            .ledger
            .get_goldcoin_payout(request_id)?
            .ok_or(LedgerError::PayoutNotFound(request_id))?;
        let payout_txid = payout
            .txid
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;
        let payout_height = payout
            .mined_height
            .ok_or(OrchestratorError::IncompleteRequest(request_id))?;

        let key_set = attestation::fetch_attestation_key_set(&self.solana_rpc).await?;

        let proof_ix = ed25519::build_attestation_proof(&sigs, &message);
        let completion_ix = instructions::record_goldcoin_completion(
            &self.submitter.pubkey(),
            obligation_index,
            payout_txid,
            payout_height as u64,
            key_set.epoch,
        );
        let blockhash = self.solana_rpc.get_latest_blockhash().await?;
        let tx = SolanaTransaction::new_signed_with_payer(
            &[proof_ix, completion_ix],
            Some(&self.submitter.pubkey()),
            &[&self.submitter],
            blockhash,
        );
        let signature = self.solana_rpc.send_transaction(&tx).await?;
        self.ledger.record_goldcoin_completion_submitted(
            request_id,
            signature_bytes(&signature),
            now,
        )?;
        Ok(())
    }

    async fn tick_goldcoin_completion_confirmations(&mut self, now: i64, report: &mut TickReport) {
        let request_ids = match self.ledger.goldcoin_payouts_in_state("Confirmed") {
            Ok(ids) => ids,
            Err(e) => {
                report
                    .errors
                    .push(format!("goldcoin_payouts_in_state(Confirmed): {e}"));
                return;
            }
        };
        for request_id in request_ids {
            let payout = match self.ledger.get_goldcoin_payout(request_id) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    report
                        .errors
                        .push(format!("completion request {request_id}: {e}"));
                    continue;
                }
            };
            let Some(sig_bytes) = payout.onchain_completion_signature else {
                continue;
            };
            let signature = Signature::from(sig_bytes);
            match self.solana_rpc.get_signature_status(&signature).await {
                Ok(Some(Ok(()))) => match self
                    .ledger
                    .mark_goldcoin_completion_confirmed(request_id, now)
                {
                    Ok(()) => report.completions_confirmed += 1,
                    Err(e) => report
                        .errors
                        .push(format!("completion request {request_id}: {e}")),
                },
                Ok(Some(Err(reason))) => report.errors.push(format!(
                    "completion request {request_id} REJECTED on chain: {reason}"
                )),
                Ok(None) => {} // still pending; retried next tick
                Err(e) => report
                    .errors
                    .push(format!("completion request {request_id}: {e}")),
            }
        }
    }
}

fn signature_bytes(signature: &Signature) -> [u8; 64] {
    signature.as_ref().try_into().unwrap()
}

/// Best-effort identity-only audit entry (docs/06-schema.md's
/// `signature_grant_log` — never key material). Deliberately never
/// propagates its own failure into the settlement path it's logging: an
/// audit-trail write must not be able to block a release/payout.
fn log_signature_grant(
    ledger: &mut Ledger,
    action_type: &str,
    identity: &str,
    request_id: i64,
    now: i64,
) {
    if let Err(e) =
        ledger.record_signature_grant(action_type, identity, Some(request_id), "info", now)
    {
        tracing::warn!(error = %e, action_type, identity, request_id, "failed to record signature_grant_log entry (non-fatal, audit trail only)");
    }
}

#[cfg(test)]
mod tests;
