//! P2SH `M`-of-`N` multisig vault: redeem script construction/parsing and
//! address derivation. Reused mechanics from the old bridge's
//! `withdrawal::vault` (docs/01-reuse-inventory.md) — the script/address
//! math is chain-mechanics, identical regardless of trust model.
//!
//! # This is internal threshold custody, not federation
//!
//! Per the approved trust model (docs/02-trust-model.md, docs/12-management-
//! decisions.md item 1): the `N` signer keys here are held in genuinely
//! separate internal custody domains operated by the single bridge
//! operator (HSM/KMS-backed in production), not by independent third-party
//! organizations. [`validation::MIN_THRESHOLD`]-style enforcement (a
//! threshold of 1 is refused) mirrors the Solana program's on-chain
//! `AttestationKeySet` guarantee: no single key may authorize a release.

use super::address::{base58check_encode, hash160, Network};
use super::hex;
use thiserror::Error;

/// Same headroom rationale as the Solana program's `MAX_ATTESTATION_KEYS`:
/// the approved model starts at 2-of-3; this bounds vault size without
/// implying anything federation-scale.
pub const MAX_VAULT_SIGNERS: usize = 8;
/// No single key may authorize a Goldcoin payout — mirrors
/// `programs/glc-reserve-bridge/src/validation.rs::MIN_THRESHOLD`.
pub const MIN_THRESHOLD: u8 = 2;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VaultError {
    #[error("vault must have at least one signer")]
    EmptySignerSet,
    #[error("vault exceeds MAX_VAULT_SIGNERS ({MAX_VAULT_SIGNERS})")]
    TooManySigners,
    #[error("threshold must be at least {MIN_THRESHOLD} (no single key may authorize a payout)")]
    ThresholdBelowMinimum,
    #[error("threshold {threshold} exceeds signer count {count}")]
    ThresholdExceedsSignerCount { threshold: u8, count: usize },
    #[error("pubkey at index {0} is not a valid 33-byte compressed SEC1 key")]
    InvalidPubkey(usize),
    #[error("duplicate pubkey in signer set")]
    DuplicatePubkey,
    #[error("malformed redeem script: {0}")]
    MalformedRedeemScript(String),
    #[error("derived address does not match: expected {expected}, got {actual}")]
    AddressMismatch { expected: String, actual: String },
    #[error("address error: {0}")]
    Address(#[from] super::address::AddressError),
}

const OP_CHECKMULTISIG: u8 = 0xae;
const PUBKEY_PUSH_LEN: u8 = 0x21; // 33

fn op_n(n: u8) -> u8 {
    0x50 + n
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultisigVault {
    pub signer_pubkeys: Vec<[u8; 33]>,
    pub threshold: u8,
    address: String,
    script_hash: [u8; 20],
}

impl MultisigVault {
    pub fn new(
        signer_pubkeys: Vec<[u8; 33]>,
        threshold: u8,
        network: Network,
    ) -> Result<Self, VaultError> {
        validate_signer_set(&signer_pubkeys, threshold)?;
        let redeem_script = build_redeem_script(&signer_pubkeys, threshold);
        let script_hash = hash160(&redeem_script);
        let address = base58check_encode(network.p2sh_version(), &script_hash);
        Ok(MultisigVault {
            signer_pubkeys,
            threshold,
            address,
            script_hash,
        })
    }

    pub fn redeem_script(&self) -> Vec<u8> {
        build_redeem_script(&self.signer_pubkeys, self.threshold)
    }

    pub fn redeem_script_hex(&self) -> String {
        hex::encode(&self.redeem_script())
    }

    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn script_hash160(&self) -> [u8; 20] {
        self.script_hash
    }

    /// `OP_HASH160 <20> <hash> OP_EQUAL` — the P2SH scriptPubKey for outputs
    /// paying into this vault (e.g. change).
    pub fn script_pubkey(&self) -> Vec<u8> {
        let mut s = vec![0xa9u8, 0x14];
        s.extend_from_slice(&self.script_hash);
        s.push(0x87);
        s
    }

    pub fn script_pubkey_hex(&self) -> String {
        hex::encode(&self.script_pubkey())
    }

    pub fn signer_count(&self) -> usize {
        self.signer_pubkeys.len()
    }

    /// The signer's position in the redeem script — this is simultaneously
    /// the vault-membership check and the ordering key `multisig.rs`'s
    /// scriptSig assembly needs (docs from the Phase 3 research: signature
    /// order in a scriptSig must match redeem-script pubkey order, NOT the
    /// order signers were asked in).
    pub fn signer_position(&self, pubkey: &[u8; 33]) -> Option<usize> {
        self.signer_pubkeys.iter().position(|p| p == pubkey)
    }

    /// Reconstructs a vault from a redeem script's hex encoding — used when
    /// the script comes from external input (e.g. a persisted record) and
    /// must be independently re-parsed and re-validated, never trusted as
    /// already-correct.
    pub fn from_redeem_script_hex(
        redeem_script_hex: &str,
        network: Network,
    ) -> Result<Self, VaultError> {
        let script = hex::decode_vec(redeem_script_hex)
            .map_err(|e| VaultError::MalformedRedeemScript(e.to_string()))?;
        if script.len() < 3 {
            return Err(VaultError::MalformedRedeemScript("too short".into()));
        }
        let m = script[0]
            .checked_sub(0x50)
            .ok_or_else(|| VaultError::MalformedRedeemScript("bad OP_M".into()))?;
        let mut pos = 1usize;
        let mut pubkeys = Vec::new();
        while pos < script.len() && script[pos] == PUBKEY_PUSH_LEN {
            let start = pos + 1;
            let end = start + 33;
            if end > script.len() {
                return Err(VaultError::MalformedRedeemScript(
                    "truncated pubkey push".into(),
                ));
            }
            let mut pk = [0u8; 33];
            pk.copy_from_slice(&script[start..end]);
            pubkeys.push(pk);
            pos = end;
        }
        if pos + 2 != script.len() {
            return Err(VaultError::MalformedRedeemScript(
                "unexpected trailer length".into(),
            ));
        }
        let n_byte = script[pos];
        let checkmultisig_byte = script[pos + 1];
        if checkmultisig_byte != OP_CHECKMULTISIG {
            return Err(VaultError::MalformedRedeemScript(
                "missing OP_CHECKMULTISIG".into(),
            ));
        }
        let n = n_byte
            .checked_sub(0x50)
            .ok_or_else(|| VaultError::MalformedRedeemScript("bad OP_N".into()))?;
        if n as usize != pubkeys.len() {
            return Err(VaultError::MalformedRedeemScript(format!(
                "OP_N={n} does not match {} pubkeys",
                pubkeys.len()
            )));
        }
        Self::new(pubkeys, m, network)
    }

    /// Re-derives the address from the redeem script and compares against
    /// `expected` — used so an operator typo in a configured address can
    /// never silently route funds somewhere nobody controls (reused
    /// discipline from the old bridge's `require_address`).
    pub fn require_address(&self, expected: &str) -> Result<(), VaultError> {
        if self.address != expected {
            return Err(VaultError::AddressMismatch {
                expected: expected.to_string(),
                actual: self.address.clone(),
            });
        }
        Ok(())
    }

    /// Validates a proposed quorum (the specific `threshold`-sized subset of
    /// signers being asked to sign a given payout): strictly ascending,
    /// unique, in range. Ascending order gives a given signer set exactly
    /// one canonical encoding — otherwise the same set could produce
    /// multiple different intent commitments for the same actual quorum.
    pub fn validate_quorum(&self, indices: &[usize]) -> Result<(), VaultError> {
        if indices.len() != self.threshold as usize {
            return Err(VaultError::MalformedRedeemScript(format!(
                "quorum has {} indices, need exactly {}",
                indices.len(),
                self.threshold
            )));
        }
        for w in indices.windows(2) {
            if w[0] >= w[1] {
                return Err(VaultError::MalformedRedeemScript(
                    "quorum indices must be strictly ascending".into(),
                ));
            }
        }
        if let Some(&last) = indices.last() {
            if last >= self.signer_pubkeys.len() {
                return Err(VaultError::MalformedRedeemScript(
                    "quorum index out of range".into(),
                ));
            }
        }
        Ok(())
    }
}

fn validate_signer_set(pubkeys: &[[u8; 33]], threshold: u8) -> Result<(), VaultError> {
    if pubkeys.is_empty() {
        return Err(VaultError::EmptySignerSet);
    }
    if pubkeys.len() > MAX_VAULT_SIGNERS {
        return Err(VaultError::TooManySigners);
    }
    if threshold < MIN_THRESHOLD {
        return Err(VaultError::ThresholdBelowMinimum);
    }
    if threshold as usize > pubkeys.len() {
        return Err(VaultError::ThresholdExceedsSignerCount {
            threshold,
            count: pubkeys.len(),
        });
    }
    for (i, pk) in pubkeys.iter().enumerate() {
        if pk[0] != 0x02 && pk[0] != 0x03 {
            return Err(VaultError::InvalidPubkey(i));
        }
    }
    for i in 0..pubkeys.len() {
        for j in (i + 1)..pubkeys.len() {
            if pubkeys[i] == pubkeys[j] {
                return Err(VaultError::DuplicatePubkey);
            }
        }
    }
    Ok(())
}

fn build_redeem_script(pubkeys: &[[u8; 33]], threshold: u8) -> Vec<u8> {
    let mut script = vec![op_n(threshold)];
    for pk in pubkeys {
        script.push(PUBKEY_PUSH_LEN);
        script.extend_from_slice(pk);
    }
    script.push(op_n(pubkeys.len() as u8));
    script.push(OP_CHECKMULTISIG);
    script
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden vector: a real 2-of-3 redeem script and its address, taken
    /// from the old bridge's `createmultisig`-verified test fixture
    /// (docs/01-reuse-inventory.md — reused as-is since it's real,
    /// node-verified data, not something to re-derive).
    const REAL_REDEEM_SCRIPT: &str = "5221028e7147e643d67093dc8ca6a8fb888f1a452dddc62de991c7ed72080d65a421e42102f1c88ca7176c3ffee952ee6fae697991b257b6d53c3bc88e81cfe99adbcdbee5210256220bb7865197a40c4590ac80f12ef18e9063eac2eff92c4476ec27034042f953ae";
    const REAL_ADDRESS: &str = "QY9YcpypWD91BEZ37TjNHYoqrquhcnVBYV";

    #[test]
    fn golden_vector_parses_and_re_derives_the_real_address() {
        let v =
            MultisigVault::from_redeem_script_hex(REAL_REDEEM_SCRIPT, Network::Testnet).unwrap();
        assert_eq!(
            v.address(),
            REAL_ADDRESS,
            "must match the real node-verified createmultisig address"
        );
        assert_eq!(v.threshold, 2);
        assert_eq!(v.signer_count(), 3);
        assert_eq!(
            v.redeem_script_hex(),
            REAL_REDEEM_SCRIPT,
            "parse -> rebuild must be an exact inverse"
        );
    }

    /// Golden vector: a real 2-of-3 redeem script and its mainnet P2SH
    /// address, taken from a real, isolated, network-disabled `goldcoind`
    /// mainnet `createmultisig` call (docs/16-p0-checkpoint.md) — not
    /// fabricated or guessed.
    const REAL_MAINNET_REDEEM_SCRIPT: &str = "5221027ebd15873d15a158260514bc3a225fc4c94d03b1598353234970118d2600c54721026f158462396baeab4cf9bc430b379a338d214ce5cbff56099a4943fefc14df0c210203ee5ca7afe9f6292b64d831ac77cb3169aae214eb22ef2450052fba9460b04453ae";
    const REAL_MAINNET_ADDRESS: &str = "MCQp94i1bMnZeqdLg1Y53FqWtLcz1Q1BkY";

    #[test]
    fn mainnet_golden_vector_parses_and_re_derives_the_real_address() {
        let v = MultisigVault::from_redeem_script_hex(REAL_MAINNET_REDEEM_SCRIPT, Network::Mainnet)
            .unwrap();
        assert_eq!(
            v.address(),
            REAL_MAINNET_ADDRESS,
            "must match the real node-verified mainnet createmultisig address"
        );
        assert_eq!(v.threshold, 2);
        assert_eq!(v.signer_count(), 3);
        assert_eq!(v.redeem_script_hex(), REAL_MAINNET_REDEEM_SCRIPT);
        // The same redeem script must never resolve to the testnet
        // address, or vice versa — the network genuinely changes the
        // result, not just cosmetically.
        assert_ne!(v.address(), REAL_ADDRESS);
    }

    #[test]
    fn require_address_accepts_the_real_vector_and_rejects_a_typo() {
        let v =
            MultisigVault::from_redeem_script_hex(REAL_REDEEM_SCRIPT, Network::Testnet).unwrap();
        v.require_address(REAL_ADDRESS).unwrap();
        assert!(v
            .require_address("QwrongAddressWouldRouteFundsAway")
            .is_err());
    }

    fn fake_pubkey(tag: u8, prefix: u8) -> [u8; 33] {
        let mut pk = [tag; 33];
        pk[0] = prefix;
        pk
    }

    #[test]
    fn accepts_the_approved_two_of_three() {
        let keys = vec![
            fake_pubkey(1, 0x02),
            fake_pubkey(2, 0x03),
            fake_pubkey(3, 0x02),
        ];
        assert!(MultisigVault::new(keys, 2, Network::Testnet).is_ok());
    }

    #[test]
    fn rejects_single_key_threshold() {
        let keys = vec![
            fake_pubkey(1, 0x02),
            fake_pubkey(2, 0x03),
            fake_pubkey(3, 0x02),
        ];
        assert_eq!(
            MultisigVault::new(keys, 1, Network::Testnet).unwrap_err(),
            VaultError::ThresholdBelowMinimum
        );
    }

    #[test]
    fn rejects_duplicate_pubkeys() {
        let pk = fake_pubkey(1, 0x02);
        assert_eq!(
            MultisigVault::new(vec![pk, pk, fake_pubkey(2, 0x03)], 2, Network::Testnet)
                .unwrap_err(),
            VaultError::DuplicatePubkey
        );
    }

    #[test]
    fn rejects_uncompressed_style_prefix() {
        let keys = vec![
            fake_pubkey(1, 0x04),
            fake_pubkey(2, 0x03),
            fake_pubkey(3, 0x02),
        ];
        assert_eq!(
            MultisigVault::new(keys, 2, Network::Testnet).unwrap_err(),
            VaultError::InvalidPubkey(0)
        );
    }

    #[test]
    fn rejects_threshold_exceeding_signer_count() {
        let keys = vec![fake_pubkey(1, 0x02), fake_pubkey(2, 0x03)];
        assert_eq!(
            MultisigVault::new(keys, 3, Network::Testnet).unwrap_err(),
            VaultError::ThresholdExceedsSignerCount {
                threshold: 3,
                count: 2
            }
        );
    }

    #[test]
    fn signer_position_matches_redeem_script_order() {
        let v =
            MultisigVault::from_redeem_script_hex(REAL_REDEEM_SCRIPT, Network::Testnet).unwrap();
        assert_eq!(v.signer_position(&v.signer_pubkeys[1]), Some(1));
        assert_eq!(v.signer_position(&fake_pubkey(99, 0x02)), None);
    }

    #[test]
    fn validate_quorum_requires_ascending_unique_in_range() {
        let v =
            MultisigVault::from_redeem_script_hex(REAL_REDEEM_SCRIPT, Network::Testnet).unwrap(); // 2-of-3
        assert!(v.validate_quorum(&[0, 1]).is_ok());
        assert!(
            v.validate_quorum(&[1, 0]).is_err(),
            "descending must be rejected"
        );
        assert!(
            v.validate_quorum(&[0, 0]).is_err(),
            "duplicate must be rejected"
        );
        assert!(
            v.validate_quorum(&[0, 5]).is_err(),
            "out of range must be rejected"
        );
        assert!(
            v.validate_quorum(&[0]).is_err(),
            "wrong length must be rejected"
        );
    }

    #[test]
    fn malformed_redeem_script_is_rejected_not_panicked_on() {
        assert!(MultisigVault::from_redeem_script_hex("00", Network::Testnet).is_err());
        assert!(MultisigVault::from_redeem_script_hex("zz", Network::Testnet).is_err());
        assert!(MultisigVault::from_redeem_script_hex("", Network::Testnet).is_err());
    }

    #[test]
    fn script_pubkey_wraps_hash_with_op_hash160_op_equal() {
        let v =
            MultisigVault::from_redeem_script_hex(REAL_REDEEM_SCRIPT, Network::Testnet).unwrap();
        let spk = v.script_pubkey();
        assert_eq!(spk[0], 0xa9);
        assert_eq!(spk[1], 0x14);
        assert_eq!(spk.len(), 23);
        assert_eq!(spk[22], 0x87);
    }
}
