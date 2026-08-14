//! Pure validation rules for the internal attestation-key set, shared by
//! `initialize` and the governance rotation path so the two can never drift.
//! Kept free of account types so the rules are unit-testable without a
//! runtime. Adapted from the old bridge's `validate_validator_set`
//! (docs/01-reuse-inventory.md) — identical shape, with a hard minimum
//! threshold of 2 added: per the approved trust model
//! (docs/02-trust-model.md), a threshold of 1 would let a single key
//! release reserves, which is exactly what was ruled out.

use anchor_lang::prelude::*;

use crate::constants::MAX_ATTESTATION_KEYS;
use crate::errors::BridgeError;

/// Minimum permitted threshold. See module docs: this is the on-chain
/// enforcement of "no single operator or single hot key capable of
/// releasing reserves."
pub const MIN_THRESHOLD: u8 = 2;

/// Enforces every invariant of a (keys, threshold) pair: non-empty,
/// `len <= MAX_ATTESTATION_KEYS`, no all-zero (default) keys, no duplicate
/// keys, `MIN_THRESHOLD <= threshold <= len`.
pub fn validate_attestation_key_set(keys: &[Pubkey], threshold: u8) -> Result<()> {
    require!(!keys.is_empty(), BridgeError::EmptyAttestationKeySet);
    require!(
        keys.len() <= MAX_ATTESTATION_KEYS,
        BridgeError::TooManyAttestationKeys
    );
    require!(threshold >= 1, BridgeError::ZeroThreshold);
    require!(
        usize::from(threshold) <= keys.len(),
        BridgeError::ThresholdExceedsKeyCount
    );
    require!(
        threshold >= MIN_THRESHOLD,
        BridgeError::ThresholdBelowMinimum
    );
    for key in keys {
        require!(
            *key != Pubkey::default(),
            BridgeError::InvalidAttestationKey
        );
    }
    // O(n²) pairwise scan: n <= MAX_ATTESTATION_KEYS (8), so at most 28
    // comparisons — cheaper and simpler than sorting or hashing on-chain.
    for i in 0..keys.len() {
        for j in (i + 1)..keys.len() {
            require!(keys[i] != keys[j], BridgeError::DuplicateAttestationKey);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(n: usize) -> Vec<Pubkey> {
        (0..n).map(|_| Pubkey::new_unique()).collect()
    }

    fn err_of(keys: &[Pubkey], threshold: u8) -> Error {
        validate_attestation_key_set(keys, threshold).unwrap_err()
    }

    #[test]
    fn accepts_approved_two_of_three() {
        assert!(validate_attestation_key_set(&keys(3), 2).is_ok());
    }

    #[test]
    fn accepts_full_set_at_max() {
        assert!(validate_attestation_key_set(
            &keys(MAX_ATTESTATION_KEYS),
            MAX_ATTESTATION_KEYS as u8
        )
        .is_ok());
    }

    #[test]
    fn accepts_partial_threshold_above_minimum() {
        assert!(validate_attestation_key_set(&keys(5), 3).is_ok());
    }

    #[test]
    fn rejects_empty_set() {
        assert_eq!(
            err_of(&[], 2),
            Error::from(BridgeError::EmptyAttestationKeySet)
        );
    }

    #[test]
    fn rejects_too_many_keys() {
        assert_eq!(
            err_of(&keys(MAX_ATTESTATION_KEYS + 1), 2),
            Error::from(BridgeError::TooManyAttestationKeys)
        );
    }

    #[test]
    fn rejects_zero_threshold() {
        assert_eq!(err_of(&keys(3), 0), Error::from(BridgeError::ZeroThreshold));
    }

    #[test]
    fn rejects_threshold_of_one_even_with_multiple_keys() {
        // The load-bearing case: a single-signature threshold is refused
        // regardless of how many keys exist, because it would let any ONE
        // of them release reserves alone.
        assert_eq!(
            err_of(&keys(3), 1),
            Error::from(BridgeError::ThresholdBelowMinimum)
        );
    }

    #[test]
    fn rejects_single_key_single_threshold() {
        assert_eq!(
            err_of(&keys(1), 1),
            Error::from(BridgeError::ThresholdBelowMinimum)
        );
    }

    #[test]
    fn rejects_threshold_above_count() {
        assert_eq!(
            err_of(&keys(3), 4),
            Error::from(BridgeError::ThresholdExceedsKeyCount)
        );
    }

    #[test]
    fn rejects_all_zero_key() {
        let mut k = keys(3);
        k[1] = Pubkey::default();
        assert_eq!(
            err_of(&k, 2),
            Error::from(BridgeError::InvalidAttestationKey)
        );
    }

    #[test]
    fn rejects_adjacent_duplicate() {
        let mut k = keys(3);
        k[1] = k[0];
        assert_eq!(
            err_of(&k, 2),
            Error::from(BridgeError::DuplicateAttestationKey)
        );
    }

    #[test]
    fn rejects_non_adjacent_duplicate() {
        let mut k = keys(MAX_ATTESTATION_KEYS);
        k[MAX_ATTESTATION_KEYS - 1] = k[0];
        assert_eq!(
            err_of(&k, 2),
            Error::from(BridgeError::DuplicateAttestationKey)
        );
    }
}
