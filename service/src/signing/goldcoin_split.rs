//! Independent multi-signer signing for [`crate::goldcoin::split`] —
//! the vault-UTXO-splitting counterpart to
//! [`crate::signing::goldcoin_vault`], reusing the exact same 2-of-3
//! remote signer path (`VaultSigner`, `crate::goldcoin::multisig::assemble`)
//! every real payout uses.
//!
//! # Why this is a separate trait from `IndependentPayoutSource`
//!
//! [`crate::goldcoin::payout::PayoutPlan`] is shaped around a bridge
//! request: a `request_id`, a destination P2PKH address, a net payout
//! amount. A vault split has none of that — no `bridge_requests` row, no
//! external destination, every output pays the vault's own script. Rather
//! than force [`crate::goldcoin::split::SplitPlan`] through a trait built
//! for a different shape, [`IndependentSplitSource`] is the split-specific
//! analog: a signer is handed only a source outpoint and must independently
//! reconstruct the entire plan — amount, chunk count, chunk sizes, AND
//! (unique to a split, since it spends value straight out of mature
//! reserve) a fresh reserve-safety check — from its own ledger view. It is
//! structurally incapable of being handed a plan to blindly sign, exactly
//! like [`crate::signing::goldcoin_vault::IndependentPayoutSource`].
//!
//! # The safety check lives here, not just at the CLI layer
//!
//! [`LedgerSplitSource::rederive_split_plan`] independently re-runs the
//! same hard-invariant formula [`crate::reconciliation::reconcile`] already
//! enforces (`observed_balance >= protected_minimum + pending_obligations`)
//! against what THIS split would leave behind, and refuses — with no
//! override — if performing it would breach that floor. Every signer runs
//! this check itself before contributing a signature, not just the
//! orchestrating `glc-admin` command: the exact same defense-in-depth this
//! crate already applies to every other fund-moving operation.

use std::time::Duration;

use thiserror::Error;

use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::multisig::PartialSignature;
use crate::goldcoin::split::{self, SplitError, SplitPlan, SplitVerifyError};
use crate::goldcoin::tx::Transaction;
use crate::goldcoin::vault::MultisigVault;
use crate::ledger::{Ledger, LedgerError, ReserveDirection};
use crate::signing::signers::{SignerError, VaultSigner};

#[derive(Debug, Error)]
pub enum SplitSigningError {
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("split planning failed: {0}")]
    Planning(#[from] SplitError),
    #[error("re-derived split plan fails its own conservation check (should never happen)")]
    PlanVerificationFailed(#[from] SplitVerifyError),
    #[error("vault signer error: {0}")]
    Signer(#[from] SignerError),
    #[error(
        "no vault UTXO {}:{vout} is known to this ledger",
        crate::goldcoin::hex::encode(txid)
    )]
    SourceNotFound { txid: [u8; 32], vout: u32 },
    #[error(
        "vault UTXO {}:{vout} is not available to split — state is {state}, not Available",
        crate::goldcoin::hex::encode(txid)
    )]
    SourceNotAvailable {
        txid: [u8; 32],
        vout: u32,
        state: String,
    },
    /// Splitting is scoped to the shared root vault only — a per-request
    /// derived deposit-address UTXO is already narrowly scoped to one
    /// funding request and must never be fragmented by this operation.
    #[error(
        "vault UTXO {}:{vout} does not belong to the root vault — splitting is refused for a per-request derived deposit address",
        crate::goldcoin::hex::encode(txid)
    )]
    SourceNotRootVault { txid: [u8; 32], vout: u32 },
    #[error(
        "vault UTXO {}:{vout} has already been split",
        crate::goldcoin::hex::encode(txid)
    )]
    AlreadySplit { txid: [u8; 32], vout: u32 },
    /// The unconditional, non-overridable safety refusal: performing this
    /// split would itself drop mature reserve below `protected_minimum +
    /// pending_obligations` — the exact formula
    /// `crate::reconciliation::reconcile` enforces reactively, checked here
    /// proactively before ever broadcasting.
    #[error(
        "refusing unsafe split: mature reserve after this split would be {mature_reserve_after}, \
         below the required floor of {required_floor} (protected_minimum + pending_obligations)"
    )]
    SafetyCheckFailed {
        mature_reserve_after: u64,
        required_floor: u64,
    },
    /// Two independent signers re-derived different plans from the same
    /// source outpoint — should never happen in a single operator
    /// invocation (nothing else is mutating the ledger concurrently), but
    /// refused rather than assembling a transaction built from
    /// disagreeing signatures.
    #[error("independent signers disagree on the re-derived split plan — refusing to proceed")]
    PlanDisagreement,
}

/// What a signer re-derives a [`SplitPlan`] FROM — its own data source,
/// never a plan handed to it directly. See module docs.
pub trait IndependentSplitSource {
    fn rederive_split_plan(
        &self,
        source_txid: [u8; 32],
        source_vout: u32,
        vault: &MultisigVault,
        chunk_target_atomic: u64,
        fee_rate_per_kb: u64,
    ) -> Result<SplitPlan, SplitSigningError>;
}

/// Re-derivation source backed directly by [`Ledger`] — every signer in
/// the dev/pilot harness shares this same ledger (the same honest
/// simplification [`crate::signing::goldcoin_vault::DevLedgerPayoutSource`]
/// already documents: true production custody domains would each have
/// their own replica of relevant chain state).
pub struct LedgerSplitSource<'a> {
    pub ledger: &'a Ledger,
}

impl IndependentSplitSource for LedgerSplitSource<'_> {
    fn rederive_split_plan(
        &self,
        source_txid: [u8; 32],
        source_vout: u32,
        vault: &MultisigVault,
        chunk_target_atomic: u64,
        fee_rate_per_kb: u64,
    ) -> Result<SplitPlan, SplitSigningError> {
        if self
            .ledger
            .get_vault_utxo_split(source_txid, source_vout)?
            .is_some()
        {
            return Err(SplitSigningError::AlreadySplit {
                txid: source_txid,
                vout: source_vout,
            });
        }

        let row = self
            .ledger
            .get_vault_utxo(source_txid, source_vout)?
            .ok_or(SplitSigningError::SourceNotFound {
                txid: source_txid,
                vout: source_vout,
            })?;
        if row.state != "Available" {
            return Err(SplitSigningError::SourceNotAvailable {
                txid: source_txid,
                vout: source_vout,
                state: row.state,
            });
        }
        let root_script = vault.script_pubkey_hex();
        if !row.script_pubkey_hex.eq_ignore_ascii_case(&root_script) {
            return Err(SplitSigningError::SourceNotRootVault {
                txid: source_txid,
                vout: source_vout,
            });
        }

        let source = VaultUtxo {
            txid: source_txid,
            vout: source_vout,
            amount_atomic: row.amount_atomic,
            script_pubkey_hex: row.script_pubkey_hex,
        };
        let plan = split::plan_split(&source, vault, chunk_target_atomic, fee_rate_per_kb)?;

        let (balance, protected_minimum, _reserved_liquidity, pending_obligations) = self
            .ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)?;
        let mature_reserve_after = balance.saturating_sub(source.amount_atomic);
        let required_floor = protected_minimum + pending_obligations;
        if mature_reserve_after < required_floor {
            return Err(SplitSigningError::SafetyCheckFailed {
                mature_reserve_after,
                required_floor,
            });
        }

        Ok(plan)
    }
}

/// Independently re-derives the split plan (including the reserve-safety
/// check), builds the unsigned transaction, verifies it against itself,
/// and signs its single input. Mirrors
/// [`crate::signing::goldcoin_vault::independently_sign`] — the split's
/// single input always signs with the vault's own root key
/// (`VaultSigner::sign_sighash`), never a derived one: splitting is scoped
/// to root-vault UTXOs only.
#[allow(clippy::too_many_arguments)]
pub async fn independently_sign_split(
    signer: &dyn VaultSigner,
    vault: &MultisigVault,
    source: &dyn IndependentSplitSource,
    source_txid: [u8; 32],
    source_vout: u32,
    chunk_target_atomic: u64,
    fee_rate_per_kb: u64,
    signer_timeout: Duration,
) -> Result<(PartialSignature, SplitPlan, Transaction), SplitSigningError> {
    let plan = source.rederive_split_plan(
        source_txid,
        source_vout,
        vault,
        chunk_target_atomic,
        fee_rate_per_kb,
    )?;
    let unsigned_tx = split::build_unsigned_split_tx(&plan);
    split::verify_split_tx(&unsigned_tx, &plan)?;
    let sighash = unsigned_tx.sighash_all(0, &vault.redeem_script());
    let identity = crate::goldcoin::hex::encode(&signer.public_key());
    let der = match tokio::time::timeout(signer_timeout, signer.sign_sighash(&sighash)).await {
        Ok(Ok(der)) => der,
        Ok(Err(e)) => return Err(SplitSigningError::Signer(e)),
        Err(_) => {
            return Err(SplitSigningError::Signer(SignerError::Timeout {
                identity,
                millis: signer_timeout.as_millis() as u64,
            }))
        }
    };
    Ok((
        PartialSignature {
            vault_pubkey: signer.public_key(),
            der_signature: der,
        },
        plan,
        unsigned_tx,
    ))
}

/// Drives [`independently_sign_split`] across the first `threshold`
/// configured vault signers, each independently re-deriving and verifying
/// the plan (refusing if any two disagree), and collects `threshold`
/// partials for the split transaction's one input. Mirrors
/// [`crate::signing::goldcoin_vault::independently_sign_all_inputs`]'s
/// shape, simplified for a split's fixed single input.
#[allow(clippy::too_many_arguments)]
pub async fn independently_sign_split_all_signers(
    vault_signers: &[Box<dyn VaultSigner>],
    vault: &MultisigVault,
    source: &dyn IndependentSplitSource,
    source_txid: [u8; 32],
    source_vout: u32,
    chunk_target_atomic: u64,
    fee_rate_per_kb: u64,
    threshold: usize,
    signer_timeout: Duration,
) -> Result<(SplitPlan, Transaction, Vec<PartialSignature>), SplitSigningError> {
    let mut partials = Vec::with_capacity(threshold);
    let mut agreed: Option<(SplitPlan, Transaction)> = None;
    for signer in &vault_signers[..threshold] {
        let (partial, plan, tx) = independently_sign_split(
            signer.as_ref(),
            vault,
            source,
            source_txid,
            source_vout,
            chunk_target_atomic,
            fee_rate_per_kb,
            signer_timeout,
        )
        .await?;
        match &agreed {
            None => agreed = Some((plan, tx)),
            Some((first_plan, _)) if *first_plan != plan => {
                return Err(SplitSigningError::PlanDisagreement)
            }
            Some(_) => {}
        }
        partials.push(partial);
    }
    let (plan, tx) = agreed.expect("threshold >= 1, so the loop ran at least once");
    Ok((plan, tx, partials))
}

#[cfg(test)]
mod tests;
