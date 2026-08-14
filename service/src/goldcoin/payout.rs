//! Payout transaction planning, construction, and pre-signing conservation
//! verification. Reused algorithm from the old bridge's `withdrawal::builder`
//! (docs/01-reuse-inventory.md).
//!
//! `verify_payout_tx` proves every conservation property BEFORE any
//! signature is requested — and, per the approved trust model's
//! independent-re-derivation discipline (docs/02-trust-model.md), this same
//! check is what each vault signer runs against its own re-derived plan
//! before contributing a partial signature (see
//! `crate::signing::goldcoin_vault`), not something the orchestrator's
//! claim is trusted for.

use thiserror::Error;

use super::address::p2pkh_script_hex;
use super::coin::VaultUtxo;
use super::hex;
use super::tx::{Transaction, TxIn, TxOut};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutPlan {
    pub inputs: Vec<VaultUtxo>,
    pub dest_p2pkh_hash: [u8; 20],
    pub payout_atomic: u64,
    /// `0` means no change output.
    pub change_atomic: u64,
    pub vault_script_pubkey: Vec<u8>,
    pub fee_atomic: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PayoutVerifyError {
    #[error("input count mismatch: transaction has {actual}, plan has {expected}")]
    InputCountMismatch { expected: usize, actual: usize },
    #[error("input {index} does not match the reserved outpoint")]
    InputMismatch { index: usize },
    #[error("output count mismatch: transaction has {actual}, expected {expected}")]
    OutputCountMismatch { expected: usize, actual: usize },
    #[error("no output pays the destination the exact planned amount")]
    MissingDestinationOutput,
    #[error("more than one output matches the destination script")]
    AmbiguousDestinationOutput,
    #[error("no output returns change to the vault for the exact planned amount")]
    MissingChangeOutput,
    #[error("an output does not match either the destination or the change script")]
    UnexpectedOutput,
    #[error(
        "value not conserved: inputs {inputs} != payout {payout} + change {change} + fee {fee}"
    )]
    ValueNotConserved {
        inputs: u64,
        payout: u64,
        change: u64,
        fee: u64,
    },
}

/// Builds the unsigned transaction for `plan`. Pure — no RPC involved,
/// unlike the old bridge's `createrawtransaction`-based builder (Goldcoin's
/// RPC round-trip is unnecessary once this crate has its own
/// `Transaction::serialize`, and building locally means `verify_payout_tx`
/// can run against a tx this crate itself fully controls, before ever
/// talking to a node).
pub fn build_unsigned_tx(plan: &PayoutPlan) -> Transaction {
    let inputs = plan
        .inputs
        .iter()
        .map(|u| TxIn::unsigned(u.txid, u.vout))
        .collect();
    let mut outputs = vec![TxOut {
        value_atomic: plan.payout_atomic,
        script_pubkey: hex::decode_vec(&p2pkh_script_hex(&plan.dest_p2pkh_hash)).unwrap(),
    }];
    if plan.change_atomic > 0 {
        outputs.push(TxOut {
            value_atomic: plan.change_atomic,
            script_pubkey: plan.vault_script_pubkey.clone(),
        });
    }
    Transaction {
        version: 1,
        inputs,
        outputs,
        locktime: 0,
    }
}

/// Proves every conservation property of `tx` against `plan` before any
/// signature is requested. See module docs — the checks below are the same
/// ones the old bridge's `verify_payout_tx` performed, in the same order.
pub fn verify_payout_tx(tx: &Transaction, plan: &PayoutPlan) -> Result<(), PayoutVerifyError> {
    if tx.inputs.len() != plan.inputs.len() {
        return Err(PayoutVerifyError::InputCountMismatch {
            expected: plan.inputs.len(),
            actual: tx.inputs.len(),
        });
    }
    for (i, (tx_in, planned)) in tx.inputs.iter().zip(&plan.inputs).enumerate() {
        if tx_in.prev_txid != planned.txid || tx_in.prev_vout != planned.vout {
            return Err(PayoutVerifyError::InputMismatch { index: i });
        }
    }

    let expected_output_count = if plan.change_atomic > 0 { 2 } else { 1 };
    if tx.outputs.len() != expected_output_count {
        return Err(PayoutVerifyError::OutputCountMismatch {
            expected: expected_output_count,
            actual: tx.outputs.len(),
        });
    }

    let dest_script = hex::decode_vec(&p2pkh_script_hex(&plan.dest_p2pkh_hash)).unwrap();
    let dest_matches: Vec<&TxOut> = tx
        .outputs
        .iter()
        .filter(|o| o.script_pubkey == dest_script)
        .collect();
    match dest_matches.len() {
        0 => return Err(PayoutVerifyError::MissingDestinationOutput),
        1 => {
            if dest_matches[0].value_atomic != plan.payout_atomic {
                return Err(PayoutVerifyError::MissingDestinationOutput);
            }
        }
        _ => return Err(PayoutVerifyError::AmbiguousDestinationOutput),
    }

    if plan.change_atomic > 0 {
        let change_ok = tx.outputs.iter().any(|o| {
            o.script_pubkey == plan.vault_script_pubkey && o.value_atomic == plan.change_atomic
        });
        if !change_ok {
            return Err(PayoutVerifyError::MissingChangeOutput);
        }
    }

    for out in &tx.outputs {
        let is_dest = out.script_pubkey == dest_script && out.value_atomic == plan.payout_atomic;
        let is_change = plan.change_atomic > 0
            && out.script_pubkey == plan.vault_script_pubkey
            && out.value_atomic == plan.change_atomic;
        if !is_dest && !is_change {
            return Err(PayoutVerifyError::UnexpectedOutput);
        }
    }

    let total_in: u64 = plan.inputs.iter().map(|u| u.amount_atomic).sum();
    if total_in != plan.payout_atomic + plan.change_atomic + plan.fee_atomic {
        return Err(PayoutVerifyError::ValueNotConserved {
            inputs: total_in,
            payout: plan.payout_atomic,
            change: plan.change_atomic,
            fee: plan.fee_atomic,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan(change_atomic: u64) -> PayoutPlan {
        let input_amount = 1_000_000 + 500 + change_atomic;
        PayoutPlan {
            inputs: vec![VaultUtxo {
                txid: [0xAAu8; 32],
                vout: 0,
                amount_atomic: input_amount,
            }],
            dest_p2pkh_hash: [0x11u8; 20],
            payout_atomic: 1_000_000,
            change_atomic,
            vault_script_pubkey: vec![
                0xa9, 0x14, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22,
                0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x22, 0x87,
            ],
            fee_atomic: 500,
        }
    }

    #[test]
    fn builds_and_verifies_a_no_change_payout() {
        let plan = sample_plan(0);
        let tx = build_unsigned_tx(&plan);
        assert_eq!(tx.outputs.len(), 1);
        verify_payout_tx(&tx, &plan).unwrap();
    }

    #[test]
    fn builds_and_verifies_a_payout_with_change() {
        let plan = sample_plan(50_000);
        let tx = build_unsigned_tx(&plan);
        assert_eq!(tx.outputs.len(), 2);
        verify_payout_tx(&tx, &plan).unwrap();
    }

    #[test]
    fn rejects_a_substituted_input() {
        let plan = sample_plan(0);
        let mut tx = build_unsigned_tx(&plan);
        tx.inputs[0].prev_txid = [0xFFu8; 32];
        assert_eq!(
            verify_payout_tx(&tx, &plan).unwrap_err(),
            PayoutVerifyError::InputMismatch { index: 0 }
        );
    }

    #[test]
    fn rejects_a_tampered_destination_amount() {
        let plan = sample_plan(0);
        let mut tx = build_unsigned_tx(&plan);
        tx.outputs[0].value_atomic += 1;
        assert_eq!(
            verify_payout_tx(&tx, &plan).unwrap_err(),
            PayoutVerifyError::MissingDestinationOutput
        );
    }

    #[test]
    fn rejects_change_sent_to_the_wrong_script() {
        let plan = sample_plan(50_000);
        let mut tx = build_unsigned_tx(&plan);
        tx.outputs[1].script_pubkey = vec![
            0x76, 0xa9, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0xac,
        ];
        assert_eq!(
            verify_payout_tx(&tx, &plan).unwrap_err(),
            PayoutVerifyError::MissingChangeOutput
        );
    }

    #[test]
    fn rejects_an_injected_extra_output() {
        let plan = sample_plan(0);
        let mut tx = build_unsigned_tx(&plan);
        tx.outputs.push(TxOut {
            value_atomic: 1,
            script_pubkey: vec![0x51],
        });
        assert_eq!(
            verify_payout_tx(&tx, &plan).unwrap_err(),
            PayoutVerifyError::OutputCountMismatch {
                expected: 1,
                actual: 2
            }
        );
    }

    #[test]
    fn rejects_value_not_conserved() {
        let mut plan = sample_plan(0);
        plan.fee_atomic += 1; // now inputs != payout + change + fee
        let tx = build_unsigned_tx(&plan);
        assert!(matches!(
            verify_payout_tx(&tx, &plan),
            Err(PayoutVerifyError::ValueNotConserved { .. })
        ));
    }

    #[test]
    fn rejects_wrong_input_count() {
        let plan = sample_plan(0);
        let mut tx = build_unsigned_tx(&plan);
        tx.inputs.push(TxIn::unsigned([0xBBu8; 32], 1));
        assert_eq!(
            verify_payout_tx(&tx, &plan).unwrap_err(),
            PayoutVerifyError::InputCountMismatch {
                expected: 1,
                actual: 2
            }
        );
    }
}
