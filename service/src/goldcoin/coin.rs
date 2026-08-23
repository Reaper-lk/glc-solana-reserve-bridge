//! Deterministic UTXO coin selection and fee sizing. Reused algorithm from
//! the old bridge's `withdrawal::coin` (docs/01-reuse-inventory.md) —
//! selection strategy and P2PKH sizing are chain-mechanics; the multisig
//! input-sizing function is parameterized by `M` here rather than hardcoded
//! to a federation's `N`, per the approved trust model's 2-of-3-and-up
//! sizing (docs/02-trust-model.md).
//!
//! Deterministic by construction: candidates are always considered in a
//! fixed sort order, so the same UTXO set and target always produce the
//! same selection — required so an independent signer re-deriving a payout
//! plan from its own chain view arrives at the identical transaction
//! (docs/02-trust-model.md's independent-re-derivation discipline).

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultUtxo {
    pub txid: [u8; 32],
    pub vout: u32,
    pub amount_atomic: u64,
    /// The scriptPubKey this output actually pays — the shared legacy
    /// vault's script for an old-style deposit, or a request-specific
    /// derived deposit address's script (`goldcoin::derivation::
    /// derive_request_vault`). Selection itself is script-agnostic (fee
    /// sizing is identical regardless: every derived redeem script is the
    /// same length as the root's, since tweaking a compressed pubkey
    /// never changes its size) — this field exists so a caller can
    /// resolve, AFTER selection, exactly which vault/redeem script signs
    /// each selected input (`signing::goldcoin_vault::rederive_plan`).
    pub script_pubkey_hex: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CoinSelectionError {
    #[error("insufficient vault funds: need {required}, only {available} available across all candidates")]
    Insufficient { required: u64, available: u64 },
    #[error("selection would require more than {max} inputs")]
    TooManyInputs { max: usize },
}

const TX_OVERHEAD_BYTES: u64 = 10;
const P2PKH_OUTPUT_BYTES: u64 = 34;
const MAX_SIGNATURE_BYTES: u64 = 73;

/// Byte cost of one P2SH-multisig input for a vault requiring `threshold`
/// signatures, with a redeem script of `redeem_script_len` bytes:
/// `36 (outpoint) + varint(scriptSig_len) + scriptSig_len + 4 (sequence)`,
/// where `scriptSig_len = 1 (OP_0) + threshold * (1 + 73) (push + max-size
/// signature) + redeem_push_prefix + redeem_script_len`. Deliberately
/// distinct from (and larger than) a P2PKH input's cost — undersizing a
/// multisig input's fee was a real defect in the donor codebase (see
/// IMPLEMENTATION_LOG.md), since a 2-of-3 input is roughly double a P2PKH
/// input's size.
pub fn multisig_input_bytes(threshold: u8, redeem_script_len: usize) -> u64 {
    let redeem_push_prefix: u64 = if redeem_script_len < 0x4c {
        1
    } else if redeem_script_len <= 0xff {
        2
    } else {
        3
    };
    let script_sig_len = 1
        + (threshold as u64) * (1 + MAX_SIGNATURE_BYTES)
        + redeem_push_prefix
        + redeem_script_len as u64;
    let script_sig_varint_bytes: u64 = if script_sig_len < 0xfd { 1 } else { 3 };
    36 + script_sig_varint_bytes + script_sig_len + 4
}

/// `ceil(size_bytes * fee_rate_per_kb / 1000)` — rounds up, so a built
/// transaction never underpays its configured rate even by a fraction of
/// an atomic unit.
pub fn fee_for(
    num_inputs: usize,
    num_outputs: usize,
    fee_rate_per_kb: u64,
    input_bytes: u64,
) -> u64 {
    let size = TX_OVERHEAD_BYTES
        + (num_inputs as u64) * input_bytes
        + (num_outputs as u64) * P2PKH_OUTPUT_BYTES;
    (size * fee_rate_per_kb).div_ceil(1000)
}

#[derive(Debug, Clone)]
pub struct SelectionResult {
    pub selected: Vec<VaultUtxo>,
    pub total_selected: u64,
    pub fee_atomic: u64,
}

/// Selects UTXOs to cover `amount_atomic` plus fees, trying strategies in
/// order: exact match (zero change) -> smallest single covering UTXO
/// (minimizes change) -> greedy largest-first accumulation. `candidates`
/// MUST already be sorted `(amount_atomic DESC, txid ASC, vout ASC)` by the
/// caller (the ledger's `available_vault_utxos` query guarantees this) —
/// selection determinism depends on it.
pub fn select(
    candidates: &[VaultUtxo],
    amount_atomic: u64,
    fee_rate_per_kb: u64,
    threshold: u8,
    redeem_script_len: usize,
    max_inputs: usize,
) -> Result<SelectionResult, CoinSelectionError> {
    let input_bytes = multisig_input_bytes(threshold, redeem_script_len);

    // 1. Exact match: a single UTXO covering amount + 1-in/1-out fee with
    // zero change.
    let fee_single_no_change = fee_for(1, 1, fee_rate_per_kb, input_bytes);
    if let Some(u) = candidates
        .iter()
        .find(|u| u.amount_atomic == amount_atomic + fee_single_no_change)
    {
        return Ok(SelectionResult {
            selected: vec![u.clone()],
            total_selected: u.amount_atomic,
            fee_atomic: fee_single_no_change,
        });
    }

    // 2. Smallest single covering UTXO (with a change output).
    let fee_single_with_change = fee_for(1, 2, fee_rate_per_kb, input_bytes);
    let target = amount_atomic + fee_single_with_change;
    if let Some(u) = candidates
        .iter()
        .filter(|u| u.amount_atomic >= target)
        .min_by_key(|u| u.amount_atomic)
    {
        return Ok(SelectionResult {
            selected: vec![u.clone()],
            total_selected: u.amount_atomic,
            fee_atomic: fee_single_with_change,
        });
    }

    // 3. Greedy largest-first accumulation (candidates already sorted
    // amount DESC).
    let mut selected = Vec::new();
    let mut total = 0u64;
    for u in candidates {
        selected.push(u.clone());
        total += u.amount_atomic;
        if selected.len() > max_inputs {
            return Err(CoinSelectionError::TooManyInputs { max: max_inputs });
        }
        let fee = fee_for(selected.len(), 2, fee_rate_per_kb, input_bytes);
        if total >= amount_atomic + fee {
            return Ok(SelectionResult {
                selected,
                total_selected: total,
                fee_atomic: fee,
            });
        }
    }

    let available: u64 = candidates.iter().map(|u| u.amount_atomic).sum();
    Err(CoinSelectionError::Insufficient {
        required: amount_atomic + fee_for(candidates.len().max(1), 2, fee_rate_per_kb, input_bytes),
        available,
    })
}

/// Splits `total_selected - amount_atomic - fee_atomic` into a change
/// amount, folding it entirely into the fee if it would be below
/// `dust_threshold` — an uneconomical/dust output would permanently strand
/// vault value rather than being spendable. `change_atomic == dust_threshold`
/// is kept (inclusive), not folded.
pub fn finalize(result: &SelectionResult, amount_atomic: u64, dust_threshold: u64) -> (u64, u64) {
    let raw_change = result
        .total_selected
        .saturating_sub(amount_atomic)
        .saturating_sub(result.fee_atomic);
    if raw_change > 0 && raw_change < dust_threshold {
        (0, result.fee_atomic + raw_change)
    } else {
        (raw_change, result.fee_atomic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utxo(txid_byte: u8, vout: u32, amount: u64) -> VaultUtxo {
        VaultUtxo {
            txid: [txid_byte; 32],
            vout,
            amount_atomic: amount,
            script_pubkey_hex: "deadbeef".to_string(),
        }
    }

    #[test]
    fn multisig_input_bytes_worked_example_matches_the_reference_calculation() {
        // 2-of-3, 105-byte redeem script -> 299 bytes (docs/01-reuse-
        // inventory.md / IMPLEMENTATION_LOG.md worked example).
        assert_eq!(multisig_input_bytes(2, 105), 299);
    }

    #[test]
    fn multisig_input_is_larger_than_naive_p2pkh_sizing() {
        // A P2PKH input is ~148 bytes; any real multisig config must cost
        // meaningfully more, or fee sizing has regressed to the old bug.
        assert!(multisig_input_bytes(2, 105) > 148);
    }

    #[test]
    fn fee_for_rounds_up() {
        // Construct a case where size * rate is not a multiple of 1000.
        let fee = fee_for(1, 1, 1, 999); // size = 10 + 999 + 34 = 1043; *1/1000 = 1.043 -> 2
        assert_eq!(fee, 2);
    }

    #[test]
    fn exact_match_selects_a_single_utxo_with_zero_change() {
        let input_bytes = multisig_input_bytes(2, 105);
        let fee = fee_for(1, 1, 1000, input_bytes);
        let candidates = vec![utxo(1, 0, 1_000_000 + fee), utxo(2, 0, 5_000_000)];
        let result = select(&candidates, 1_000_000, 1000, 2, 105, 10).unwrap();
        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected[0].txid[0], 1);
        let (change, _) = finalize(&result, 1_000_000, 1000);
        assert_eq!(change, 0);
    }

    #[test]
    fn smallest_covering_utxo_is_chosen_when_no_exact_match() {
        let candidates = vec![
            utxo(1, 0, 10_000_000),
            utxo(2, 0, 2_000_000),
            utxo(3, 0, 3_000_000),
        ];
        let result = select(&candidates, 1_000_000, 1000, 2, 105, 10).unwrap();
        assert_eq!(result.selected.len(), 1);
        assert_eq!(
            result.selected[0].txid[0], 2,
            "must pick the smallest UTXO that still covers the target, not the largest"
        );
    }

    #[test]
    fn greedy_accumulates_multiple_utxos_when_no_single_one_covers() {
        let candidates = vec![
            utxo(1, 0, 600_000),
            utxo(2, 0, 500_000),
            utxo(3, 0, 400_000),
        ];
        let result = select(&candidates, 1_000_000, 1000, 2, 105, 10).unwrap();
        assert!(result.selected.len() >= 2);
        assert!(result.total_selected >= 1_000_000);
    }

    #[test]
    fn insufficient_funds_is_reported_not_panicked() {
        let candidates = vec![utxo(1, 0, 100)];
        let err = select(&candidates, 1_000_000, 1000, 2, 105, 10).unwrap_err();
        assert!(matches!(err, CoinSelectionError::Insufficient { .. }));
    }

    #[test]
    fn too_many_inputs_is_reported_not_silently_exceeded() {
        let candidates: Vec<_> = (0..20).map(|i| utxo(i, 0, 1)).collect();
        let err = select(&candidates, 15, 1000, 2, 105, 5).unwrap_err();
        assert_eq!(err, CoinSelectionError::TooManyInputs { max: 5 });
    }

    #[test]
    fn dust_change_is_folded_into_fee_not_emitted_as_an_output() {
        // total_selected - amount - fee = 1_001_000 - 1_000_000 - 500 = 500,
        // which is below the 1000 dust threshold.
        let result = SelectionResult {
            selected: vec![utxo(1, 0, 1_001_000)],
            total_selected: 1_001_000,
            fee_atomic: 500,
        };
        let (change, fee) = finalize(&result, 1_000_000, 1000);
        assert_eq!(change, 0, "sub-dust change must not become an output");
        assert_eq!(fee, 1000, "folded change must land entirely in the fee");
    }

    #[test]
    fn change_at_exactly_the_dust_threshold_is_kept() {
        let result = SelectionResult {
            selected: vec![utxo(1, 0, 1_001_500)],
            total_selected: 1_001_500,
            fee_atomic: 500,
        };
        let (change, fee) = finalize(&result, 1_000_000, 1000);
        assert_eq!(
            change, 1000,
            "change exactly at the dust threshold is inclusive-kept"
        );
        assert_eq!(fee, 500);
    }

    #[test]
    fn selection_is_deterministic_across_repeated_calls() {
        let candidates = vec![
            utxo(1, 0, 600_000),
            utxo(2, 0, 500_000),
            utxo(3, 0, 400_000),
        ];
        let a = select(&candidates, 1_000_000, 1000, 2, 105, 10).unwrap();
        let b = select(&candidates, 1_000_000, 1000, 2, 105, 10).unwrap();
        assert_eq!(
            a.selected, b.selected,
            "identical inputs must always produce an identical selection"
        );
    }
}
