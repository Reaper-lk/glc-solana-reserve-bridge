//! Partial-signature verification and scriptSig assembly for a P2SH
//! multisig spend. Reused algorithm from the old bridge's
//! `withdrawal::multisig` (docs/01-reuse-inventory.md) — Goldcoin has no
//! PSBT/`combinerawtransaction` (verified against a real node by the old
//! bridge's engineering), so partial signatures collected out-of-band must
//! be spliced into the final scriptSig by hand.
//!
//! # Signature order is consensus-critical
//!
//! `OP_CHECKMULTISIG` requires signatures to appear in the scriptSig in the
//! same relative order as their pubkeys in the redeem script — NOT the
//! order signers were asked in, or the order partials arrived. [`assemble`]
//! always re-sorts by each signer's [`MultisigVault::signer_position`]
//! before splicing, regardless of input order, specifically because a past
//! version of this logic (in the donor codebase) got this wrong and it was
//! only caught by real-node broadcast rejection
//! (`mandatory-script-verify-flag-failed`) — see IMPLEMENTATION_LOG.md.

use thiserror::Error;

use super::tx::push_data;
use super::vault::MultisigVault;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MultisigError {
    #[error("pubkey is not a member of this vault")]
    SignerNotInVault,
    #[error("the same signer position contributed more than once")]
    DuplicateSigner,
    #[error("only {have} of the required {threshold} signatures were supplied")]
    ThresholdNotMet { have: usize, threshold: usize },
    #[error("{have} signatures supplied, exceeding the vault's {threshold}-signature requirement")]
    TooManySignatures { have: usize, threshold: usize },
    #[error("signature does not verify against the claimed pubkey and sighash")]
    InvalidSignature,
}

const SIGHASH_ALL_BYTE: u8 = 0x01;

/// One signer's contribution: a DER-encoded ECDSA signature (no trailing
/// sighash-type byte — [`assemble`] appends it) over a specific input's
/// legacy sighash, plus the pubkey identifying which vault signer produced
/// it.
#[derive(Debug, Clone)]
pub struct PartialSignature {
    pub vault_pubkey: [u8; 33],
    pub der_signature: Vec<u8>,
}

/// Verifies a DER-encoded ECDSA signature against `pubkey` and `sighash`.
/// Every partial is verified before being placed — a valid signature is
/// self-authenticating (nothing else needs to vouch for it, since only the
/// key holder could have produced it), and an invalid one must never be
/// silently spliced into a transaction that will then fail broadcast.
pub fn verify_partial(pubkey: &[u8; 33], sighash: &[u8; 32], der_signature: &[u8]) -> bool {
    let Ok(pk) = libsecp256k1::PublicKey::parse_compressed(pubkey) else {
        return false;
    };
    let Ok(sig) = libsecp256k1::Signature::parse_der(der_signature) else {
        return false;
    };
    let msg = libsecp256k1::Message::parse(sighash);
    libsecp256k1::verify(&msg, &sig, &pk)
}

/// Assembles the final scriptSig for input `input_index` from `partials`:
/// `OP_0 <sig_1 push> ... <sig_M push> <redeem_script push>`. `OP_0` is the
/// mandatory dummy element compensating for `OP_CHECKMULTISIG`'s
/// well-known off-by-one stack-consumption bug — omitting it makes the
/// spend invalid, not just nonstandard.
pub fn assemble(
    vault: &MultisigVault,
    sighash: &[u8; 32],
    partials: &[PartialSignature],
) -> Result<Vec<u8>, MultisigError> {
    let mut placed: Vec<(usize, &PartialSignature)> = Vec::with_capacity(partials.len());
    let mut seen_positions = std::collections::HashSet::new();
    for partial in partials {
        let position = vault
            .signer_position(&partial.vault_pubkey)
            .ok_or(MultisigError::SignerNotInVault)?;
        if !seen_positions.insert(position) {
            return Err(MultisigError::DuplicateSigner);
        }
        if !verify_partial(&partial.vault_pubkey, sighash, &partial.der_signature) {
            return Err(MultisigError::InvalidSignature);
        }
        placed.push((position, partial));
    }

    let threshold = vault.threshold as usize;
    if placed.len() < threshold {
        return Err(MultisigError::ThresholdNotMet {
            have: placed.len(),
            threshold,
        });
    }
    if placed.len() > threshold {
        return Err(MultisigError::TooManySignatures {
            have: placed.len(),
            threshold,
        });
    }

    // The consensus-critical step: sort by redeem-script pubkey position,
    // never by collection/argument order.
    placed.sort_by_key(|(pos, _)| *pos);

    let mut script_sig = vec![0x00u8]; // OP_0 dummy
    for (_, partial) in &placed {
        let mut sig_with_type = partial.der_signature.clone();
        sig_with_type.push(SIGHASH_ALL_BYTE);
        push_data(&mut script_sig, &sig_with_type);
    }
    push_data(&mut script_sig, &vault.redeem_script());
    Ok(script_sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goldcoin::vault::MultisigVault;

    fn keypair(seed: u8) -> (libsecp256k1::SecretKey, [u8; 33]) {
        let mut bytes = [0u8; 32];
        bytes[31] = seed;
        bytes[0] = 1; // avoid the all-zero invalid secret key
        let sk = libsecp256k1::SecretKey::parse(&bytes).unwrap();
        let pk = libsecp256k1::PublicKey::from_secret_key(&sk);
        (sk, pk.serialize_compressed())
    }

    fn sign(sk: &libsecp256k1::SecretKey, sighash: &[u8; 32]) -> Vec<u8> {
        let msg = libsecp256k1::Message::parse(sighash);
        let (sig, _) = libsecp256k1::sign(&msg, sk);
        sig.serialize_der().as_ref().to_vec()
    }

    fn two_of_three() -> (MultisigVault, [(libsecp256k1::SecretKey, [u8; 33]); 3]) {
        let signers = [keypair(1), keypair(2), keypair(3)];
        let vault = MultisigVault::new(signers.iter().map(|(_, pk)| *pk).collect(), 2).unwrap();
        (vault, signers)
    }

    #[test]
    fn verify_partial_accepts_a_genuine_signature_and_rejects_a_wrong_pubkey() {
        let (_, signers) = two_of_three();
        let sighash = [0x42u8; 32];
        let sig = sign(&signers[0].0, &sighash);
        assert!(verify_partial(&signers[0].1, &sighash, &sig));
        assert!(
            !verify_partial(&signers[1].1, &sighash, &sig),
            "signature must not verify against a different signer's pubkey"
        );
    }

    #[test]
    fn assemble_orders_signatures_by_redeem_script_position_not_collection_order() {
        let (vault, signers) = two_of_three();
        let sighash = [0x11u8; 32];
        // Contribute in REVERSE redeem-script order (signer 1 then signer 0)
        // to prove assemble() reorders rather than trusting input order.
        let p1 = PartialSignature {
            vault_pubkey: signers[1].1,
            der_signature: sign(&signers[1].0, &sighash),
        };
        let p0 = PartialSignature {
            vault_pubkey: signers[0].1,
            der_signature: sign(&signers[0].0, &sighash),
        };
        let script_sig = assemble(&vault, &sighash, &[p1.clone(), p0.clone()]).unwrap();

        // Reassembling with arguments already in forward order must
        // produce byte-IDENTICAL scriptSig — proving order-of-collection
        // has no effect on the result.
        let script_sig_forward = assemble(&vault, &sighash, &[p0, p1]).unwrap();
        assert_eq!(script_sig, script_sig_forward);

        assert_eq!(
            script_sig[0], 0x00,
            "must start with the OP_0 CHECKMULTISIG dummy"
        );
    }

    #[test]
    fn assemble_rejects_below_threshold() {
        let (vault, signers) = two_of_three();
        let sighash = [0x22u8; 32];
        let p0 = PartialSignature {
            vault_pubkey: signers[0].1,
            der_signature: sign(&signers[0].0, &sighash),
        };
        assert_eq!(
            assemble(&vault, &sighash, &[p0]).unwrap_err(),
            MultisigError::ThresholdNotMet {
                have: 1,
                threshold: 2
            }
        );
    }

    #[test]
    fn assemble_rejects_above_threshold() {
        let (vault, signers) = two_of_three();
        let sighash = [0x33u8; 32];
        let all: Vec<_> = signers
            .iter()
            .map(|(sk, pk)| PartialSignature {
                vault_pubkey: *pk,
                der_signature: sign(sk, &sighash),
            })
            .collect();
        assert_eq!(
            assemble(&vault, &sighash, &all).unwrap_err(),
            MultisigError::TooManySignatures {
                have: 3,
                threshold: 2
            }
        );
    }

    #[test]
    fn assemble_rejects_duplicate_signer_contribution() {
        let (vault, signers) = two_of_three();
        let sighash = [0x44u8; 32];
        let sig = sign(&signers[0].0, &sighash);
        let p = PartialSignature {
            vault_pubkey: signers[0].1,
            der_signature: sig,
        };
        assert_eq!(
            assemble(&vault, &sighash, &[p.clone(), p]).unwrap_err(),
            MultisigError::DuplicateSigner
        );
    }

    #[test]
    fn assemble_rejects_a_signer_not_in_the_vault() {
        let (vault, _) = two_of_three();
        let (outsider_sk, outsider_pk) = keypair(99);
        let sighash = [0x55u8; 32];
        let p = PartialSignature {
            vault_pubkey: outsider_pk,
            der_signature: sign(&outsider_sk, &sighash),
        };
        assert_eq!(
            assemble(&vault, &sighash, &[p]).unwrap_err(),
            MultisigError::SignerNotInVault
        );
    }

    #[test]
    fn assemble_rejects_an_invalid_signature_even_from_a_real_vault_member() {
        let (vault, signers) = two_of_three();
        let sighash = [0x66u8; 32];
        let wrong_sighash = [0x77u8; 32];
        // Signed over the WRONG sighash — a mismatched signature must never
        // be accepted just because the pubkey is legitimate.
        let bad_sig = sign(&signers[0].0, &wrong_sighash);
        let p = PartialSignature {
            vault_pubkey: signers[0].1,
            der_signature: bad_sig,
        };
        let p2 = PartialSignature {
            vault_pubkey: signers[1].1,
            der_signature: sign(&signers[1].0, &sighash),
        };
        assert_eq!(
            assemble(&vault, &sighash, &[p, p2]).unwrap_err(),
            MultisigError::InvalidSignature
        );
    }
}
