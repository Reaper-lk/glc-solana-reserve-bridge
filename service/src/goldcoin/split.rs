//! Vault UTXO splitting: an operator-triggered, root-vault-only
//! transaction that consumes one large mature vault UTXO and produces
//! several smaller UTXOs, all still owned by the same vault script.
//!
//! # Why this exists
//!
//! [`crate::goldcoin::coin::select`] (the production incident this module
//! answers) already prefers a bounded combination of smaller mature UTXOs
//! over one oversized one, but it can only choose among UTXOs that
//! actually exist. When the vault's mature liquidity is concentrated in
//! one very large UTXO, a payout has no choice but to consume it, creating
//! a large immature change output and temporarily starving spendable
//! reserve — a real production incident (docs/09-runbook.md). This module
//! lets an operator proactively fragment such a UTXO ahead of time, using
//! the same signing/broadcast machinery every other vault-spending
//! operation in this crate uses — see [`crate::signing::goldcoin_split`]
//! for the independent multi-signer signing path, and `glc-admin`'s
//! `split-vault-utxo` command for the operator-facing entry point.
//!
//! # Deterministic by construction
//!
//! Exactly like [`super::coin::select`], the same `(source, chunk_target,
//! fee_rate_per_kb)` always produces the same [`SplitPlan`] — required so
//! every independent signer re-deriving this plan from its own ledger view
//! arrives at byte-identical output amounts (docs/02-trust-model.md's
//! independent-re-derivation discipline).

use thiserror::Error;

use super::coin::{self, VaultUtxo};
use super::tx::{Transaction, TxIn, TxOut};
use super::vault::MultisigVault;

/// A UTXO must be at least this many times its target chunk size before
/// splitting is considered worthwhile — enforced structurally by
/// [`plan_split`] requiring at least 2 whole chunks, never a request to
/// carve a barely-oversized UTXO into one near-full-size piece and one
/// sliver.
pub const MIN_SPLIT_OUTPUTS: usize = 2;

/// No split output may be smaller than this, regardless of how the chunk
/// target divides the source UTXO — 1,000 GLC (8 decimals,
/// `amount_conversion::GOLDCOIN_DECIMALS`). Well above `dust_threshold`
/// (an economic floor, not a network-relay one): a split exists to create
/// USEFUL future payout liquidity, not to fragment the vault into pieces
/// too small to matter.
pub const MIN_CHUNK_FLOOR_ATOMIC: u64 = 1_000 * 100_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    pub source: VaultUtxo,
    /// Every output pays this exact script — the vault's own, i.e. the
    /// same script the source UTXO itself already belongs to. Never a
    /// derived or external destination.
    pub vault_script_pubkey: Vec<u8>,
    /// One entry per output, in construction order — deterministic and
    /// index-aligned with the transaction [`build_unsigned_split_tx`]
    /// produces.
    pub output_amounts: Vec<u64>,
    pub fee_atomic: u64,
}

impl SplitPlan {
    pub fn output_count(&self) -> usize {
        self.output_amounts.len()
    }

    pub fn total_output_atomic(&self) -> u64 {
        self.output_amounts.iter().sum()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SplitError {
    #[error("chunk target must be greater than zero")]
    InvalidChunkTarget,
    #[error("source UTXO {source_amount} is not large enough to produce at least {MIN_SPLIT_OUTPUTS} chunks of {chunk_target} — not worth splitting")]
    NotWorthSplitting {
        source_amount: u64,
        chunk_target: u64,
    },
    #[error("source UTXO {source_amount} cannot cover the fee ({fee}) for the planned split")]
    InsufficientForFee { source_amount: u64, fee: u64 },
    #[error("planned chunk amount {chunk_amount} would fall below the minimum useful chunk size {floor}")]
    ChunkBelowFloor { chunk_amount: u64, floor: u64 },
}

/// Deterministically plans a split of `source` into as many roughly-equal
/// chunks of about `chunk_target_atomic` as fit, all paying back to
/// `vault`'s own script. `source.script_pubkey_hex` is NOT checked against
/// `vault` here (that is the caller's responsibility, matching how
/// `coin::select`'s candidates are pre-filtered by its own caller) — this
/// function only does the arithmetic.
///
/// `num_outputs = ceil(source.amount_atomic / chunk_target_atomic)`,
/// refused outright if that is fewer than [`MIN_SPLIT_OUTPUTS`] (the UTXO
/// is not meaningfully larger than one chunk). The fee for that many
/// outputs is computed once (fee size depends on `num_outputs`, so this is
/// not circular — `num_outputs` is fixed before fee sizing happens), then
/// `source.amount_atomic - fee` is divided as evenly as possible: every
/// output gets `distributable / num_outputs`, and the first
/// `distributable % num_outputs` outputs each get one extra atomic unit —
/// deterministic and reproducible by any independent caller given the same
/// inputs.
pub fn plan_split(
    source: &VaultUtxo,
    vault: &MultisigVault,
    chunk_target_atomic: u64,
    fee_rate_per_kb: u64,
) -> Result<SplitPlan, SplitError> {
    if chunk_target_atomic == 0 {
        return Err(SplitError::InvalidChunkTarget);
    }
    let num_outputs = source.amount_atomic.div_ceil(chunk_target_atomic);
    if num_outputs < MIN_SPLIT_OUTPUTS as u64 {
        return Err(SplitError::NotWorthSplitting {
            source_amount: source.amount_atomic,
            chunk_target: chunk_target_atomic,
        });
    }
    let num_outputs = num_outputs as usize;

    let input_bytes = coin::multisig_input_bytes(vault.threshold, vault.redeem_script().len());
    let fee_atomic = coin::fee_for(1, num_outputs, fee_rate_per_kb, input_bytes);
    let distributable =
        source
            .amount_atomic
            .checked_sub(fee_atomic)
            .ok_or(SplitError::InsufficientForFee {
                source_amount: source.amount_atomic,
                fee: fee_atomic,
            })?;

    // The base (pre-remainder) amount is the SMALLEST any output can be —
    // `distribute_evenly` only ever adds to it, never subtracts — so
    // checking it alone is sufficient to guarantee every output clears the
    // floor.
    let base = distributable / num_outputs as u64;
    if base < MIN_CHUNK_FLOOR_ATOMIC {
        return Err(SplitError::ChunkBelowFloor {
            chunk_amount: base,
            floor: MIN_CHUNK_FLOOR_ATOMIC,
        });
    }
    let output_amounts = distribute_evenly(distributable, num_outputs as u64);

    Ok(SplitPlan {
        source: source.clone(),
        vault_script_pubkey: vault.script_pubkey(),
        output_amounts,
        fee_atomic,
    })
}

/// Deterministically distributes `distributable_atomic` into `chunk_count`
/// outputs: every output gets `distributable_atomic / chunk_count`, and the
/// first `distributable_atomic % chunk_count` outputs each get one extra
/// atomic unit — the exact formula [`plan_split`] uses to build a split's
/// outputs, factored out so it can be reproduced independently from
/// already-persisted figures (source amount, fee, chunk count) without
/// re-deriving fee from a possibly-since-changed `fee_rate_per_kb`. See
/// [`matches_expected_split_output`], which is exactly this reproduction.
pub fn distribute_evenly(distributable_atomic: u64, chunk_count: u64) -> Vec<u64> {
    if chunk_count == 0 {
        return Vec::new();
    }
    let base = distributable_atomic / chunk_count;
    let remainder = distributable_atomic % chunk_count;
    (0..chunk_count)
        .map(|i| if i < remainder { base + 1 } else { base })
        .collect()
}

/// Reconstructs the exact [`SplitPlan`] a persisted `vault_utxo_splits`
/// row was built from, using only already-persisted figures (source
/// amount, fee, chunk count) — the split analog of
/// [`crate::goldcoin::payout_recovery`]'s reconstruct-and-verify
/// discipline, and the same `distribute_evenly` reproduction
/// [`matches_expected_split_output`] already relies on. Deliberately does
/// NOT re-derive the fee from the current `fee_rate_per_kb` (which may
/// have changed since the row was built); the caller MUST verify the
/// reconstruction against the persisted `unsigned_tx_hex` byte-for-byte
/// before trusting it (see `signing::goldcoin_split::RecoverySplitSource`).
pub fn reconstruct_plan(
    source: &VaultUtxo,
    vault: &MultisigVault,
    chunk_count: u64,
    fee_atomic: u64,
) -> Result<SplitPlan, SplitError> {
    let distributable =
        source
            .amount_atomic
            .checked_sub(fee_atomic)
            .ok_or(SplitError::InsufficientForFee {
                source_amount: source.amount_atomic,
                fee: fee_atomic,
            })?;
    Ok(SplitPlan {
        source: source.clone(),
        vault_script_pubkey: vault.script_pubkey(),
        output_amounts: distribute_evenly(distributable, chunk_count),
        fee_atomic,
    })
}

/// Whether `(vout, amount_atomic)` is exactly the output a split of
/// `source_amount_atomic` into `chunk_count` chunks (with the given,
/// already-persisted `fee_atomic`) would have produced at that index —
/// the check `goldcoin::indexer` uses to recognize its own split outputs
/// as internal vault movements rather than unexplained deposits
/// (docs/09-runbook.md's "Vault UTXO splitting" section). Exact match
/// only: a mismatched amount, or a `vout` beyond `chunk_count`, is `false`
/// — this never accepts an output merely because it belongs to a
/// recognized split transaction, only because it is byte-for-byte the
/// specific output that split was persisted to produce.
pub fn matches_expected_split_output(
    source_amount_atomic: u64,
    fee_atomic: u64,
    chunk_count: u64,
    vout: u32,
    amount_atomic: u64,
) -> bool {
    if u64::from(vout) >= chunk_count {
        return false;
    }
    let Some(distributable) = source_amount_atomic.checked_sub(fee_atomic) else {
        return false;
    };
    distribute_evenly(distributable, chunk_count).get(vout as usize) == Some(&amount_atomic)
}

/// Builds the unsigned split transaction for `plan`: one input (the source
/// UTXO), one output per `plan.output_amounts` entry, every output paying
/// `plan.vault_script_pubkey`. Pure, like
/// [`crate::goldcoin::payout::build_unsigned_tx`] — no RPC involved.
pub fn build_unsigned_split_tx(plan: &SplitPlan) -> Transaction {
    let inputs = vec![TxIn::unsigned(plan.source.txid, plan.source.vout)];
    let outputs = plan
        .output_amounts
        .iter()
        .map(|&amount| TxOut {
            value_atomic: amount,
            script_pubkey: plan.vault_script_pubkey.clone(),
        })
        .collect();
    Transaction {
        version: 1,
        inputs,
        outputs,
        locktime: 0,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SplitVerifyError {
    #[error("split transaction must have exactly one input, found {0}")]
    InputCountMismatch(usize),
    #[error("input does not match the source outpoint")]
    InputMismatch,
    #[error("output count mismatch: transaction has {actual}, plan has {expected}")]
    OutputCountMismatch { expected: usize, actual: usize },
    #[error("output {index} does not pay the vault's own script")]
    OutputScriptMismatch { index: usize },
    #[error("output {index} amount {actual} does not match the planned amount {expected}")]
    OutputAmountMismatch {
        index: usize,
        expected: u64,
        actual: u64,
    },
    #[error("value not conserved: source {source_amount} != outputs {outputs} + fee {fee}")]
    ValueNotConserved {
        source_amount: u64,
        outputs: u64,
        fee: u64,
    },
}

/// Proves every conservation property of `tx` against `plan` before any
/// signature is requested — the split-transaction equivalent of
/// [`crate::goldcoin::payout::verify_payout_tx`]. Every output is checked,
/// in order, against `plan.output_amounts` (rather than the
/// script-plus-amount matching `verify_payout_tx` uses, which relies on a
/// destination output and a change output having different scripts) —
/// every split output pays the identical script, so exact positional
/// agreement with the plan's deterministic construction is the only
/// meaningful check.
pub fn verify_split_tx(tx: &Transaction, plan: &SplitPlan) -> Result<(), SplitVerifyError> {
    if tx.inputs.len() != 1 {
        return Err(SplitVerifyError::InputCountMismatch(tx.inputs.len()));
    }
    if tx.inputs[0].prev_txid != plan.source.txid || tx.inputs[0].prev_vout != plan.source.vout {
        return Err(SplitVerifyError::InputMismatch);
    }

    if tx.outputs.len() != plan.output_amounts.len() {
        return Err(SplitVerifyError::OutputCountMismatch {
            expected: plan.output_amounts.len(),
            actual: tx.outputs.len(),
        });
    }
    for (i, (out, &expected)) in tx.outputs.iter().zip(&plan.output_amounts).enumerate() {
        if out.script_pubkey != plan.vault_script_pubkey {
            return Err(SplitVerifyError::OutputScriptMismatch { index: i });
        }
        if out.value_atomic != expected {
            return Err(SplitVerifyError::OutputAmountMismatch {
                index: i,
                expected,
                actual: out.value_atomic,
            });
        }
    }

    let outputs_total = plan.total_output_atomic();
    if plan.source.amount_atomic != outputs_total + plan.fee_atomic {
        return Err(SplitVerifyError::ValueNotConserved {
            source_amount: plan.source.amount_atomic,
            outputs: outputs_total,
            fee: plan.fee_atomic,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goldcoin::address::Network;

    fn fake_pubkey(seed: u8) -> [u8; 33] {
        let mut bytes = [0u8; 32];
        bytes[0] = 1;
        bytes[31] = seed;
        let sk = libsecp256k1::SecretKey::parse(&bytes).unwrap();
        libsecp256k1::PublicKey::from_secret_key(&sk).serialize_compressed()
    }

    fn test_vault() -> MultisigVault {
        MultisigVault::new(
            vec![fake_pubkey(1), fake_pubkey(2), fake_pubkey(3)],
            2,
            Network::Testnet,
        )
        .unwrap()
    }

    fn source_utxo(amount_atomic: u64) -> VaultUtxo {
        VaultUtxo {
            txid: [0xABu8; 32],
            vout: 3,
            amount_atomic,
            script_pubkey_hex: "deadbeef".to_string(),
        }
    }

    /// Roughly the production incident's numbers: a ~90,100 GLC UTXO, a
    /// 12,500 GLC chunk target.
    const INCIDENT_AMOUNT: u64 = 90_099 * 100_000_000 + 99_962_300; // ~90,099.999623 GLC
    const CHUNK_TARGET: u64 = 12_500 * 100_000_000;

    #[test]
    fn splits_the_incident_utxo_into_the_expected_chunk_count() {
        let vault = test_vault();
        let plan = plan_split(&source_utxo(INCIDENT_AMOUNT), &vault, CHUNK_TARGET, 1000).unwrap();
        assert_eq!(plan.output_count(), 8, "ceil(90,100 / 12,500) == 8");
        for &amount in &plan.output_amounts {
            assert!(amount >= MIN_CHUNK_FLOOR_ATOMIC);
            assert!(
                amount < CHUNK_TARGET,
                "each chunk should be near, not above, the target"
            );
        }
    }

    #[test]
    fn every_output_pays_the_vault_script_and_value_is_conserved() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let plan = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        assert!(plan
            .output_amounts
            .iter()
            .all(|_| plan.vault_script_pubkey == vault.script_pubkey()));
        assert_eq!(
            plan.total_output_atomic() + plan.fee_atomic,
            source.amount_atomic
        );
    }

    #[test]
    fn distributes_the_remainder_across_the_first_outputs_deterministically() {
        let vault = test_vault();
        // Chosen so the division is inexact (easy to hand-check the
        // remainder) while every resulting chunk still clears
        // MIN_CHUNK_FLOOR_ATOMIC: a chunk target of 2,000 GLC, a source
        // just above 3x that (forcing 4 outputs of ~1,500 GLC each).
        let chunk_target = 2_000 * 100_000_000;
        let source = source_utxo(3 * chunk_target + 7);
        let plan = plan_split(&source, &vault, chunk_target, 1000).unwrap();
        assert_eq!(plan.output_count(), 4);
        let mut sorted = plan.output_amounts.clone();
        sorted.sort_unstable();
        // At most a 1-atomic-unit spread between the largest and smallest
        // chunk.
        assert!(sorted[sorted.len() - 1] - sorted[0] <= 1);
    }

    #[test]
    fn plan_split_is_deterministic_across_repeated_calls() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let a = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        let b = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn refuses_a_utxo_that_is_not_worth_splitting() {
        let vault = test_vault();
        // Exactly at (or below) one chunk -> ceil(amount / target) == 1.
        let source = source_utxo(CHUNK_TARGET);
        let err = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap_err();
        assert!(matches!(err, SplitError::NotWorthSplitting { .. }));
    }

    #[test]
    fn refuses_a_zero_chunk_target() {
        let vault = test_vault();
        let err = plan_split(&source_utxo(INCIDENT_AMOUNT), &vault, 0, 1000).unwrap_err();
        assert_eq!(err, SplitError::InvalidChunkTarget);
    }

    #[test]
    fn refuses_a_plan_whose_chunks_would_fall_below_the_floor() {
        let vault = test_vault();
        // 2 chunks requested, but the source is only just above 2x the
        // floor's neighborhood with a tiny chunk target relative to source,
        // forcing many tiny outputs.
        let tiny_target = MIN_CHUNK_FLOOR_ATOMIC / 10;
        let source = source_utxo(MIN_CHUNK_FLOOR_ATOMIC * 3); // -> many small chunks
        let err = plan_split(&source, &vault, tiny_target, 1000).unwrap_err();
        assert!(matches!(err, SplitError::ChunkBelowFloor { .. }));
    }

    #[test]
    fn build_and_verify_round_trips_a_valid_plan() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let plan = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        let tx = build_unsigned_split_tx(&plan);
        assert_eq!(tx.outputs.len(), plan.output_count());
        verify_split_tx(&tx, &plan).unwrap();
    }

    #[test]
    fn verify_rejects_a_tampered_output_amount() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let plan = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        let mut tx = build_unsigned_split_tx(&plan);
        tx.outputs[0].value_atomic += 1;
        assert!(matches!(
            verify_split_tx(&tx, &plan).unwrap_err(),
            SplitVerifyError::OutputAmountMismatch { index: 0, .. }
        ));
    }

    #[test]
    fn verify_rejects_an_output_paying_a_different_script() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let plan = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        let mut tx = build_unsigned_split_tx(&plan);
        tx.outputs[1].script_pubkey = vec![0x51];
        assert!(matches!(
            verify_split_tx(&tx, &plan).unwrap_err(),
            SplitVerifyError::OutputScriptMismatch { index: 1 }
        ));
    }

    #[test]
    fn verify_rejects_a_substituted_input() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let plan = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        let mut tx = build_unsigned_split_tx(&plan);
        tx.inputs[0].prev_txid = [0xFFu8; 32];
        assert_eq!(
            verify_split_tx(&tx, &plan).unwrap_err(),
            SplitVerifyError::InputMismatch
        );
    }

    #[test]
    fn verify_rejects_an_injected_extra_output() {
        let vault = test_vault();
        let source = source_utxo(INCIDENT_AMOUNT);
        let plan = plan_split(&source, &vault, CHUNK_TARGET, 1000).unwrap();
        let mut tx = build_unsigned_split_tx(&plan);
        tx.outputs.push(TxOut {
            value_atomic: 1,
            script_pubkey: plan.vault_script_pubkey.clone(),
        });
        assert!(matches!(
            verify_split_tx(&tx, &plan).unwrap_err(),
            SplitVerifyError::OutputCountMismatch { .. }
        ));
    }

    /// The exact production values from the vault-split-indexing incident
    /// (docs/09-runbook.md): a 67,270 GLC UTXO split into 6 chunks against
    /// a 12,500 GLC target, producing four outputs of 11,211.66658117 GLC
    /// and two of 11,211.66658116 GLC — pinned here as a real-world-derived
    /// golden vector, not a synthetic one.
    const INCIDENT_SOURCE_AMOUNT: u64 = 67_270 * 100_000_000; // 6,727,000,000,000
    const INCIDENT_FEE_ATOMIC: u64 = 51_300;
    const INCIDENT_CHUNK_COUNT: u64 = 6;
    const INCIDENT_LARGER_OUTPUT: u64 = 1_121_166_658_117; // 11,211.66658117 GLC
    const INCIDENT_SMALLER_OUTPUT: u64 = 1_121_166_658_116; // 11,211.66658116 GLC

    #[test]
    fn distribute_evenly_reproduces_the_exact_production_split_incident_values() {
        let distributable = INCIDENT_SOURCE_AMOUNT - INCIDENT_FEE_ATOMIC;
        let amounts = distribute_evenly(distributable, INCIDENT_CHUNK_COUNT);
        assert_eq!(
            amounts,
            vec![
                INCIDENT_LARGER_OUTPUT,
                INCIDENT_LARGER_OUTPUT,
                INCIDENT_LARGER_OUTPUT,
                INCIDENT_LARGER_OUTPUT,
                INCIDENT_SMALLER_OUTPUT,
                INCIDENT_SMALLER_OUTPUT,
            ]
        );
        assert_eq!(amounts.iter().sum::<u64>(), distributable);
    }

    #[test]
    fn distribute_evenly_splits_exactly_with_no_remainder() {
        assert_eq!(distribute_evenly(900, 3), vec![300, 300, 300]);
    }

    #[test]
    fn distribute_evenly_of_zero_chunks_is_empty() {
        assert_eq!(distribute_evenly(1_000, 0), Vec::<u64>::new());
    }

    #[test]
    fn matches_expected_split_output_accepts_every_real_production_output() {
        for (vout, &expected) in [
            INCIDENT_LARGER_OUTPUT,
            INCIDENT_LARGER_OUTPUT,
            INCIDENT_LARGER_OUTPUT,
            INCIDENT_LARGER_OUTPUT,
            INCIDENT_SMALLER_OUTPUT,
            INCIDENT_SMALLER_OUTPUT,
        ]
        .iter()
        .enumerate()
        {
            assert!(
                matches_expected_split_output(
                    INCIDENT_SOURCE_AMOUNT,
                    INCIDENT_FEE_ATOMIC,
                    INCIDENT_CHUNK_COUNT,
                    vout as u32,
                    expected,
                ),
                "vout {vout} expected to match"
            );
        }
    }

    #[test]
    fn matches_expected_split_output_rejects_a_tampered_amount() {
        assert!(!matches_expected_split_output(
            INCIDENT_SOURCE_AMOUNT,
            INCIDENT_FEE_ATOMIC,
            INCIDENT_CHUNK_COUNT,
            0,
            INCIDENT_LARGER_OUTPUT + 1,
        ));
    }

    #[test]
    fn matches_expected_split_output_rejects_a_vout_beyond_chunk_count() {
        assert!(!matches_expected_split_output(
            INCIDENT_SOURCE_AMOUNT,
            INCIDENT_FEE_ATOMIC,
            INCIDENT_CHUNK_COUNT,
            6,
            INCIDENT_SMALLER_OUTPUT,
        ));
    }

    #[test]
    fn matches_expected_split_output_rejects_a_fee_larger_than_the_source() {
        assert!(!matches_expected_split_output(100, 1_000, 2, 0, 0,));
    }

    #[test]
    fn matches_expected_split_output_does_not_accept_a_value_from_the_wrong_vout() {
        // The larger amount really is at vout 0..3, not vout 4/5 — a value
        // that merely belongs to the SAME split but at the wrong index must
        // still be rejected (exact (vout, amount) match, not "amount
        // appears somewhere in this split").
        assert!(!matches_expected_split_output(
            INCIDENT_SOURCE_AMOUNT,
            INCIDENT_FEE_ATOMIC,
            INCIDENT_CHUNK_COUNT,
            4,
            INCIDENT_LARGER_OUTPUT,
        ));
    }
}
