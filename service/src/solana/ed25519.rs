//! Client-side builder for the Solana ed25519-precompile instruction that
//! carries the internal attestation-key signatures
//! (docs/02-trust-model.md). This is the exact byte layout
//! `programs/glc-reserve-bridge/src/verification.rs` parses — the same
//! logic this workspace's Phase 2 litesvm tests already exercised
//! successfully from the on-chain side; this module is its off-chain
//! counterpart, written fresh here since this crate deliberately does not
//! depend on the on-chain program crate (docs/01-reuse-inventory.md owner
//! decision R1).
//!
//! Signing and proof assembly are deliberately separate functions —
//! [`sign`] produces a `(pubkey, signature)` pair from one signer's
//! keypair, [`build_attestation_proof`] assembles the final instruction
//! from already-produced pairs. This mirrors
//! `crate::goldcoin::multisig`'s `sign`/`assemble` split and, more
//! importantly, matches how production custody domains would actually work:
//! a signer process holds the key and never hands it (or the ability to
//! re-derive a signature from it) to whatever assembles the final
//! transaction.

use solana_sdk::ed25519_program;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};

/// `"This instruction"` sentinel Solana's precompile parser uses for
/// self-referential offsets — every entry in this builder's output points
/// into its own instruction data, matching what the on-chain parser
/// requires (`programs/glc-reserve-bridge/src/verification.rs`).
const SELF_REFERENCE: u16 = u16::MAX;

/// Produces one signer's `(pubkey, signature)` pair over `message`. In
/// production this call happens inside a signer's own custody-domain
/// process; here it's a plain function because [`crate::signing::attestation`]'s
/// dev/test signer holds its key in-process (see that module's docs).
pub fn sign(keypair: &Keypair, message: &[u8]) -> (Pubkey, Signature) {
    (keypair.pubkey(), keypair.sign_message(message))
}

/// Builds the ed25519-precompile instruction carrying one signature per
/// `signatures` entry, all claimed over the identical `message`, all
/// self-referential. Must be placed immediately before the instruction it
/// authorizes (e.g. `release_from_reserve`) in the same transaction.
/// Does not itself verify anything — that is the runtime precompile's job
/// before the following instruction executes, and
/// `crate::solana::verification`-equivalent parsing is the on-chain
/// program's job; this function only encodes.
pub fn build_attestation_proof(signatures: &[(Pubkey, Signature)], message: &[u8]) -> Instruction {
    let n = signatures.len();
    let entries_end = 2 + n * 14;
    let keys_end = entries_end + n * 32;
    let sigs_end = keys_end + n * 64;
    let msg_off = sigs_end;
    let mut data = vec![0u8; msg_off + message.len()];
    data[0] = n as u8;
    data[1] = 0;
    for (i, (pubkey, signature)) in signatures.iter().enumerate() {
        let base = 2 + i * 14;
        let pk_off = (entries_end + i * 32) as u16;
        let sig_off = (keys_end + i * 64) as u16;
        data[base..base + 2].copy_from_slice(&sig_off.to_le_bytes());
        data[base + 2..base + 4].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
        data[base + 4..base + 6].copy_from_slice(&pk_off.to_le_bytes());
        data[base + 6..base + 8].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
        data[base + 8..base + 10].copy_from_slice(&(msg_off as u16).to_le_bytes());
        data[base + 10..base + 12].copy_from_slice(&(message.len() as u16).to_le_bytes());
        data[base + 12..base + 14].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
        let pk_pos = entries_end + i * 32;
        data[pk_pos..pk_pos + 32].copy_from_slice(pubkey.as_ref());
        let sig_pos = keys_end + i * 64;
        data[sig_pos..sig_pos + 64].copy_from_slice(signature.as_ref());
    }
    data[msg_off..].copy_from_slice(message);
    Instruction {
        program_id: ed25519_program::id(),
        accounts: vec![],
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_two_signer_proof_with_self_referential_offsets() {
        let signers = [Keypair::new(), Keypair::new()];
        let message = b"test attestation message............";
        let pairs: Vec<(Pubkey, Signature)> = signers.iter().map(|k| sign(k, message)).collect();
        let ix = build_attestation_proof(&pairs, message);
        assert_eq!(ix.program_id, ed25519_program::id());
        assert_eq!(ix.data[0], 2, "num_signatures byte");
        assert_eq!(ix.data[1], 0, "padding byte must be zero");
        assert_eq!(&ix.data[ix.data.len() - message.len()..], message);
    }

    #[test]
    fn each_signature_verifies_against_its_own_pubkey_and_the_message() {
        let signers = [Keypair::new(), Keypair::new(), Keypair::new()];
        let message = b"another message, different length!";
        let pairs: Vec<(Pubkey, Signature)> = signers.iter().map(|k| sign(k, message)).collect();
        let ix = build_attestation_proof(&pairs, message);

        let n = pairs.len();
        let entries_end = 2 + n * 14;
        let keys_end = entries_end + n * 32;
        for (i, signer) in signers.iter().enumerate() {
            let pk_pos = entries_end + i * 32;
            let pk_bytes = &ix.data[pk_pos..pk_pos + 32];
            let sig_pos = keys_end + i * 64;
            let sig_bytes = &ix.data[sig_pos..sig_pos + 64];
            let pubkey = Pubkey::try_from(pk_bytes).unwrap();
            assert_eq!(pubkey, signer.pubkey());
            let sig = Signature::try_from(sig_bytes).unwrap();
            assert!(sig.verify(pubkey.as_ref(), message));
        }
    }

    #[test]
    fn a_signature_over_a_different_message_does_not_verify() {
        let kp = Keypair::new();
        let (pubkey, signature) = sign(&kp, b"message A");
        assert!(!signature.verify(pubkey.as_ref(), b"message B"));
    }
}
