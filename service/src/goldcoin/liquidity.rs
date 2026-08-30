//! Automatic vault-UTXO liquidity shaping: the daemon-driven counterpart
//! to the operator CLI `glc-admin split-vault-utxo`, closing the
//! 2026-08-30 production incident (docs/09-runbook.md's "Automatic UTXO
//! liquidity shaping" section): SolToGlc payouts repeatedly failing with
//! `TooManyInputs` because the mature pool had degraded into fragments
//! while large liquidity sat either as one oversized deposit UTXO or as
//! still-maturing payout change — with the only remedy being manual,
//! operator-run UTXO splits.
//!
//! # What one shaping tick does
//!
//! [`run_shaping_tick`] performs AT MOST ONE split-shaped action per tick,
//! in strict priority order:
//!
//! 1. **Resume any split already in flight** (`vault_utxo_splits` rows in
//!    `Built` or `Signed`, oldest first) — the restart-safety half. A
//!    `Signed` row re-broadcasts its exact stored bytes
//!    ([`crate::goldcoin::split_recovery`], no signer round-trip); a
//!    `Built` row re-signs its exact reconstructed plan
//!    ([`crate::signing::goldcoin_split::RecoverySplitSource`], which
//!    proves byte-identity with the persisted unsigned transaction before
//!    any signature is produced). Either way the tick stops there — a
//!    resumed split IS this tick's action.
//! 2. **Consider one new split**, only if ALL of:
//!    - the payout-ready mature pool (Available UTXOs of at least half
//!      the canonical chunk target) is BELOW
//!      [`ShapingPolicy::target_available_count`] — a healthy pool never
//!      produces a self-transaction;
//!    - no previous split's chunk outputs are still maturing — shaping
//!      never stacks a second self-transaction on top of one whose
//!      liquidity is already en route;
//!    - a mature, unreserved, never-before-split ROOT-vault UTXO of at
//!      least [`ShapingPolicy::min_source_atomic`] exists (largest first,
//!      deterministic tie-break by the pool's canonical sort order).
//!
//! The new split goes through the IDENTICAL independent 2-of-3 signing
//! path the operator CLI uses ([`crate::signing::goldcoin_split`]),
//! including every signer's own non-overridable reserve-floor refusal
//! (`mature_reserve_after >= protected_minimum + pending_obligations`) —
//! an automatic trigger never gets a weaker safety posture than a human
//! one. A refusal (e.g. the oversized UTXO IS most of the reserve, the
//! exact bootstrap shape a fresh 1,000,000 GLC deposit produces) is a
//! quiet skip, retried on a later tick once payouts' own fanned-out
//! change has built enough independent mature cover; payouts themselves
//! are never blocked by shaping — coin selection can always spend the
//! oversized UTXO directly in the meantime.
//!
//! # Concurrency and restart safety
//!
//! Every fact lives in the ledger (`vault_utxo_splits` + `vault_utxos`),
//! never in memory across ticks. The moment a split broadcasts,
//! [`crate::ledger::Ledger::record_vault_utxo_split_broadcast_effects`]
//! atomically marks the source `Spent` (so a payout's coin selection can
//! never pick it again — the outpoint is already spent on-chain by the
//! mempooled split) and inserts the chunk outputs as `Unconfirmed` rows
//! (so reconciliation's `own_unconfirmed_change_atomic` term explains the
//! mature-balance dip with no gap). The `UNIQUE(source_txid, source_vout)`
//! index remains the structural once-per-outpoint guarantee, exactly as
//! for operator splits.

use std::time::Duration;

use thiserror::Error;

use super::indexer::GoldcoinRpc;
use super::rpc::{call_with_retry, BroadcastOutcome, RpcError};
use super::split::{self, SplitError};
use super::split_recovery::{
    recover_stuck_vault_utxo_split, SplitRecoveryError, SplitRecoveryOutcome,
};
use super::vault::MultisigVault;
use crate::goldcoin::multisig::{self, MultisigError};
use crate::ledger::{Ledger, LedgerError};
use crate::signing::goldcoin_split::{
    independently_sign_split_all_signers, LedgerSplitSource, RecoverySplitSource, SplitSigningError,
};
use crate::signing::signers::VaultSigner;

/// The operator-configured shaping knobs (`service/src/config.rs`,
/// `goldcoin.utxo_shaping_*` + the canonical `change_fanout_target_atomic`
/// chunk size), bundled like `goldcoin::payout::PayoutPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapingPolicy {
    /// The canonical payout-chunk size — `goldcoin.change_fanout_target_
    /// atomic`, the SAME target every payout's change fan-out uses: one
    /// sizing for the whole crate, never two drifting ones.
    pub chunk_target_atomic: u64,
    /// A new split is only considered while the payout-ready mature count
    /// (Available UTXOs of at least `chunk_target_atomic / 2`) is below
    /// this.
    pub target_available_count: u32,
    /// Minimum size for a split candidate (config-validated to be at
    /// least two whole chunks).
    pub min_source_atomic: u64,
    /// Cap on chunk outputs per split; a larger source splits into
    /// correspondingly larger chunks (each itself a later candidate),
    /// bounding transaction size and shaping gradually.
    pub max_outputs_per_split: usize,
    pub fee_rate_per_kb: u64,
}

/// What one [`run_shaping_tick`] call actually did — at most one of
/// `resumed_split_txid`/`new_split_txid` is ever set (one action per
/// tick), and `skipped` says why nothing was done when both are `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapingOutcome {
    /// A previously stuck (`Built`/`Signed`) split was driven to
    /// `Broadcast` this tick.
    pub resumed_split_txid: Option<[u8; 32]>,
    /// A brand-new split was planned, threshold-signed, and broadcast
    /// this tick.
    pub new_split_txid: Option<[u8; 32]>,
    /// Why no split action happened (pool healthy, chunks still maturing,
    /// no candidate, or the reserve-floor safety check refused).
    pub skipped: Option<String>,
}

#[derive(Debug, Error)]
pub enum ShapingError {
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("goldcoin rpc error: {0}")]
    Rpc(#[from] RpcError),
    #[error("split signing error: {0}")]
    Signing(#[from] SplitSigningError),
    #[error("split planning error: {0}")]
    Planning(#[from] SplitError),
    #[error("split recovery error: {0}")]
    Recovery(#[from] SplitRecoveryError),
    #[error("multisig assembly error: {0}")]
    Multisig(#[from] MultisigError),
    #[error(
        "Goldcoin RPC reports missing inputs broadcasting shaping split #{0} — the source UTXO \
         is no longer spendable on-chain; needs operator investigation, never silently retried"
    )]
    BroadcastConflict(i64),
}

/// See module docs. `threshold` is `operators.vault_threshold`; the first
/// `threshold` entries of `vault_signers` are asked, matching every other
/// vault-spending path in this crate.
#[allow(clippy::too_many_arguments)]
pub async fn run_shaping_tick<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    policy: &ShapingPolicy,
    signer_timeout: Duration,
    now: i64,
) -> Result<ShapingOutcome, ShapingError> {
    // 1. Restart safety: a split already in flight is always finished (or
    // fails loudly) before any new one is considered.
    if let Some(txid) = resume_pending_splits(
        ledger,
        goldcoin_rpc,
        vault,
        vault_signers,
        threshold,
        signer_timeout,
        now,
    )
    .await?
    {
        return Ok(ShapingOutcome {
            resumed_split_txid: Some(txid),
            ..Default::default()
        });
    }

    // 2. Trigger check against the same candidate pool coin selection
    // uses. "Payout-ready" counts only chunks big enough to matter for a
    // real payout (at least half the canonical chunk target) — a pool of
    // slivers is not healthy no matter how many rows it has.
    let available = ledger.available_vault_utxos()?;
    let payout_ready = available
        .iter()
        .filter(|u| u.amount_atomic >= policy.chunk_target_atomic / 2)
        .count();
    if payout_ready as u32 >= policy.target_available_count {
        return Ok(ShapingOutcome {
            skipped: Some(format!(
                "pool healthy: {payout_ready} payout-ready mature UTXOs >= target {}",
                policy.target_available_count
            )),
            ..Default::default()
        });
    }
    let maturing_chunks = ledger.unconfirmed_split_chunk_count()?;
    if maturing_chunks > 0 {
        return Ok(ShapingOutcome {
            skipped: Some(format!(
                "{maturing_chunks} chunk output(s) from a previous split still maturing — not \
                 stacking another self-transaction on top"
            )),
            ..Default::default()
        });
    }

    // 3. Candidate: the largest oversized, never-before-split root-vault
    // UTXO. `available` is already sorted (amount DESC, txid, vout), so
    // the first match is the deterministic choice.
    let root_script = vault.script_pubkey_hex();
    let mut candidate = None;
    for u in &available {
        if u.amount_atomic < policy.min_source_atomic {
            break; // sorted DESC: nothing further qualifies either
        }
        if !u.script_pubkey_hex.eq_ignore_ascii_case(&root_script) {
            continue; // per-request derived deposit UTXOs are never split
        }
        if ledger.get_vault_utxo_split(u.txid, u.vout)?.is_some() {
            continue; // structurally once per outpoint, same as the CLI
        }
        candidate = Some(u.clone());
        break;
    }
    let Some(source) = candidate else {
        return Ok(ShapingOutcome {
            skipped: Some(format!(
                "no mature root-vault UTXO >= {} atomic to split ({payout_ready} payout-ready \
                 UTXOs; pool will replenish via change maturity)",
                policy.min_source_atomic
            )),
            ..Default::default()
        });
    };

    // A very large source splits into `max_outputs_per_split` larger
    // chunks rather than an unbounded number of target-sized ones —
    // deterministic, and each resulting chunk is itself a later candidate
    // if the pool thins again.
    let effective_chunk_target = source
        .amount_atomic
        .div_ceil(policy.max_outputs_per_split as u64)
        .max(policy.chunk_target_atomic);

    // 4. Threshold signing via the identical independent path the CLI
    // uses — including each signer's own reserve-floor refusal. That
    // refusal is an EXPECTED outcome (e.g. the bootstrap shape where the
    // oversized UTXO is most of the reserve), reported as a skip and
    // retried on a later tick, never an error loop.
    let sign_result = {
        let sign_source = LedgerSplitSource { ledger };
        independently_sign_split_all_signers(
            vault_signers,
            vault,
            &sign_source,
            source.txid,
            source.vout,
            effective_chunk_target,
            policy.fee_rate_per_kb,
            threshold,
            signer_timeout,
        )
        .await
    };
    let (plan, mut tx, partials) = match sign_result {
        Ok(v) => v,
        Err(SplitSigningError::SafetyCheckFailed {
            mature_reserve_after,
            required_floor,
        }) => {
            return Ok(ShapingOutcome {
                skipped: Some(format!(
                    "reserve-floor safety check refused splitting {} atomic (mature reserve \
                     after would be {mature_reserve_after} < required floor {required_floor}) — \
                     will retry once independent mature cover grows",
                    source.amount_atomic
                )),
                ..Default::default()
            });
        }
        Err(e) => return Err(e.into()),
    };

    // 5. Persist and broadcast, exactly the CLI's sequence, plus the
    // atomic broadcast effects (source Spent + chunks Unconfirmed).
    let unsigned_hex = crate::goldcoin::hex::encode(&tx.serialize());
    let split_id = ledger.record_vault_utxo_split_built(
        &plan,
        effective_chunk_target,
        &unsigned_hex,
        "auto: utxo liquidity shaping",
        now,
    )?;
    let sighash = tx.sighash_all(0, &vault.redeem_script());
    tx.inputs[0].script_sig = multisig::assemble(vault, &sighash, &partials)?;
    let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
    ledger.record_vault_utxo_split_signed(split_id, &signed_hex, now)?;

    match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(&signed_hex)).await? {
        BroadcastOutcome::Accepted { .. }
        | BroadcastOutcome::AlreadyInChain
        | BroadcastOutcome::AlreadyInMempool => {
            let split_txid = tx.txid();
            ledger.record_vault_utxo_split_broadcast(split_id, split_txid, now)?;
            ledger.record_vault_utxo_split_broadcast_effects(
                source.txid,
                source.vout,
                split_txid,
                &plan.output_amounts,
                &root_script,
                now,
            )?;
            Ok(ShapingOutcome {
                new_split_txid: Some(split_txid),
                ..Default::default()
            })
        }
        BroadcastOutcome::MissingInputs => Err(ShapingError::BroadcastConflict(split_id)),
    }
}

/// Drives the oldest pending (`Built`/`Signed`) split to `Broadcast`.
/// Returns `Some(txid)` if one was driven there this call (that is the
/// tick's one action), `None` if there was nothing pending.
async fn resume_pending_splits<GR: GoldcoinRpc>(
    ledger: &mut Ledger,
    goldcoin_rpc: &GR,
    vault: &MultisigVault,
    vault_signers: &[Box<dyn VaultSigner>],
    threshold: usize,
    signer_timeout: Duration,
    now: i64,
) -> Result<Option<[u8; 32]>, ShapingError> {
    let Some(pending) = ledger.pending_vault_utxo_splits()?.into_iter().next() else {
        return Ok(None);
    };
    let snapshot = ledger
        .get_vault_utxo_split(pending.source_txid, pending.source_vout)?
        .ok_or(LedgerError::VaultUtxoSplitNotFound(pending.id))?;
    let output_amounts = split::distribute_evenly(
        snapshot
            .source_amount_atomic
            .saturating_sub(snapshot.fee_atomic),
        snapshot.chunk_count as u64,
    );
    let root_script = vault.script_pubkey_hex();

    match pending.state.as_str() {
        "Signed" => {
            let outcome = recover_stuck_vault_utxo_split(
                ledger,
                goldcoin_rpc,
                pending.source_txid,
                pending.source_vout,
                now,
            )
            .await?;
            let txid = match outcome {
                SplitRecoveryOutcome::Broadcast { txid, .. } => txid,
                // `AlreadyDone` means another path already drove it to
                // `Broadcast`; effects below are idempotent either way.
                SplitRecoveryOutcome::AlreadyDone { .. } => snapshot
                    .txid
                    .ok_or(LedgerError::VaultUtxoSplitNotFound(pending.id))?,
            };
            ledger.record_vault_utxo_split_broadcast_effects(
                pending.source_txid,
                pending.source_vout,
                txid,
                &output_amounts,
                &root_script,
                now,
            )?;
            Ok(Some(txid))
        }
        "Built" => {
            let sign_result = {
                let sign_source = RecoverySplitSource { ledger };
                independently_sign_split_all_signers(
                    vault_signers,
                    vault,
                    &sign_source,
                    pending.source_txid,
                    pending.source_vout,
                    snapshot.chunk_target_atomic,
                    0, // ignored by RecoverySplitSource: the persisted fee is authoritative
                    threshold,
                    signer_timeout,
                )
                .await
            };
            let (plan, mut tx, partials) = sign_result?;
            let sighash = tx.sighash_all(0, &vault.redeem_script());
            tx.inputs[0].script_sig = multisig::assemble(vault, &sighash, &partials)?;
            let signed_hex = crate::goldcoin::hex::encode(&tx.serialize());
            ledger.record_vault_utxo_split_signed(pending.id, &signed_hex, now)?;
            match call_with_retry(3, || goldcoin_rpc.send_raw_transaction(&signed_hex)).await? {
                BroadcastOutcome::Accepted { .. }
                | BroadcastOutcome::AlreadyInChain
                | BroadcastOutcome::AlreadyInMempool => {
                    let split_txid = tx.txid();
                    ledger.record_vault_utxo_split_broadcast(pending.id, split_txid, now)?;
                    ledger.record_vault_utxo_split_broadcast_effects(
                        pending.source_txid,
                        pending.source_vout,
                        split_txid,
                        &plan.output_amounts,
                        &root_script,
                        now,
                    )?;
                    Ok(Some(split_txid))
                }
                BroadcastOutcome::MissingInputs => Err(ShapingError::BroadcastConflict(pending.id)),
            }
        }
        other => {
            // pending_vault_utxo_splits only returns Built/Signed; anything
            // else is a query/schema drift that must fail loudly.
            Err(ShapingError::Recovery(SplitRecoveryError::NotRecoverable(
                pending.id,
                other.to_string(),
            )))
        }
    }
}
