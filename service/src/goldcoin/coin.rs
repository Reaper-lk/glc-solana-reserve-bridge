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
/// order: exact match (zero change) -> the smaller-change choice between a
/// single covering UTXO and a bounded smallest-first combination -> greedy
/// largest-first accumulation. `candidates` MUST already be sorted
/// `(amount_atomic DESC, txid ASC, vout ASC)` by the caller (the ledger's
/// `available_vault_utxos` query guarantees this) — selection determinism
/// depends on it.
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

    // 2. Smallest single covering UTXO (with a change output) — the
    // cheapest possible choice when one exists, but not necessarily the
    // one that leaves the least value stranded as change: whichever mature
    // UTXO happens to be smallest-while-still-sufficient could still be
    // wildly oversized relative to the payout if every smaller UTXO has
    // already been spent (a real production incident: a ~9,900 GLC payout
    // consumed the vault's one ~100,000 GLC UTXO because it was the only
    // individually-sufficient one, creating ~90,100 GLC of change that sat
    // immature and temporarily pushed spendable reserve below the
    // protected minimum). Before committing to it, this also considers a
    // bounded combination of smaller UTXOs (`smallest_first_combination`) and
    // takes whichever genuinely leaves less new change — never overriding
    // `max_inputs`, and never touched when no single UTXO covers the
    // target at all (see step 3, which needs multiple inputs regardless
    // and keeps minimizing input count as its own, separate goal).
    let fee_single_with_change = fee_for(1, 2, fee_rate_per_kb, input_bytes);
    let target = amount_atomic + fee_single_with_change;
    let smallest_single = candidates
        .iter()
        .filter(|u| u.amount_atomic >= target)
        .min_by_key(|u| (u.amount_atomic, u.txid, u.vout));
    if let Some(single) = smallest_single {
        let single_result = SelectionResult {
            selected: vec![single.clone()],
            total_selected: single.amount_atomic,
            fee_atomic: fee_single_with_change,
        };
        let combination = smallest_first_combination(
            candidates,
            single.amount_atomic,
            amount_atomic,
            fee_rate_per_kb,
            input_bytes,
            max_inputs,
        );
        return Ok(match combination {
            Some(combo)
                if change_of(&combo, amount_atomic) < change_of(&single_result, amount_atomic) =>
            {
                combo
            }
            _ => single_result,
        });
    }

    // 3. Greedy largest-first accumulation (candidates already sorted
    // amount DESC) — reached only when no single UTXO covers the target,
    // so multiple inputs are unavoidable regardless of strategy; minimizing
    // input count (and therefore fee) remains the right goal here.
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

/// The change a selection would leave behind — the metric `select` uses to
/// decide between a single oversized UTXO and a smallest-first combination.
/// Already fee-aware: a combination's higher per-input fee cost is baked
/// into `fee_atomic` before this subtracts it, so comparing this value
/// directly across the two candidates is a fair comparison, not just an
/// input-count preference.
fn change_of(result: &SelectionResult, amount_atomic: u64) -> u64 {
    result
        .total_selected
        .saturating_sub(amount_atomic)
        .saturating_sub(result.fee_atomic)
}

/// Accumulates the SMALLEST mature UTXOs first (ascending amount, ties
/// broken by `txid`/`vout` for the same determinism guarantee `select`
/// itself depends on) until `amount_atomic` plus fee is covered, capped at
/// `max_inputs`. Returns `None` if that many of the smallest UTXOs still
/// isn't enough — `select` falls back to the single-UTXO choice in that
/// case, never exceeding `max_inputs` to force a combination through.
///
/// `exclude_at_or_above` is the single-UTXO candidate's own amount: this
/// search only considers UTXOs STRICTLY SMALLER than it. Without that
/// exclusion, the search would eventually accumulate the oversized UTXO
/// itself alongside a few dust-sized ones (since nothing stops the
/// ascending walk from reaching it once smaller candidates run out),
/// which can shave a negligible amount off the final change while still
/// touching the exact UTXO this whole mechanism exists to avoid, at the
/// cost of a needlessly larger transaction — never the intended trade.
fn smallest_first_combination(
    candidates: &[VaultUtxo],
    exclude_at_or_above: u64,
    amount_atomic: u64,
    fee_rate_per_kb: u64,
    input_bytes: u64,
    max_inputs: usize,
) -> Option<SelectionResult> {
    let mut ascending: Vec<&VaultUtxo> = candidates
        .iter()
        .filter(|u| u.amount_atomic < exclude_at_or_above)
        .collect();
    ascending.sort_by_key(|u| (u.amount_atomic, u.txid, u.vout));

    let mut selected = Vec::new();
    let mut total = 0u64;
    for u in ascending {
        if selected.len() >= max_inputs {
            return None;
        }
        selected.push(u.clone());
        total += u.amount_atomic;
        let fee = fee_for(selected.len(), 2, fee_rate_per_kb, input_bytes);
        if total >= amount_atomic + fee {
            return Some(SelectionResult {
                selected,
                total_selected: total,
                fee_atomic: fee,
            });
        }
    }
    None
}

/// Splits `total_selected - amount_atomic - fee_atomic` into a change
/// amount, folding it entirely into the fee if it would be below
/// `dust_threshold` — an uneconomical/dust output would permanently strand
/// vault value rather than being spendable. `change_atomic == dust_threshold`
/// is kept (inclusive), not folded.
///
/// Superseded by [`finalize_fanout`] for real payout construction (a single
/// change output is exactly `finalize_fanout`'s `max_outputs = 1` case) —
/// kept as its own function because [`super::payout`]'s own conservation
/// tests and a few call sites still reason about a single change amount
/// directly.
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

/// Splits `total_selected - amount_atomic - fee` into deterministic change
/// OUTPUTS — never a single lump — reusing the exact same distribution
/// formula [`super::split::plan_split`] uses for manual vault-UTXO
/// splitting ([`super::split::distribute_evenly`]): one canonical way this
/// crate ever divides a value into near-equal pieces, not two
/// independently-evolving implementations. This is the production fix for
/// a real incident: a burst of Solana->Goldcoin payouts, each producing one
/// large single change output, drained the mature UTXO pool faster than
/// 6-confirmation maturity could replenish it — even after the vault had
/// already been manually pre-split once. Fanning every payout's own change
/// back out (instead of consolidating it into one lump) keeps the mature
/// pool naturally replenished by normal traffic itself.
///
/// Deterministic and fee-aware: starts from `ceil(leftover /
/// target_output_atomic)` change outputs (clamped to `[1, max_outputs]`),
/// then walks DOWN one output at a time — recomputing the REAL fee for
/// that many total outputs each time (`fee_for(num_inputs, 1 + N, ...)`,
/// never trusting `result.fee_atomic`'s own narrower 1-or-2-output
/// assumption) — until every resulting change output clears
/// `dust_threshold`, or until zero change outputs remain (all leftover
/// value folds into the fee, exactly [`finalize`]'s existing single-output
/// dust behavior generalized to N outputs). `num_inputs`/`input_bytes`
/// must be the same values that produced `result` (i.e. `select`'s own
/// `multisig_input_bytes(threshold, redeem_script_len)` and
/// `result.selected.len()`).
///
/// Two independent callers given the same inputs always compute the same
/// `Vec<u64>` in the same order — required so every vault signer
/// independently re-deriving a payout plan builds a byte-identical
/// transaction (docs/02-trust-model.md).
#[allow(clippy::too_many_arguments)]
pub fn finalize_fanout(
    result: &SelectionResult,
    amount_atomic: u64,
    num_inputs: usize,
    input_bytes: u64,
    fee_rate_per_kb: u64,
    dust_threshold: u64,
    target_output_atomic: u64,
    max_outputs: usize,
) -> (Vec<u64>, u64) {
    let max_outputs = max_outputs.max(1);
    let leftover = result.total_selected.saturating_sub(amount_atomic);
    let mut num_change_outputs = if leftover == 0 {
        0
    } else {
        leftover
            .div_ceil(target_output_atomic.max(1))
            .clamp(1, max_outputs as u64) as usize
    };

    loop {
        if num_change_outputs == 0 {
            // No change output at all: everything not paid to the
            // destination goes to the fee — always conservative, since
            // `select` already guarantees `leftover` covers at least the
            // narrower 1-or-2-output fee it planned against.
            return (Vec::new(), leftover);
        }
        let total_outputs = 1 + num_change_outputs;
        let fee = fee_for(num_inputs, total_outputs, fee_rate_per_kb, input_bytes);
        let Some(distributable) = leftover.checked_sub(fee) else {
            num_change_outputs -= 1;
            continue;
        };
        let amounts = super::split::distribute_evenly(distributable, num_change_outputs as u64);
        let smallest = *amounts.iter().min().unwrap_or(&0);
        if smallest < dust_threshold {
            num_change_outputs -= 1;
            continue;
        }
        return (amounts, fee);
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
    fn avoids_a_disproportionately_oversized_single_utxo_when_a_smaller_combination_leaves_less_change(
    ) {
        // Mirrors the production incident: the vault's only individually-
        // sufficient UTXO is enormous relative to the payout, but several
        // smaller mature UTXOs — none sufficient alone — combine to cover
        // it while leaving far less value stranded as new change.
        let candidates = vec![
            utxo(1, 0, 100_000_000), // the one oversized UTXO
            utxo(2, 0, 4_000_000),
            utxo(3, 0, 3_500_000),
            utxo(4, 0, 3_000_000),
        ];
        let result = select(&candidates, 9_900_000, 1000, 2, 105, 10).unwrap();
        assert!(
            result.selected.len() > 1,
            "expected a combination of smaller UTXOs, not the single oversized one"
        );
        assert!(
            result.selected.iter().all(|u| u.txid[0] != 1),
            "the oversized UTXO must not be touched when smaller ones suffice: {:?}",
            result
                .selected
                .iter()
                .map(|u| u.txid[0])
                .collect::<Vec<_>>()
        );
        let (change, _) = finalize(&result, 9_900_000, 1000);
        // The single-UTXO alternative would have left ~90,099,623 in
        // change (100,000,000 - 9,900,000 - fee) — the chosen combination
        // must leave dramatically less.
        assert!(
            change < 1_000_000,
            "expected the combination's change to be small, got {change}"
        );
    }

    #[test]
    fn uses_the_single_utxo_when_no_combination_of_smaller_ones_covers_the_target() {
        // The smaller UTXOs here can never sum enough regardless of how
        // many are combined — the oversized UTXO is genuinely unavoidable,
        // and selection must still fall back to it rather than fail.
        let candidates = vec![
            utxo(1, 0, 100_000_000),
            utxo(2, 0, 10),
            utxo(3, 0, 20),
            utxo(4, 0, 30),
        ];
        let result = select(&candidates, 9_900_000, 1000, 2, 105, 10).unwrap();
        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected[0].txid[0], 1);
    }

    #[test]
    fn a_combination_that_would_exceed_max_inputs_is_not_used() {
        // Five medium UTXOs could combine to cover the target, but only by
        // using more inputs than this vault's configured max_inputs allows
        // — selection must still respect that bound and fall back to the
        // single oversized UTXO rather than exceeding it.
        let candidates = vec![
            utxo(1, 0, 100_000_000),
            utxo(2, 0, 2_100_000),
            utxo(3, 0, 2_100_000),
            utxo(4, 0, 2_100_000),
            utxo(5, 0, 2_100_000),
            utxo(6, 0, 2_100_000),
        ];
        let result = select(&candidates, 9_900_000, 1000, 2, 105, 2).unwrap();
        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected[0].txid[0], 1);
    }

    #[test]
    fn prefers_the_single_utxo_when_it_already_leaves_less_change_than_any_combination() {
        // A reasonably-sized single UTXO that closely covers the target is
        // still the right choice even when a combination is technically
        // available — the comparison must not switch to a combination just
        // because one exists, only when it is genuinely better.
        let candidates = vec![
            utxo(1, 0, 10_000_377), // covers the target almost exactly
            utxo(2, 0, 100_000_000),
            utxo(3, 0, 3_000_000),
        ];
        let result = select(&candidates, 9_900_000, 1000, 2, 105, 10).unwrap();
        assert_eq!(result.selected.len(), 1);
        assert_eq!(result.selected[0].txid[0], 1);
    }

    #[test]
    fn oversized_utxo_avoidance_is_deterministic_across_repeated_calls() {
        let candidates = vec![
            utxo(1, 0, 100_000_000),
            utxo(2, 0, 4_000_000),
            utxo(3, 0, 3_500_000),
            utxo(4, 0, 3_000_000),
        ];
        let a = select(&candidates, 9_900_000, 1000, 2, 105, 10).unwrap();
        let b = select(&candidates, 9_900_000, 1000, 2, 105, 10).unwrap();
        assert_eq!(a.selected, b.selected);
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

    /// 2026-08-30 incident regression, requirement B: a fragmented pool
    /// where a valid <= max_inputs combination EXISTS must produce that
    /// combination, never a spurious `TooManyInputs`. Shape mirrors the
    /// incident: no single UTXO covers a ~19,400 GLC net payout, the pool
    /// is many ~2,425 GLC change fragments, and exactly a 9-input
    /// combination fits within max_inputs = 10.
    #[test]
    fn fragmented_pool_with_a_valid_combination_is_selected_not_rejected() {
        const GLC: u64 = 100_000_000;
        // 30 fragments of ~2,425 GLC — enough total, none sufficient alone.
        let candidates: Vec<_> = (0..30).map(|i| utxo(i, 0, 2_425 * GLC)).collect();
        let target = 19_400 * GLC;
        let result = select(&candidates, target, 1000, 2, 105, 10).unwrap();
        assert!(
            result.selected.len() <= 10,
            "must respect max_inputs, got {}",
            result.selected.len()
        );
        assert_eq!(
            result.selected.len(),
            9,
            "ceil(19,400 / 2,425) = 8 covers only the amount; with the fee, 9 fragments are \
             the minimal count — largest-first must find exactly that"
        );
        assert!(result.total_selected >= target + result.fee_atomic);
    }

    /// Requirement C, the flip side: when NO <= max_inputs combination
    /// exists, `TooManyInputs` is the correct fail-closed answer — the
    /// largest `max_inputs` candidates are the maximum-sum subset of that
    /// size (fee depends only on input count), so if they cannot cover the
    /// target, no subset can, and selection must refuse rather than
    /// exceed the bound or pick something insufficient.
    #[test]
    fn genuinely_infeasible_within_max_inputs_fails_closed() {
        const GLC: u64 = 100_000_000;
        // 30 fragments of 1,500 GLC: the largest 10 sum to 15,000 GLC,
        // below a 19,400 GLC target — infeasible at max_inputs = 10 even
        // though the pool as a whole holds 45,000 GLC.
        let candidates: Vec<_> = (0..30).map(|i| utxo(i, 0, 1_500 * GLC)).collect();
        let err = select(&candidates, 19_400 * GLC, 1000, 2, 105, 10).unwrap_err();
        assert_eq!(err, CoinSelectionError::TooManyInputs { max: 10 });
    }

    /// Exhaustive feasibility-completeness check on a mixed pool: for
    /// every target in a sweep, `select` errs with `TooManyInputs` if and
    /// only if the largest-`max_inputs` subset genuinely cannot cover
    /// `target + fee(max_inputs)` — i.e. the greedy can never miss a
    /// combination some other strategy would have found (the property the
    /// 2026-08-30 incident review demanded be pinned, not just argued).
    #[test]
    fn selection_never_reports_too_many_inputs_when_any_valid_combination_exists() {
        const GLC: u64 = 100_000_000;
        let amounts: Vec<u64> = (0..24)
            .map(|i| (500 + 173 * (i as u64 % 11)) * GLC)
            .collect();
        let candidates: Vec<_> = {
            let mut v: Vec<_> = amounts
                .iter()
                .enumerate()
                .map(|(i, &a)| utxo(i as u8, 0, a))
                .collect();
            v.sort_by(|a, b| {
                b.amount_atomic
                    .cmp(&a.amount_atomic)
                    .then(a.txid.cmp(&b.txid))
                    .then(a.vout.cmp(&b.vout))
            });
            v
        };
        let max_inputs = 6;
        let input_bytes = multisig_input_bytes(2, 105);
        let mut sorted_desc: Vec<u64> = amounts.clone();
        sorted_desc.sort_unstable_by(|a, b| b.cmp(a));
        let best_sum: u64 = sorted_desc.iter().take(max_inputs).sum();
        for step in 0..200u64 {
            let target = (100 + step * 47) * GLC;
            let outcome = select(&candidates, target, 1000, 2, 105, max_inputs);
            // Feasible iff some k <= max_inputs has largest-k sum >=
            // target + fee(k, 2 outputs); since fee grows with k while the
            // largest-k sum grows by whole candidates, checking k =
            // max_inputs with its own fee is the weakest bound — check all
            // k exactly.
            let feasible = (1..=max_inputs).any(|k| {
                let sum: u64 = sorted_desc.iter().take(k).sum();
                sum >= target + fee_for(k, 2, 1000, input_bytes)
            }) || candidates
                .iter()
                .any(|u| u.amount_atomic == target + fee_for(1, 1, 1000, input_bytes));
            match outcome {
                Ok(r) => {
                    assert!(r.selected.len() <= max_inputs);
                    assert!(
                        feasible,
                        "select succeeded on an infeasible target {target}"
                    );
                }
                Err(CoinSelectionError::TooManyInputs { .. }) => {
                    assert!(
                        !feasible,
                        "TooManyInputs for target {target} although a <= {max_inputs}-input \
                         combination exists (best_sum {best_sum})"
                    );
                }
                Err(CoinSelectionError::Insufficient { .. }) => {
                    // The whole pool cannot cover the target — trivially
                    // infeasible at any input bound.
                }
            }
        }
    }

    /// The explicit, tested configuration decision behind raising
    /// production `max_inputs` from 10 to 25
    /// (service/config.pilot-template.toml): a worst-case 25-input 2-of-3
    /// payout with the maximum 11 outputs (1 destination + 10 fanned-out
    /// change) is ~7.8 KB — far below any relay/standardness ceiling —
    /// and at the real production fee rate (100,000 atomic units per KB)
    /// costs under 0.008 GLC, noise against a 20,000 GLC payout.
    #[test]
    fn twenty_five_input_transaction_size_and_fee_are_modest() {
        let input_bytes = multisig_input_bytes(2, 105); // 299 bytes/input
        let size = TX_OVERHEAD_BYTES + 25 * input_bytes + 11 * P2PKH_OUTPUT_BYTES;
        assert_eq!(size, 7_859, "10 + 25*299 + 11*34");
        assert!(size < 100_000, "far below the standard tx size ceiling");
        let fee = fee_for(25, 11, 100_000, input_bytes);
        assert_eq!(fee, 785_900, "atomic units — 0.007859 GLC");
        assert!(
            fee < 100_000_000 / 100,
            "worst-case fee stays under 0.01 GLC"
        );
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
