//! The daemon-side executor behind `POST /refunds/glc/{id}/execute` — the
//! one operation on the admin API that can move funds.
//!
//! # Why this lives in the daemon and not in `glc-admin`
//!
//! The running `glc-bridge-daemon` already owns the production
//! `vault_remote_signers` and the environment holding their auth tokens.
//! That trust boundary is deliberate and is preserved exactly: no vault
//! key material, and no signer token, ever reaches `glc-admin`, a CLI
//! argument, or this API's responses. `glc-admin --execute` is a *request*
//! to the daemon; the daemon does the work.
//!
//! # Nothing the caller says is trusted
//!
//! The request carries a request id and an audit note. That is all there
//! is to carry: there is no destination, amount, fee, transaction, signer
//! or override parameter anywhere in the request type, so no caller —
//! authenticated or not — can influence where the money goes. Every value
//! is derived here, server-side, from Goldcoin and Solana chain state
//! cross-checked against the ledger.
//!
//! In particular the validation `glc-admin` performed for its dry run is
//! re-run in full immediately before signing. A dry run that passed
//! minutes ago proves nothing about now: the request may have moved on, a
//! Solana release may have appeared, the chain may disagree, or the pause
//! may have been lifted.
//!
//! # Signing
//!
//! Uses the daemon's existing 2-of-3 vault signers, the same
//! `multisig::assemble` the payout path uses, and the same verification
//! discipline. No hot wallet, no new key path, no reduced threshold.
//!
//! # LIMITATION (unchanged by this module)
//!
//! Signers still receive only a 32-byte sighash, so they cannot verify
//! what they sign — exactly as for every payout today. The verification
//! here is the ORCHESTRATOR's, not the signers'. See
//! `crate::goldcoin::refund`'s module docs and docs/09-runbook.md.

use std::sync::Arc;
use std::time::Duration;

use crate::admin_api::{
    AdminError, BoxFut, GlcRefundAction, GlcRefundCheckView, GlcRefundExecuteView,
};
use crate::goldcoin::address::Network;
use crate::goldcoin::multisig::{self, PartialSignature};
use crate::goldcoin::payout::{PayoutPlan, PayoutPolicy};
use crate::goldcoin::refund::{self, RefundExecuteOutcome};
use crate::goldcoin::rpc::{BroadcastOutcome, DecodedTransaction, RpcError};
use crate::goldcoin::tx::Transaction;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{GoldcoinRefundState, Ledger};
use crate::signing::signers::VaultSigner;
use crate::solana::rpc::SolanaRpc;

/// What the admin API calls to perform a refund. Behind a trait so
/// `AdminApi` keeps its existing generics and so tests can substitute a
/// stub without a real signer or a real node.
pub trait GlcRefundExecutor: Send + Sync + 'static {
    fn execute(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<GlcRefundExecuteView, AdminError>>;
}

/// The production executor: the daemon's Goldcoin RPC, vault, policy and
/// signer clients.
///
/// # Why the work runs on its own thread
///
/// `Ledger` is `!Sync`, so any future holding a `&Ledger` across an
/// `.await` is not `Send` — and the admin listener is `tokio::spawn`ed,
/// which requires `Send`. The refund lifecycle inherently interleaves
/// ledger writes with chain reads, so it cannot simply avoid the borrow.
///
/// Rather than weaken the ledger's threading model for one endpoint, each
/// execution runs on a dedicated thread with its own current-thread
/// runtime, and the caller awaits a oneshot. The `Ledger` is opened and
/// dropped entirely inside that thread, so it never crosses a thread
/// boundary at all. One refund at a time, on its own thread, is exactly
/// the concurrency this operation wants.
pub struct DaemonGlcRefundExecutor<GR, SR> {
    inner: Arc<ExecutorInner<GR, SR>>,
}

struct ExecutorInner<GR, SR> {
    db_path: std::path::PathBuf,
    goldcoin_rpc: GR,
    solana_rpc: SR,
    vault: MultisigVault,
    policy: PayoutPolicy,
    network: Network,
    /// `goldcoin.vault_min_confirmations` — the depth a deposit must have
    /// before it may be returned.
    required_confirmations: i64,
    vault_signers: Arc<Vec<Box<dyn VaultSigner>>>,
    vault_threshold: usize,
    signer_timeout: Duration,
}

impl<GR, SR> DaemonGlcRefundExecutor<GR, SR> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db_path: std::path::PathBuf,
        goldcoin_rpc: GR,
        solana_rpc: SR,
        vault: MultisigVault,
        policy: PayoutPolicy,
        network: Network,
        required_confirmations: i64,
        vault_signers: Arc<Vec<Box<dyn VaultSigner>>>,
        vault_threshold: usize,
        signer_timeout: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(ExecutorInner {
                db_path,
                goldcoin_rpc,
                solana_rpc,
                vault,
                policy,
                network,
                required_confirmations,
                vault_signers,
                vault_threshold,
                signer_timeout,
            }),
        }
    }
}

/// Adapts a Goldcoin RPC client to the narrow surface the refund trace
/// needs.
pub trait GoldcoinRefundRpc: Send + Sync + 'static {
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> BoxFut<'_, Result<DecodedTransaction, RpcError>>;
    fn send_raw_transaction(&self, hex: &str) -> BoxFut<'_, Result<BroadcastOutcome, RpcError>>;
}

struct RpcBridge<'a, GR: GoldcoinRefundRpc>(&'a GR);

impl<GR: GoldcoinRefundRpc> refund::RefundRpc for RpcBridge<'_, GR> {
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        self.0.get_raw_transaction(txid_hex).await
    }
    async fn send_raw_transaction(&self, hex: &str) -> Result<BroadcastOutcome, RpcError> {
        self.0.send_raw_transaction(hex).await
    }
}

struct SolanaBridge<'a, SR: SolanaRpc>(&'a SR);

impl<SR: SolanaRpc + Sync> refund::ReleaseWitnessRpc for SolanaBridge<'_, SR> {
    async fn account_exists(&self, pubkey: &solana_sdk::pubkey::Pubkey) -> Result<bool, String> {
        self.0
            .get_account(pubkey)
            .await
            .map(|a| a.is_some())
            .map_err(|e| e.to_string())
    }
}

/// Collects `threshold` partial signatures per input from the daemon's
/// vault signers and assembles the multisig scriptSigs.
///
/// Each signer is asked for exactly the sighash of the transaction being
/// broadcast — the same contract the payout path uses. A signer that
/// fails, times out, or returns a signature that does not verify aborts
/// the whole refund: there is no "carry on with fewer signatures" path,
/// so the threshold cannot be silently reduced.
/// Note there is no root-vault parameter: each input is signed under
/// `plan.input_contexts[i].vault`, which is the root vault for a legacy
/// input and the request-specific derived vault for a per-request deposit
/// address. Taking the root here would have been the wrong key for a
/// derived input.
async fn sign_with_vault(
    signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    plan: &PayoutPlan,
    mut tx: Transaction,
    signer_timeout: Duration,
) -> Result<Transaction, String> {
    if threshold == 0 || threshold > signers.len() {
        return Err(format!(
            "vault threshold {threshold} is not satisfiable with {} configured signers",
            signers.len()
        ));
    }
    for input_index in 0..plan.inputs.len() {
        let redeem = plan.input_contexts[input_index].vault.redeem_script();
        let sighash = tx.sighash_all(input_index, &redeem);
        let mut partials: Vec<PartialSignature> = Vec::with_capacity(threshold);
        for signer in signers.iter().take(threshold) {
            let der =
                match tokio::time::timeout(signer_timeout, signer.sign_sighash(&sighash)).await {
                    Ok(Ok(der)) => der,
                    Ok(Err(e)) => {
                        return Err(format!(
                            "vault signer {} refused: {e}",
                            crate::goldcoin::hex::encode(&signer.public_key())
                        ))
                    }
                    Err(_) => {
                        return Err(format!(
                            "vault signer {} timed out after {}ms",
                            crate::goldcoin::hex::encode(&signer.public_key()),
                            signer_timeout.as_millis()
                        ))
                    }
                };
            let pubkey = signer.public_key();
            // Every partial is verified locally before it is placed: a
            // signer returning a wrong or malformed signature fails the
            // refund rather than producing an unspendable transaction.
            if !multisig::verify_partial(&pubkey, &sighash, &der) {
                return Err(format!(
                    "vault signer {} returned a signature that does not verify",
                    crate::goldcoin::hex::encode(&pubkey)
                ));
            }
            partials.push(PartialSignature {
                vault_pubkey: pubkey,
                der_signature: der,
            });
        }
        if partials.len() < threshold {
            return Err(format!(
                "only {} of {threshold} required vault signatures were collected",
                partials.len()
            ));
        }
        tx.inputs[input_index].script_sig =
            multisig::assemble(&plan.input_contexts[input_index].vault, &sighash, &partials)
                .map_err(|e| format!("could not assemble the multisig scriptSig: {e}"))?;
    }
    Ok(tx)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl<GR, SR> ExecutorInner<GR, SR>
where
    GR: GoldcoinRefundRpc,
    SR: SolanaRpc + Send + Sync + 'static,
{
    fn open_ledger(&self) -> Result<Ledger, AdminError> {
        Ledger::open(&self.db_path).map_err(|e| AdminError::Upstream(e.to_string()))
    }

    async fn run(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> Result<GlcRefundExecuteView, AdminError> {
        let goldcoin = RpcBridge(&self.goldcoin_rpc);
        let solana = SolanaBridge(&self.solana_rpc);

        // The full server-side re-verification, reported per check. This
        // is the SAME dry run the CLI runs, re-run here against fresh
        // state — never a summary of what the CLI claimed.
        let mut witness_mode = "unknown".to_string();
        let mut witness_is_legacy = false;
        let expected_script;
        let checks = {
            let ledger = self.open_ledger()?;
            if ledger
                .get_request(request_id)
                .map_err(|e| AdminError::Upstream(e.to_string()))?
                .is_none()
            {
                return Err(AdminError::NotFound(format!(
                    "bridge request {request_id} not found"
                )));
            }
            let report = refund::dry_run_refund(
                &goldcoin,
                &solana,
                &ledger,
                request_id,
                &self.vault,
                &self.policy,
                self.network,
                self.required_confirmations,
            )
            .await
            .map_err(|e| AdminError::Upstream(e.to_string()))?;
            if let Some(mode) = report.amount_witness_mode {
                witness_mode = mode.describe().to_string();
                witness_is_legacy = mode.is_legacy();
            }
            expected_script = report.expected_deposit_script_hex.clone();
            report
                .checks
                .iter()
                .map(|c| GlcRefundCheckView {
                    name: c.name.to_string(),
                    passed: c.passed,
                    detail: c.detail.clone(),
                })
                .collect::<Vec<_>>()
        };

        let signers = Arc::clone(&self.vault_signers);
        let threshold = self.vault_threshold;
        let vault = self.vault.clone();
        let timeout = self.signer_timeout;

        let mut ledger = self.open_ledger()?;
        let outcome = refund::execute_refund(
            &goldcoin,
            &solana,
            &mut ledger,
            request_id,
            &note,
            &actor,
            &self.vault,
            &self.policy,
            self.network,
            self.required_confirmations,
            now_unix(),
            move |plan, tx| async move {
                let _ = &vault; // kept alive for the closure's lifetime
                sign_with_vault(&signers, threshold, &plan, tx, timeout).await
            },
        )
        .await
        .map_err(|e| AdminError::Upstream(e.to_string()))?;

        let row = ledger
            .get_goldcoin_refund(request_id)
            .map_err(|e| AdminError::Upstream(e.to_string()))?
            .ok_or_else(|| {
                AdminError::Upstream(format!(
                    "refund row for request {request_id} vanished after execution"
                ))
            })?;
        let request_state = ledger
            .get_request(request_id)
            .map_err(|e| AdminError::Upstream(e.to_string()))?
            .map(|r| format!("{:?}", r.state))
            .unwrap_or_else(|| "unknown".to_string());

        // Audited under the authenticated operator identity: what they
        // asked for, what note they gave, and what actually happened —
        // including the txid. Never the token, never signer material.
        let audit_detail = format!(
            "glc refund request {request_id}: {} -> lifecycle {} request {request_state} txid {}",
            match outcome {
                RefundExecuteOutcome::Broadcast { .. } => "broadcast",
                RefundExecuteOutcome::Rebroadcast { .. } => "rebroadcast (same bytes)",
                RefundExecuteOutcome::AlreadyBroadcast { .. } => "already broadcast",
                RefundExecuteOutcome::AlreadyRefunded => "already refunded",
            },
            row.state.as_str(),
            row.txid
                .map(|t| crate::goldcoin::hex::encode(&t))
                .unwrap_or_else(|| "-".to_string())
        );
        let _ = ledger.append_admin_audit(&crate::ledger::AdminAuditEntry {
            at: now_unix(),
            actor: actor.clone(),
            action: "execute_glc_refund".to_string(),
            target: Some(format!("request {request_id}")),
            old_value: None,
            new_value: Some(audit_detail),
            note: note.clone(),
            outcome: crate::ledger::AdminAuditOutcome::Success,
        });

        Ok(GlcRefundExecuteView {
            request_id,
            action: match outcome {
                RefundExecuteOutcome::Broadcast { .. } => GlcRefundAction::Broadcast,
                RefundExecuteOutcome::Rebroadcast { .. } => GlcRefundAction::Rebroadcast,
                RefundExecuteOutcome::AlreadyBroadcast { .. } => GlcRefundAction::AlreadyBroadcast,
                RefundExecuteOutcome::AlreadyRefunded => GlcRefundAction::AlreadyRefunded,
            },
            lifecycle_state: row.state.as_str().to_string(),
            request_state,
            source_txid: crate::goldcoin::hex::encode(&row.source_txid),
            source_vout: row.source_vout,
            observed_amount_atomic: row.observed_amount_atomic,
            observed_amount_glc: refund::format_glc(row.observed_amount_atomic),
            refund_destination: row.refund_dest_address.clone(),
            refund_principal_atomic: row.refund_amount_atomic,
            refund_principal_glc: refund::format_glc(row.refund_amount_atomic),
            fee_atomic: row.fee_atomic,
            fee_glc: refund::format_glc(row.fee_atomic),
            txid: row.txid.map(|t| crate::goldcoin::hex::encode(&t)),
            confirmations: row.confirmations,
            amount_witness_mode: witness_mode,
            amount_witness_is_legacy: witness_is_legacy,
            expected_deposit_script_hex: expected_script,
            checks,
            note,
            actor,
        })
    }
}

impl<GR, SR> GlcRefundExecutor for DaemonGlcRefundExecutor<GR, SR>
where
    GR: GoldcoinRefundRpc,
    SR: SolanaRpc + Send + Sync + 'static,
{
    fn execute(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<GlcRefundExecuteView, AdminError>> {
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            std::thread::Builder::new()
                .name(format!("glc-refund-exec-{request_id}"))
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => {
                            let _ = tx.send(Err(AdminError::Upstream(format!(
                                "could not start the refund execution runtime: {e}"
                            ))));
                            return;
                        }
                    };
                    let result = rt.block_on(inner.run(request_id, note, actor));
                    let _ = tx.send(result);
                })
                .map_err(|e| {
                    AdminError::Upstream(format!("could not start the refund executor: {e}"))
                })?;
            rx.await.unwrap_or_else(|_| {
                Err(AdminError::Upstream(
                    "the refund executor thread ended without reporting a result; the refund's \
                     durable state in goldcoin_refunds is authoritative — re-run to resume it"
                        .to_string(),
                ))
            })
        })
    }
}

/// Convenience for the daemon: the lifecycle states an operator can still
/// act on, used only for logging clarity.
pub fn is_open_state(state: GoldcoinRefundState) -> bool {
    !matches!(state, GoldcoinRefundState::Refunded)
}

/// Adapts the concrete `GoldcoinRpcClient` to [`GoldcoinRefundRpc`].
/// Lives here rather than in the daemon binary so the trait and its one
/// production implementation stay together.
pub struct RealGoldcoinRefundRpc(pub Arc<crate::goldcoin::rpc::RpcClient>);

impl GoldcoinRefundRpc for RealGoldcoinRefundRpc {
    fn get_raw_transaction(
        &self,
        txid_hex: &str,
    ) -> BoxFut<'_, Result<DecodedTransaction, RpcError>> {
        let client = Arc::clone(&self.0);
        let txid = txid_hex.to_string();
        Box::pin(async move { client.get_raw_transaction(&txid).await })
    }
    fn send_raw_transaction(&self, hex: &str) -> BoxFut<'_, Result<BroadcastOutcome, RpcError>> {
        let client = Arc::clone(&self.0);
        let hex = hex.to_string();
        Box::pin(async move { client.send_raw_transaction(&hex).await })
    }
}

#[cfg(test)]
pub(crate) mod tests;
