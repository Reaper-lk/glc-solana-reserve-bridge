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
//! [`RecoverySplitSource::rederive_split_plan`] independently re-runs the
//! same hard-invariant solvency formula
//! [`crate::reconciliation::reconcile`] already enforces
//! (`observed_balance + own_unconfirmed_change_atomic >=
//! protected_minimum + pending_obligations`) against what THIS split
//! would leave behind — since a split's chunk outputs all pay the
//! vault's own script and are ledger-tracked from the instant of
//! broadcast, that projection is `balance - fee >= floor` (only the
//! network fee genuinely leaves; see the check's own comment for the
//! 2026-08-30 alignment history) — and refuses, with no override, if
//! performing it would breach that floor. Every signer runs this check
//! itself before contributing a signature, not just the orchestrating
//! caller (`glc-admin`, or the daemon's `goldcoin::liquidity` automatic
//! shaping tick): the exact same defense-in-depth this crate already
//! applies to every other fund-moving operation.

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
    /// The unconditional, non-overridable safety refusal: performing this
    /// split would drop total reserve value (mature plus the split's own
    /// ledger-tracked immature chunks — i.e. everything except the
    /// network fee, which alone genuinely leaves) below
    /// `protected_minimum + pending_obligations` — the exact solvency
    /// formula `crate::reconciliation::reconcile` enforces reactively,
    /// checked here proactively before ever broadcasting.
    #[error(
        "refusing unsafe split: reserve value after this split's fee would be \
         {mature_reserve_after}, below the required floor of {required_floor} \
         (protected_minimum + pending_obligations)"
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
    /// [`RecoverySplitSource`] only resumes a split genuinely stuck in
    /// `Built` (recorded, never signed — e.g. a crash between
    /// `record_vault_utxo_split_built` and `record_vault_utxo_split_signed`).
    /// `Signed` has its own resume path (`goldcoin::liquidity::resume_pending_split`, which
    /// re-broadcasts the exact stored bytes without any signer round-trip);
    /// `Broadcast` needs nothing.
    #[error(
        "split for {}:{vout} is in state {state}, not Built — refusing to re-sign",
        crate::goldcoin::hex::encode(txid)
    )]
    SplitNotRecoverable {
        txid: [u8; 32],
        vout: u32,
        state: String,
    },
    /// The plan reconstructed from persisted `vault_utxo_splits` figures
    /// does not serialize to byte-for-byte the same unsigned transaction
    /// that was originally built and persisted — recovery refuses rather
    /// than signing a subtly different transaction (the same
    /// strongest-available check `goldcoin::payout_recovery` applies).
    #[error("split for {}:{vout}: reconstructed unsigned transaction does not match the originally persisted one — refusing to recover",
            crate::goldcoin::hex::encode(txid))]
    ReconstructedTxMismatch { txid: [u8; 32], vout: u32 },
    /// No pending split row exists for the outpoint recovery was asked
    /// about.
    #[error(
        "no vault UTXO split exists for {}:{vout}",
        crate::goldcoin::hex::encode(txid)
    )]
    SplitNotFound { txid: [u8; 32], vout: u32 },
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

/// Re-derivation source for a split stuck in `Built` (recorded but never
/// signed — a crash window the automatic shaping tick must be able to
/// cross on its own; see `goldcoin::liquidity`). Each signer still
/// independently reconstructs the plan from its own ledger view — from
/// the PERSISTED row's figures, exactly like
/// [`crate::goldcoin::payout_recovery`]'s `RecoveryPayoutSource` — and
/// independently proves the reconstruction serializes byte-for-byte to
/// the originally persisted unsigned transaction before signing. The
/// source must still be `Available` (a `Built` split never broadcast, so
/// nothing may have spent it), and the same non-overridable reserve-floor
/// safety check every fresh split gets is re-run
/// against CURRENT state, never assumed from build time.
///
/// The trait's `chunk_target_atomic`/`fee_rate_per_kb` parameters are
/// cross-checked against / superseded by the persisted row: the caller
/// passes the row's own `chunk_target_atomic` (verified equal), and the
/// fee is always the persisted one — never re-derived from a
/// possibly-since-changed live fee rate, which would silently change the
/// transaction being recovered.
pub struct RecoverySplitSource<'a> {
    pub ledger: &'a Ledger,
}

impl IndependentSplitSource for RecoverySplitSource<'_> {
    fn rederive_split_plan(
        &self,
        source_txid: [u8; 32],
        source_vout: u32,
        vault: &MultisigVault,
        chunk_target_atomic: u64,
        _fee_rate_per_kb: u64,
    ) -> Result<SplitPlan, SplitSigningError> {
        let snapshot = self
            .ledger
            .get_vault_utxo_split(source_txid, source_vout)?
            .ok_or(SplitSigningError::SplitNotFound {
                txid: source_txid,
                vout: source_vout,
            })?;
        if snapshot.state != "Built" {
            return Err(SplitSigningError::SplitNotRecoverable {
                txid: source_txid,
                vout: source_vout,
                state: snapshot.state,
            });
        }
        if snapshot.chunk_target_atomic != chunk_target_atomic {
            return Err(SplitSigningError::ReconstructedTxMismatch {
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
        if row.amount_atomic != snapshot.source_amount_atomic {
            return Err(SplitSigningError::ReconstructedTxMismatch {
                txid: source_txid,
                vout: source_vout,
            });
        }
        // Root-vault-only, checked by every signer independently (not
        // just the claiming caller): a per-request derived deposit UTXO
        // must never be restructured — spending it would break the
        // GlcToSol deposit binding its request depends on.
        if !row
            .script_pubkey_hex
            .eq_ignore_ascii_case(&vault.script_pubkey_hex())
        {
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

        let plan = split::reconstruct_plan(
            &source,
            vault,
            snapshot.chunk_count as u64,
            snapshot.fee_atomic,
        )?;
        let reconstructed_hex =
            crate::goldcoin::hex::encode(&split::build_unsigned_split_tx(&plan).serialize());
        if !reconstructed_hex.eq_ignore_ascii_case(&snapshot.unsigned_tx_hex) {
            return Err(SplitSigningError::ReconstructedTxMismatch {
                txid: source_txid,
                vout: source_vout,
            });
        }

        // Same unconditional reserve-floor refusal a fresh split gets
        // (the solvency-invariant-aligned
        // `balance - fee >= floor` form), against CURRENT reserve state.
        let (balance, protected_minimum, _reserved_liquidity, pending_obligations) = self
            .ledger
            .reserve_snapshot(ReserveDirection::GoldcoinReserve)?;
        let reserve_after_fee = balance.saturating_sub(plan.fee_atomic);
        let required_floor = protected_minimum + pending_obligations;
        if reserve_after_fee < required_floor {
            return Err(SplitSigningError::SafetyCheckFailed {
                mature_reserve_after: reserve_after_fee,
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
