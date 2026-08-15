//! Strict parser for the ed25519-precompile instruction that carries the
//! internal attestation-key signatures (docs/02-trust-model.md).
//!
//! Reused near-verbatim from the old bridge's `verification.rs`
//! (docs/01-reuse-inventory.md classifies this pure mechanism as
//! authority-agnostic and directly reusable): only the terminology and
//! error variant names changed, from "validator" to "attestation signer".
//! The mechanism itself does not care whether the key set behind it is a
//! federation or an internal threshold-custody group — it is byte-parsing.
//!
//! Trust model: the runtime's ed25519 precompile has ALREADY verified every
//! signature in the instruction before this program executes — an invalid
//! signature aborts the whole transaction. What the precompile does NOT
//! guarantee is *what* was signed by *whom relevant to us*; that is this
//! module's job, and it is deliberately paranoid:
//!
//! - every offset is bounds-checked with checked arithmetic;
//! - every entry must be fully self-referential (`u16::MAX` instruction
//!   indices) — entries may never point into other instructions' data,
//!   closing the classic introspection confusion attacks;
//! - every entry's message must be byte-identical to the expected canonical
//!   claim message (constraint 3: never release based only on a
//!   requester's claim — the requester cannot forge what a signer signed);
//! - signers must be current attestation keys, and duplicates are hard
//!   errors (a compromised or malfunctioning single key cannot count
//!   twice toward the threshold).

use anchor_lang::prelude::*;

use crate::constants::MAX_ATTESTATION_KEYS;
use crate::errors::BridgeError;

const ED25519_HEADER_LEN: usize = 2;
const ED25519_ENTRY_LEN: usize = 14;
const SIGNATURE_LEN: usize = 64;
const PUBKEY_LEN: usize = 32;
const SELF_REFERENCE: u16 = u16::MAX;

// The dedup bitmask below is a u16; a larger key cap would overflow it.
const _: () = assert!(MAX_ATTESTATION_KEYS <= 16);

fn u16_at(data: &[u8], offset: usize) -> Result<u16> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or(BridgeError::MalformedSignatureVerification)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// Parses the ed25519 instruction's data and returns the number of UNIQUE
/// current attestation keys that signed `expected_message`.
///
/// Errors rather than skips on anything unexpected: malformed layout,
/// out-of-bounds offsets, cross-instruction references, a message that is
/// not byte-identical to `expected_message`, a signer that is not a current
/// attestation key, or the same key appearing twice.
pub fn count_unique_attestation_signers(
    data: &[u8],
    expected_message: &[u8],
    keys: &[Pubkey],
) -> Result<usize> {
    require!(
        data.len() >= ED25519_HEADER_LEN,
        BridgeError::MalformedSignatureVerification
    );
    let num_signatures = data[0] as usize;
    require!(
        num_signatures >= 1 && data[1] == 0,
        BridgeError::MalformedSignatureVerification
    );

    let mut seen: u16 = 0;
    let mut count: usize = 0;

    for i in 0..num_signatures {
        let entry_base = ED25519_HEADER_LEN
            .checked_add(
                i.checked_mul(ED25519_ENTRY_LEN)
                    .ok_or(BridgeError::MalformedSignatureVerification)?,
            )
            .ok_or(BridgeError::MalformedSignatureVerification)?;

        let signature_offset = u16_at(data, entry_base)? as usize;
        let signature_ix_index = u16_at(data, entry_base + 2)?;
        let public_key_offset = u16_at(data, entry_base + 4)? as usize;
        let public_key_ix_index = u16_at(data, entry_base + 6)?;
        let message_offset = u16_at(data, entry_base + 8)? as usize;
        let message_size = u16_at(data, entry_base + 10)? as usize;
        let message_ix_index = u16_at(data, entry_base + 12)?;

        require!(
            signature_ix_index == SELF_REFERENCE
                && public_key_ix_index == SELF_REFERENCE
                && message_ix_index == SELF_REFERENCE,
            BridgeError::MalformedSignatureVerification
        );

        let sig_end = signature_offset
            .checked_add(SIGNATURE_LEN)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
        require!(
            data.get(signature_offset..sig_end).is_some(),
            BridgeError::MalformedSignatureVerification
        );

        let msg_end = message_offset
            .checked_add(message_size)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
        let message = data
            .get(message_offset..msg_end)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
        require!(
            message == expected_message,
            BridgeError::SignatureMessageMismatch
        );

        let pk_end = public_key_offset
            .checked_add(PUBKEY_LEN)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
        let pk_bytes = data
            .get(public_key_offset..pk_end)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
        let signer =
            Pubkey::try_from(pk_bytes).map_err(|_| BridgeError::MalformedSignatureVerification)?;
        let key_index = keys
            .iter()
            .position(|k| *k == signer)
            .ok_or(BridgeError::UnknownAttestationSigner)?;

        let bit = 1u16
            .checked_shl(key_index as u32)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
        require!(seen & bit == 0, BridgeError::DuplicateAttestationSignature);
        seen |= bit;
        count = count
            .checked_add(1)
            .ok_or(BridgeError::MalformedSignatureVerification)?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &[u8] = b"canonical message bytes for unit tests only....";

    fn keys(n: usize) -> Vec<Pubkey> {
        (0..n).map(|_| Pubkey::new_unique()).collect()
    }

    fn build(entries: &[Pubkey], message: &[u8]) -> Vec<u8> {
        let n = entries.len();
        let entries_end = ED25519_HEADER_LEN + n * ED25519_ENTRY_LEN;
        let keys_end = entries_end + n * PUBKEY_LEN;
        let sigs_end = keys_end + n * SIGNATURE_LEN;
        let msg_offset = sigs_end;

        let mut data = vec![0u8; msg_offset + message.len()];
        data[0] = n as u8;
        data[1] = 0;
        for (i, pk) in entries.iter().enumerate() {
            let base = ED25519_HEADER_LEN + i * ED25519_ENTRY_LEN;
            let sig_off = (keys_end + i * SIGNATURE_LEN) as u16;
            let pk_off = (entries_end + i * PUBKEY_LEN) as u16;
            data[base..base + 2].copy_from_slice(&sig_off.to_le_bytes());
            data[base + 2..base + 4].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
            data[base + 4..base + 6].copy_from_slice(&pk_off.to_le_bytes());
            data[base + 6..base + 8].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
            data[base + 8..base + 10].copy_from_slice(&(msg_offset as u16).to_le_bytes());
            data[base + 10..base + 12].copy_from_slice(&(message.len() as u16).to_le_bytes());
            data[base + 12..base + 14].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
            let pk_pos = entries_end + i * PUBKEY_LEN;
            data[pk_pos..pk_pos + PUBKEY_LEN].copy_from_slice(pk.as_ref());
        }
        data[msg_offset..].copy_from_slice(message);
        data
    }

    fn err_of(data: &[u8], keys: &[Pubkey]) -> Error {
        count_unique_attestation_signers(data, MSG, keys).unwrap_err()
    }

    #[test]
    fn counts_all_unique_valid_signers() {
        let k = keys(5);
        let data = build(&[k[0], k[2], k[4]], MSG);
        assert_eq!(count_unique_attestation_signers(&data, MSG, &k).unwrap(), 3);
    }

    #[test]
    fn counts_the_approved_two_of_three() {
        let k = keys(3);
        let data = build(&[k[0], k[1]], MSG);
        assert_eq!(count_unique_attestation_signers(&data, MSG, &k).unwrap(), 2);
    }

    #[test]
    fn full_max_key_set_counts() {
        let k = keys(MAX_ATTESTATION_KEYS);
        let data = build(&k, MSG);
        assert_eq!(
            count_unique_attestation_signers(&data, MSG, &k).unwrap(),
            MAX_ATTESTATION_KEYS
        );
    }

    #[test]
    fn rejects_duplicate_signer() {
        let k = keys(3);
        let data = build(&[k[0], k[1], k[0]], MSG);
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::DuplicateAttestationSignature)
        );
    }

    #[test]
    fn rejects_unknown_signer() {
        let k = keys(3);
        let outsider = Pubkey::new_unique();
        let data = build(&[k[0], outsider], MSG);
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::UnknownAttestationSigner)
        );
    }

    #[test]
    fn rejects_wrong_message() {
        let k = keys(3);
        let data = build(&[k[0], k[1]], b"some other message entirely.....");
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::SignatureMessageMismatch)
        );
    }

    #[test]
    fn rejects_message_with_matching_prefix_but_wrong_length() {
        let k = keys(3);
        let mut long = MSG.to_vec();
        long.push(0);
        let data = build(&[k[0]], &long);
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::SignatureMessageMismatch)
        );
    }

    #[test]
    fn rejects_cross_instruction_reference() {
        let k = keys(3);
        let mut data = build(&[k[0]], MSG);
        let base = ED25519_HEADER_LEN;
        data[base + 12..base + 14].copy_from_slice(&0u16.to_le_bytes());
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::MalformedSignatureVerification)
        );
    }

    #[test]
    fn rejects_zero_signature_count() {
        let k = keys(3);
        let mut data = build(&[k[0]], MSG);
        data[0] = 0;
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::MalformedSignatureVerification)
        );
    }

    #[test]
    fn rejects_nonzero_padding() {
        let k = keys(3);
        let mut data = build(&[k[0]], MSG);
        data[1] = 1;
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::MalformedSignatureVerification)
        );
    }

    #[test]
    fn rejects_truncated_data() {
        let k = keys(3);
        let data = build(&[k[0], k[1]], MSG);
        let truncated = &data[..ED25519_HEADER_LEN + ED25519_ENTRY_LEN + 4];
        assert_eq!(
            err_of(truncated, &k),
            Error::from(BridgeError::MalformedSignatureVerification)
        );
    }

    #[test]
    fn rejects_out_of_bounds_message_offset() {
        let k = keys(3);
        let mut data = build(&[k[0]], MSG);
        let base = ED25519_HEADER_LEN;
        data[base + 8..base + 10].copy_from_slice(&u16::MAX.to_le_bytes()[..2]);
        data[base + 12..base + 14].copy_from_slice(&SELF_REFERENCE.to_le_bytes());
        assert_eq!(
            err_of(&data, &k),
            Error::from(BridgeError::MalformedSignatureVerification)
        );
    }

    #[test]
    fn rejects_empty_data() {
        let k = keys(1);
        assert_eq!(
            err_of(&[], &k),
            Error::from(BridgeError::MalformedSignatureVerification)
        );
    }
}
