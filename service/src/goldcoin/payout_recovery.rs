//! Operator-triggered recovery for a Goldcoin payout stuck in the
//! `Signed` state after its broadcast was rejected — e.g. by Goldcoin
//! Core's non-canonical-signature policy check (`-26: 64:
//! non-mandatory-script-verify-flag`, the bug `goldcoin::multisig::
//! sign_low_s` fixes going forward). A request in this state has already
//! transitioned `bridge_requests.state` to `SettlementAuthorized` and has
//! a `goldcoin_payouts` row in `Signed` state — `Orchestrator::
//! tick_goldcoin_payouts` skips any request with an existing payout row
//! forever (`needs operator attention if stuck`, by design), so nothing
//! in the normal tick loop will ever revisit it.
//!
//! # Never trusts the persisted `signed_tx_hex`, never re-selects inputs
//!
//! [`recover_stuck_goldcoin_payout`] never rebroadcasts the stored
//! `signed_tx_hex` verbatim — whatever made the original broadcast fail
//! may be baked into those exact bytes. Instead it independently
//! reconstructs the SAME plan from what the original build already
//! committed (the persisted `goldcoin_payouts`/`goldcoin_payout_inputs`
//! rows, via [`RecoveryPayoutSource`]) — never a fresh coin selection,
//! which the already-`Reserved` inputs are structurally invisible to
//! anyway (`Ledger::available_vault_utxos` only returns `state =
//! 'Available'` rows) — proves that reconstruction serializes to
//! byte-for-byte the same unsigned transaction that was originally built,
//! re-runs the real independent multi-signer signing path
//! (`signing::goldcoin_vault::independently_sign_all_inputs`, identical to
//! the one a normal build uses), and only then attempts to broadcast.
//!
//! # Explicit operator action only
//!
//! This module is never called from [`crate::orchestrator::Orchestrator::
//! tick`] or any other automatic loop — only from an explicit operator
//! command (`glc-admin retry-goldcoin-payout`). The normal daemon's
//! skip-if-a-payout-row-exists behavior is unchanged.

use std::time::Duration;

use thiserror::Error;

use super::address::Network;
use super::indexer::GoldcoinRpc;
use super::multisig;
use super::payout::{self, PayoutInputContext, PayoutPlan, PayoutPolicy};
use super::rpc::BroadcastOutcome;
use super::vault::MultisigVault;
use crate::amount_conversion;
use crate::ledger::{Direction, Ledger, LedgerError, RequestState};
use crate::signing::goldcoin_vault::{
    independently_sign_all_inputs, IndependentPayoutSource, SigningError,
};
use crate::signing::signers::VaultSigner;

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("signing error: {0}")]
    Signing(#[from] SigningError),
    #[error("multisig assembly error: {0}")]
    Multisig(#[from] multisig::MultisigError),
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("goldcoin rpc error: {0}")]
    Rpc(#[from] super::rpc::RpcError),
    #[error("Goldcoin RPC reports missing inputs while recovering request {0} — the reserved UTXO(s) are no longer spendable on-chain; needs operator investigation, never silently retried")]
    BroadcastConflict(i64),
}

/// Outcome of a recovery attempt that did not error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// Broadcast succeeded (or the node reported it already known/mined)
    /// this call. `resigned_hex_changed` is `true` when the freshly
    /// re-signed transaction differs from what was previously persisted —
    /// `false` means re-signing reproduced byte-identical bytes to what
    /// was already stored (deterministic signing: same plan, same keys,
    /// same signature every time), which is a strong signal the original
    /// broadcast rejection has a cause other than signature
    /// canonicalization.
    Broadcast {
        txid: [u8; 32],
        resigned_hex_changed: bool,
    },
    /// The payout was already past `Signed` (`Broadcast`/`Confirmed`/
    /// `Completed`) before this call did anything — a safe, non-mutating
    /// no-op, safe to call again.
    AlreadyDone { state: String },
}

/// Reconstructs a [`PayoutPlan`] for a request's ALREADY-BUILT payout from
/// the exact facts `Ledger::record_goldcoin_payout_built` persisted —
/// never coin selection, never a value trusted without independently
/// re-checking it against current `bridge_requests`/`vault_utxos` state.
pub struct RecoveryPayoutSource<'a> {
    pub ledger: &'a Ledger,
}

impl IndependentPayoutSource for RecoveryPayoutSource<'_> {
    fn rederive_plan(
        &self,
        request_id: i64,
        vault: &MultisigVault,
        policy: &PayoutPolicy,
        network: Network,
    ) -> Result<PayoutPlan, SigningError> {
        let request = self
            .ledger
            .get_request(request_id)?
            .ok_or(SigningError::RequestNotFound(request_id))?;
        if request.direction != Direction::SolToGlc {
            return Err(SigningError::WrongDirection(request_id));
        }
        // Recovery's precondition is the OPPOSITE state a normal build
        // requires: a request only reaches SettlementAuthorized once
        // `record_goldcoin_payout_signed` has already run, which is
        // exactly the case a stuck-in-Signed payout is in.
        if request.state != RequestState::SettlementAuthorized {
            return Err(SigningError::NotSettlementAuthorized(
                request_id,
                request.state,
            ));
        }

        let dest_addr = String::from_utf8_lossy(&request.recipient)
            .trim_end_matches('\0')
            .to_string();
        let dest_p2pkh_hash = crate::goldcoin::address::decode_p2pkh(&dest_addr, network)?;

        let fee_breakdown = amount_conversion::verify_fee_breakdown(
            request.gross_amount_atomic,
            request.fee_amount_atomic,
            request.net_amount_atomic,
        )
        .map_err(|e| SigningError::Conversion(request_id, e))?;
        let payout_atomic = fee_breakdown.net.0;

        let payout_row = self
            .ledger
            .get_goldcoin_payout_full(request_id)?
            .ok_or_else(|| {
                SigningError::PayoutNotRecoverable(request_id, "<no payout row>".to_string())
            })?;
        if payout_row.state != "Signed" {
            return Err(SigningError::PayoutNotRecoverable(
                request_id,
                payout_row.state,
            ));
        }

        // Every field this recovery will actually use for signing must
        // independently agree with what's freshly derivable from
        // `bridge_requests` — never trusted from the persisted row alone.
        if payout_row.payout_atomic != payout_atomic {
            return Err(SigningError::PayoutFieldMismatch {
                request_id,
                field: "payout_atomic",
            });
        }
        if payout_row.dest_p2pkh_hash != dest_p2pkh_hash {
            return Err(SigningError::PayoutFieldMismatch {
                request_id,
                field: "dest_p2pkh_hash",
            });
        }

        // The exact inputs already reserved for this payout — never a
        // fresh selection (`policy` is accepted only to match
        // `IndependentPayoutSource`'s shared signature; recovery never uses
        // it for sizing).
        let _ = policy;
        let inputs = self.ledger.get_goldcoin_payout_inputs(request_id)?;

        let root_script = vault.script_pubkey_hex();
        let mut input_contexts = Vec::with_capacity(inputs.len());
        for utxo in &inputs {
            if utxo.script_pubkey_hex.eq_ignore_ascii_case(&root_script) {
                input_contexts.push(PayoutInputContext {
                    vault: vault.clone(),
                    funding_request_id: None,
                });
            } else {
                let funding_request_id = self
                    .ledger
                    .find_glc_to_sol_request_by_deposit_script(&utxo.script_pubkey_hex)?
                    .ok_or_else(|| {
                        SigningError::UnknownVaultUtxoScript(utxo.script_pubkey_hex.clone())
                    })?;
                let derived_vault = crate::goldcoin::derivation::derive_request_vault(
                    vault,
                    funding_request_id,
                    network,
                )?;
                input_contexts.push(PayoutInputContext {
                    vault: derived_vault,
                    funding_request_id: Some(funding_request_id),
                });
            }
        }

        let plan = PayoutPlan {
            inputs,
            input_contexts,
            dest_p2pkh_hash,
            payout_atomic,
            change_outputs: payout_row.change_outputs,
            vault_script_pubkey: vault.script_pubkey(),
            fee_atomic: payout_row.fee_atomic,
        };

        // The single strongest check: reconstructing this plan must
        // reproduce, byte for byte, the exact unsigned transaction
        // originally built and persisted. Any drift at all — a different
        // input, a different amount, a different destination — changes
        // this serialization; refusing here is the anti-tamper backstop
        // requirement 9 asks for, independent of the field-by-field checks
        // above.
        let rebuilt = payout::build_unsigned_tx(&plan);
        let rebuilt_hex = crate::goldcoin::hex::encode(&rebuilt.serialize());
        if Some(rebuilt_hex) != payout_row.unsigned_tx_hex {
            return Err(SigningError::ReconstructedTxMismatch(request_id));
        }
        payout::verify_payout_tx(&rebuilt, &plan)?;

        Ok(plan)
    }
}

/// Recovers request `request_id`'s Goldcoin payout if, and only if, it is
/// genuinely stuck in `Signed` state — see module docs. Idempotent: safe
/// to call repeatedly, including after it has already succeeded (returns
/// [`RecoveryOutcome::AlreadyDone`] without mutating anything once the
/// payout has reached `Broadcast` or later).
#[allow(clippy::too_many_arguments)]
pub async fn recover_stuck_goldcoin_payout<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    goldcoin_rpc: &GR,
    request_id: i64,
    threshold: usize,
    policy: &PayoutPolicy,
    network: Network,
    signer_timeout: Duration,
    now: i64,
) -> Result<RecoveryOutcome, RecoveryError> {
    let payout = ledger
        .get_goldcoin_payout_full(request_id)?
        .ok_or_else(|| SigningError::PayoutNotRecoverable(request_id, "<no payout row>".into()))?;
    match payout.state.as_str() {
        "Broadcast" | "Confirmed" | "Completed" => {
            return Ok(RecoveryOutcome::AlreadyDone {
                state: payout.state,
            });
        }
        "Signed" => {}
        other => {
            return Err(SigningError::PayoutNotRecoverable(request_id, other.to_string()).into())
        }
    }
    let previous_signed_hex = payout.signed_tx_hex;

    let source = RecoveryPayoutSource { ledger };
    let (plan, mut tx, partials) = independently_sign_all_inputs(
        vault_signers,
        vault,
        &source,
        request_id,
        threshold,
        policy,
        network,
        signer_timeout,
    )
    .await?;

    for (input_index, input_partials) in partials.iter().enumerate() {
        let input_vault = &plan.input_contexts[input_index].vault;
        let sighash = tx.sighash_all(input_index, &input_vault.redeem_script());
        tx.inputs[input_index].script_sig =
            multisig::assemble(input_vault, &sighash, input_partials)?;
    }
    let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
    let resigned_hex_changed = previous_signed_hex.as_deref() != Some(signed_hex.as_str());

    // Records the freshly re-signed bytes without changing `state` — the
    // row stays `Signed` unless/until the broadcast below actually
    // succeeds (requirement: never mark Broadcast before RPC acceptance).
    ledger.record_goldcoin_payout_resigned(request_id, &signed_hex, now)?;

    match goldcoin_rpc.send_raw_transaction(&signed_hex).await? {
        BroadcastOutcome::Accepted { .. } | BroadcastOutcome::AlreadyInChain => {
            let txid = tx.txid();
            ledger.record_goldcoin_payout_broadcast(request_id, txid, now)?;
            Ok(RecoveryOutcome::Broadcast {
                txid,
                resigned_hex_changed,
            })
        }
        BroadcastOutcome::MissingInputs => Err(RecoveryError::BroadcastConflict(request_id)),
    }
}

#[cfg(test)]
mod tests;
