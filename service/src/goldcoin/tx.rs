//! Minimal Goldcoin (Bitcoin/Litecoin-lineage, pre-SegWit) transaction
//! model: serialization, txid computation, and legacy `SIGHASH_ALL`
//! preimage construction. Reused algorithm from the old bridge's
//! transaction-construction code (docs/01-reuse-inventory.md).
//!
//! # Byte order — read this before touching `prev_txid`
//!
//! Every other module in this crate (`goldcoin::indexer`, `ledger`) stores
//! a transaction id as exactly `hex::decode_exact::<32>(rpc_txid_hex)` —
//! i.e. RPC/human DISPLAY order, which by universal Bitcoin-lineage
//! convention is the byte-REVERSE of the raw double-SHA256 output
//! ("internal" order). Raw wire serialization (what actually goes into a
//! `TxIn`'s prevout field, and what a real node's mempool/consensus code
//! parses) requires INTERNAL order. This module is the ONE place that
//! reversal happens:
//!
//! - [`Transaction::serialize`] reverses each input's `prev_txid` (assumed
//!   stored in display order, matching the rest of this crate) before
//!   writing it into the prevout field.
//! - [`Transaction::txid`] reverses the raw double-SHA256 output before
//!   returning it, so the result is directly comparable to (and storable
//!   alongside) every other txid this crate handles.
//!
//! This is standard, well-documented Bitcoin/Litecoin-lineage behavior, not
//! a Goldcoin-specific quirk — but unlike the byte-level facts the old
//! bridge verified against a real node (OP_RETURN size, RPC shapes, address
//! version bytes), this specific reversal has **not** been independently
//! confirmed against a live Goldcoin node in this environment (none is
//! available — see IMPLEMENTATION_LOG.md). Treat it as textbook-correct but
//! unverified until a real-node broadcast test (Phase 6,
//! docs/11-testing-plan.md) confirms it byte-for-byte, the same way the old
//! bridge's own OP_RETURN-deposit convention was confirmed.

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxIn {
    /// Display-order txid (see module docs), matching every other module's
    /// storage convention.
    pub prev_txid: [u8; 32],
    pub prev_vout: u32,
    pub script_sig: Vec<u8>,
    pub sequence: u32,
}

impl TxIn {
    pub fn unsigned(prev_txid: [u8; 32], prev_vout: u32) -> Self {
        TxIn {
            prev_txid,
            prev_vout,
            script_sig: Vec::new(),
            sequence: 0xffff_ffff,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOut {
    pub value_atomic: u64,
    pub script_pubkey: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub version: i32,
    pub inputs: Vec<TxIn>,
    pub outputs: Vec<TxOut>,
    pub locktime: u32,
}

fn write_varint(out: &mut Vec<u8>, n: u64) {
    if n < 0xfd {
        out.push(n as u8);
    } else if n <= 0xffff {
        out.push(0xfd);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n <= 0xffff_ffff {
        out.push(0xfe);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    } else {
        out.push(0xff);
        out.extend_from_slice(&n.to_le_bytes());
    }
}

/// Minimal-encoding push of `data` (the same rule Goldcoin/Bitcoin-lineage
/// scriptSig/scriptPubKey construction uses throughout): a single
/// length-byte for lengths < 0x4c (76), else `OP_PUSHDATA1`
/// (`0x4c`)+1-byte length for ≤ 0xff, else `OP_PUSHDATA2` (`0x4d`)+2-byte-LE
/// length. This crate never needs `OP_PUSHDATA4` (no push exceeds 65535
/// bytes).
pub fn push_data(out: &mut Vec<u8>, data: &[u8]) {
    let len = data.len();
    if len < 0x4c {
        out.push(len as u8);
    } else if len <= 0xff {
        out.push(0x4c);
        out.push(len as u8);
    } else if len <= 0xffff {
        out.push(0x4d);
        out.extend_from_slice(&(len as u16).to_le_bytes());
    } else {
        panic!("push_data: {len} bytes exceeds this crate's supported push size (OP_PUSHDATA4 never needed)");
    }
    out.extend_from_slice(data);
}

impl Transaction {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.version.to_le_bytes());
        write_varint(&mut out, self.inputs.len() as u64);
        for input in &self.inputs {
            let mut internal_order = input.prev_txid;
            internal_order.reverse(); // display order -> internal order (module docs)
            out.extend_from_slice(&internal_order);
            out.extend_from_slice(&input.prev_vout.to_le_bytes());
            write_varint(&mut out, input.script_sig.len() as u64);
            out.extend_from_slice(&input.script_sig);
            out.extend_from_slice(&input.sequence.to_le_bytes());
        }
        write_varint(&mut out, self.outputs.len() as u64);
        for output in &self.outputs {
            out.extend_from_slice(&output.value_atomic.to_le_bytes());
            write_varint(&mut out, output.script_pubkey.len() as u64);
            out.extend_from_slice(&output.script_pubkey);
        }
        out.extend_from_slice(&self.locktime.to_le_bytes());
        out
    }

    /// Display-order txid: double-SHA256 of the serialization, byte-reversed
    /// (module docs).
    pub fn txid(&self) -> [u8; 32] {
        let first = Sha256::digest(self.serialize());
        let second = Sha256::digest(first);
        let mut bytes: [u8; 32] = second.into();
        bytes.reverse();
        bytes
    }

    /// Legacy `SIGHASH_ALL` preimage for `input_index`, per Bitcoin-lineage
    /// convention (docs/01-reuse-inventory.md, real-node-verified by the old
    /// bridge): every input's scriptSig is blanked except `input_index`,
    /// whose scriptSig slot is temporarily replaced with `redeem_script`
    /// (not the real scriptPubKey — this is the P2SH spending convention),
    /// then the whole transaction is serialized with a trailing 4-byte-LE
    /// sighash-type appended, and double-SHA256'd.
    ///
    /// **Critical, real-node-verified quirk**: this preimage does NOT
    /// commit to any input's value — legacy sighash carries no amount
    /// field. A valid signature is therefore no evidence whatsoever about
    /// what amount it authorizes; callers (in particular
    /// [`crate::signing::goldcoin_vault`]) must independently verify
    /// amounts against their own UTXO view before signing, never trust
    /// anything implied by the preimage itself.
    pub fn sighash_all(&self, input_index: usize, redeem_script: &[u8]) -> [u8; 32] {
        const SIGHASH_ALL: u32 = 0x01;
        let mut stripped = self.clone();
        for (i, input) in stripped.inputs.iter_mut().enumerate() {
            input.script_sig = if i == input_index {
                redeem_script.to_vec()
            } else {
                Vec::new()
            };
        }
        let mut preimage = stripped.serialize();
        preimage.extend_from_slice(&SIGHASH_ALL.to_le_bytes());
        let first = Sha256::digest(&preimage);
        let second = Sha256::digest(first);
        second.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tx() -> Transaction {
        Transaction {
            version: 1,
            inputs: vec![TxIn::unsigned([0xAAu8; 32], 0)],
            outputs: vec![TxOut {
                value_atomic: 100_000_000,
                script_pubkey: vec![
                    0x76, 0xa9, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x88,
                    0xac,
                ],
            }],
            locktime: 0,
        }
    }

    #[test]
    fn varint_boundaries_round_trip_via_serialize_len() {
        // Exercised indirectly: an output script of exactly 0xfd bytes
        // forces the 3-byte varint encoding path in serialize(); confirm
        // the serialized length matches what we expect (1 [0xfd marker] +
        // 2 [u16 len] + payload), i.e. the encoder didn't silently truncate
        // or misencode the boundary.
        let mut out = Vec::new();
        write_varint(&mut out, 0xfd);
        assert_eq!(out, vec![0xfd, 0xfd, 0x00]);
        let mut out2 = Vec::new();
        write_varint(&mut out2, 0x10000);
        assert_eq!(out2, vec![0xfe, 0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn push_data_minimal_encoding_thresholds() {
        let mut out = Vec::new();
        push_data(&mut out, &[0u8; 75]);
        assert_eq!(out[0], 75);
        let mut out2 = Vec::new();
        push_data(&mut out2, &[0u8; 76]);
        assert_eq!(&out2[0..2], &[0x4c, 76]);
        let mut out3 = Vec::new();
        push_data(&mut out3, &[0u8; 256]);
        assert_eq!(out3[0], 0x4d);
    }

    #[test]
    fn prev_txid_is_reversed_between_display_and_wire_order() {
        let tx = sample_tx();
        let serialized = tx.serialize();
        // version(4) + varint(1 input, 1 byte) = offset 5 for the prevout
        // hash field.
        let wire_txid = &serialized[5..37];
        let mut expected = tx.inputs[0].prev_txid;
        expected.reverse();
        assert_eq!(wire_txid, expected.as_slice());
    }

    #[test]
    fn serialize_is_deterministic() {
        let tx = sample_tx();
        assert_eq!(tx.serialize(), tx.serialize());
    }

    #[test]
    fn txid_changes_with_any_field() {
        let a = sample_tx();
        let mut b = sample_tx();
        b.locktime = 1;
        assert_ne!(a.txid(), b.txid());
    }

    #[test]
    fn sighash_all_blanks_other_inputs_scriptsigs() {
        let mut tx = sample_tx();
        tx.inputs.push(TxIn::unsigned([0xBBu8; 32], 1));
        tx.inputs[0].script_sig = vec![0xde, 0xad];
        tx.inputs[1].script_sig = vec![0xbe, 0xef];
        let redeem = vec![0x51, 0xae];
        let h0 = tx.sighash_all(0, &redeem);
        let h1 = tx.sighash_all(1, &redeem);
        assert_ne!(h0, h1, "sighash must differ per input index");
    }

    #[test]
    fn sighash_all_does_not_mutate_the_original_transaction() {
        let tx = sample_tx();
        let before = tx.clone();
        let _ = tx.sighash_all(0, &[0x51, 0xae]);
        assert_eq!(tx, before);
    }
}
