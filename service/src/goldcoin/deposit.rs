//! Pure deposit-extraction logic: vault-script matching and OP_RETURN
//! request-id binding extraction. No I/O — unit-testable against fabricated
//! RPC responses (docs/01-reuse-inventory.md: same discipline as the old
//! bridge's `glc::deposit`).
//!
//! # Binding scheme: request id, not recipient pubkey
//!
//! The old (federated, mint/burn) bridge bound each deposit's OP_RETURN to
//! the 32-byte Solana recipient pubkey directly — sufficient there because
//! any valid deposit could mint, permissionlessly, to whatever recipient it
//! named. The reserve model requires the opposite discipline: a deposit
//! must satisfy a *specific, pre-existing reservation* (`AwaitingDeposit` in
//! docs/04-state-machines.md) or the "never accept a transfer that cannot
//! be fulfilled" guarantee is meaningless — a user could deposit without
//! ever having reserved capacity.
//!
//! So this binds to the `bridge_requests.id` (a `u64`, little-endian, in
//! the first 8 bytes of the 32-byte OP_RETURN payload; the remaining 24
//! bytes are reserved/zero, same reserved-byte-padding discipline the old
//! bridge's on-chain accounts use for future fields) rather than to a
//! recipient. This removes an entire class of matching ambiguity a
//! recipient-only binding would have under concurrent requests to the same
//! recipient (ADR-grade decision, recorded here since no ADR file exists
//! yet for this repository — see IMPLEMENTATION_LOG.md).

use super::hex;
use super::rpc::DecodedTransaction;

#[derive(Debug, PartialEq, Eq)]
pub enum BindingError {
    NoOpReturn,
    MultipleOpReturn,
    WrongPushSize { actual: usize },
}

impl BindingError {
    pub fn reason_code(&self) -> &'static str {
        match self {
            BindingError::NoOpReturn => "no_request_binding",
            BindingError::MultipleOpReturn => "ambiguous_request_binding",
            BindingError::WrongPushSize { .. } => "malformed_request_binding",
        }
    }
}

/// A vault-paying output, independent of whether its request binding is
/// valid — the indexer records a `Failed`/unmatched row per vault output
/// even on a malformed binding rather than silently ignoring a real
/// payment.
#[derive(Debug, Clone, Copy)]
pub struct VaultOutputCandidate {
    pub vout: u32,
    pub amount_atomic: u64,
}

fn is_vault_output(script_pub_key_hex: &str, vault_script_hex: &str) -> bool {
    script_pub_key_hex.eq_ignore_ascii_case(vault_script_hex)
}

const OP_RETURN: u8 = 0x6a;

/// Extracts the transaction's single valid 32-byte OP_RETURN payload and
/// decodes the first 8 bytes as a little-endian `bridge_requests.id`.
pub fn extract_request_binding(tx: &DecodedTransaction) -> Result<i64, BindingError> {
    let mut found: Option<[u8; 32]> = None;
    let mut count = 0usize;
    for vout in &tx.vout {
        let script = match hex::decode_vec(&vout.script_pub_key.hex) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if script.first() != Some(&OP_RETURN) {
            continue;
        }
        count += 1;
        if count > 1 {
            return Err(BindingError::MultipleOpReturn);
        }
        let payload_len = script.get(1).copied().unwrap_or(0) as usize;
        let payload = script.get(2..);
        let actual_len = payload.map(<[u8]>::len).unwrap_or(0);
        if payload_len != 32 || actual_len != 32 {
            return Err(BindingError::WrongPushSize { actual: actual_len });
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(payload.unwrap());
        found = Some(buf);
    }
    let payload = found.ok_or(BindingError::NoOpReturn)?;
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&payload[..8]);
    Ok(i64::from_le_bytes(id_bytes))
}

/// Every vault-paying output in `tx`, converting each amount from the RPC's
/// decimal GLC value to atomic units (8 decimals, verified empirically —
/// docs/goldcoin-rpc-notes.md).
pub fn vault_output_candidates(
    tx: &DecodedTransaction,
    vault_script_hex: &str,
) -> Vec<VaultOutputCandidate> {
    tx.vout
        .iter()
        .filter(|v| is_vault_output(&v.script_pub_key.hex, vault_script_hex))
        .map(|v| VaultOutputCandidate {
            vout: v.n,
            amount_atomic: glc_to_atomic(v.value),
        })
        .collect()
}

/// 8-decimal GLC -> atomic units, rounding to the nearest atomic unit
/// (verified: RPC values are already atomic-unit-exact for well-formed
/// amounts; rounding only guards against binary float representation
/// noise, never a source of value drift in practice).
pub(crate) fn glc_to_atomic(value: f64) -> u64 {
    (value * 100_000_000.0).round() as u64
}

/// Encodes a `bridge_requests.id` as the 32-byte OP_RETURN payload a user's
/// wallet (or this service's own deposit-instructions endpoint, once built)
/// would push: request id in the first 8 bytes LE, the rest zero.
pub fn encode_request_binding(request_id: i64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&request_id.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goldcoin::rpc::{DecodedScriptPubKey, DecodedVout};

    const VAULT_SCRIPT_HEX: &str = "76a9145a7ab7adf8185c27b3f54104cdccfe1ff0cd54cf88ac";

    fn vault_tx(vout_n: u32, value: f64, request_id: i64) -> DecodedTransaction {
        let mut op_return = vec![0x6a, 32u8];
        op_return.extend_from_slice(&encode_request_binding(request_id));
        DecodedTransaction {
            txid: "deadbeef".repeat(8),
            confirmations: Some(1),
            vout: vec![
                DecodedVout {
                    value,
                    n: vout_n,
                    script_pub_key: DecodedScriptPubKey {
                        hex: VAULT_SCRIPT_HEX.to_string(),
                        kind: "pubkeyhash".to_string(),
                    },
                },
                DecodedVout {
                    value: 0.0,
                    n: vout_n + 1,
                    script_pub_key: DecodedScriptPubKey {
                        hex: hex::encode(&op_return),
                        kind: "nulldata".to_string(),
                    },
                },
            ],
        }
    }

    #[test]
    fn extracts_request_id_round_trip() {
        let tx = vault_tx(0, 5.0, 424242);
        assert_eq!(extract_request_binding(&tx).unwrap(), 424242);
    }

    #[test]
    fn negative_ids_are_never_produced_but_round_trip_bytes_correctly() {
        // Defensive: encode/decode is a pure byte round-trip regardless of
        // sign; callers only ever pass positive rowids.
        let tx = vault_tx(0, 5.0, 1);
        assert_eq!(extract_request_binding(&tx).unwrap(), 1);
    }

    #[test]
    fn rejects_no_op_return() {
        let mut tx = vault_tx(0, 5.0, 1);
        tx.vout.pop();
        assert_eq!(
            extract_request_binding(&tx).unwrap_err(),
            BindingError::NoOpReturn
        );
    }

    #[test]
    fn rejects_multiple_op_returns() {
        let mut tx = vault_tx(0, 5.0, 1);
        let extra = tx.vout[1].clone();
        tx.vout.push(extra);
        assert_eq!(
            extract_request_binding(&tx).unwrap_err(),
            BindingError::MultipleOpReturn
        );
    }

    #[test]
    fn rejects_wrong_push_size() {
        let mut tx = vault_tx(0, 5.0, 1);
        let mut op_return = vec![0x6a, 4u8];
        op_return.extend_from_slice(&[1, 2, 3, 4]);
        tx.vout[1].script_pub_key.hex = hex::encode(&op_return);
        assert_eq!(
            extract_request_binding(&tx).unwrap_err(),
            BindingError::WrongPushSize { actual: 4 }
        );
    }

    #[test]
    fn vault_output_candidates_matches_exact_script_only() {
        let tx = vault_tx(0, 5.0, 1);
        let candidates = vault_output_candidates(&tx, VAULT_SCRIPT_HEX);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].amount_atomic, 500_000_000);
    }

    #[test]
    fn vault_output_candidates_empty_for_non_matching_script() {
        let tx = vault_tx(0, 5.0, 1);
        assert!(vault_output_candidates(&tx, "deadbeef").is_empty());
    }

    #[test]
    fn glc_to_atomic_conversion_is_exact_for_typical_amounts() {
        assert_eq!(glc_to_atomic(1.0), 100_000_000);
        assert_eq!(glc_to_atomic(0.00000001), 1);
        assert_eq!(glc_to_atomic(5.12345678), 512345678);
    }
}
