//! Pure validation rules for the internal attestation-key set and the
//! reserve rebalance policy, each shared by every path that writes them so
//! the paths can never drift.
//! Kept free of account types so the rules are unit-testable without a
//! runtime. Adapted from the old bridge's `validate_validator_set`
//! (docs/01-reuse-inventory.md) — identical shape, with a hard minimum
//! threshold of 2 added: per the approved trust model
//! (docs/02-trust-model.md), a threshold of 1 would let a single key
//! release reserves, which is exactly what was ruled out.

use anchor_lang::prelude::*;

use crate::constants::{MAX_ATTESTATION_KEYS, MAX_TREASURY_DESTINATIONS};
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

/// Enforces every invariant of a proposed rebalance policy. Called at
/// PROPOSAL time (so an invalid policy can never sit in the timelock queue
/// looking legitimate), again at EXECUTION time (so a policy that was
/// valid when proposed cannot be applied if the rules have since
/// tightened), and again at INITIALIZATION.
///
/// `reserve_token_account` is the live reserve vault address: a treasury
/// destination equal to it would be a withdrawal to the account the funds
/// are already in, which is at best a no-op and at worst a way to make an
/// audit trail of "withdrawals" that never left. It is refused here rather
/// than only at execution time so the impossible policy can never be
/// approved in the first place.
pub fn validate_rebalance_policy(
    treasuries: &[Pubkey],
    reserve_token_account: &Pubkey,
) -> Result<()> {
    require!(!treasuries.is_empty(), BridgeError::EmptyTreasuryAllowlist);
    require!(
        treasuries.len() <= MAX_TREASURY_DESTINATIONS,
        BridgeError::TooManyTreasuryDestinations
    );
    for (i, t) in treasuries.iter().enumerate() {
        require!(
            *t != Pubkey::default(),
            BridgeError::InvalidTreasuryDestination
        );
        require!(
            t != reserve_token_account,
            BridgeError::TreasuryDestinationIsReserveItself
        );
        require!(
            !treasuries[..i].contains(t),
            BridgeError::DuplicateTreasuryDestination
        );
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

    // ------------------------------------------ validate_rebalance_policy --

    fn policy_err(treasuries: &[Pubkey], reserve: &Pubkey) -> Error {
        validate_rebalance_policy(treasuries, reserve).unwrap_err()
    }

    #[test]
    fn accepts_the_production_shape_of_exactly_one_treasury() {
        let reserve = Pubkey::new_unique();
        let treasury = Pubkey::new_unique();
        assert!(
            validate_rebalance_policy(&[treasury], &reserve).is_ok(),
            "the initial production policy is a single canonical treasury"
        );
    }

    #[test]
    fn accepts_the_maximum_allowlist() {
        let reserve = Pubkey::new_unique();
        let treasuries: Vec<Pubkey> = (0..MAX_TREASURY_DESTINATIONS)
            .map(|_| Pubkey::new_unique())
            .collect();
        assert!(validate_rebalance_policy(&treasuries, &reserve).is_ok());
    }

    #[test]
    fn rejects_an_empty_allowlist() {
        let reserve = Pubkey::new_unique();
        assert_eq!(
            policy_err(&[], &reserve),
            Error::from(BridgeError::EmptyTreasuryAllowlist)
        );
    }

    #[test]
    fn rejects_more_than_max_destinations() {
        let reserve = Pubkey::new_unique();
        let treasuries: Vec<Pubkey> = (0..MAX_TREASURY_DESTINATIONS + 1)
            .map(|_| Pubkey::new_unique())
            .collect();
        assert_eq!(
            policy_err(&treasuries, &reserve),
            Error::from(BridgeError::TooManyTreasuryDestinations)
        );
    }

    #[test]
    fn rejects_the_default_pubkey_as_a_destination() {
        let reserve = Pubkey::new_unique();
        assert_eq!(
            policy_err(&[Pubkey::default()], &reserve),
            Error::from(BridgeError::InvalidTreasuryDestination)
        );
    }

    #[test]
    fn rejects_a_duplicate_destination() {
        let reserve = Pubkey::new_unique();
        let t = Pubkey::new_unique();
        assert_eq!(
            policy_err(&[t, t], &reserve),
            Error::from(BridgeError::DuplicateTreasuryDestination)
        );
    }

    /// Allowlisting the reserve vault itself would authorize a "withdrawal"
    /// that never leaves — refused at policy time, not merely at execution.
    #[test]
    fn rejects_the_reserve_token_account_as_a_destination() {
        let reserve = Pubkey::new_unique();
        assert_eq!(
            policy_err(&[reserve], &reserve),
            Error::from(BridgeError::TreasuryDestinationIsReserveItself)
        );
        // Also when it is not the first entry.
        assert_eq!(
            policy_err(&[Pubkey::new_unique(), reserve], &reserve),
            Error::from(BridgeError::TreasuryDestinationIsReserveItself)
        );
    }

    /// The allowlist is the WHOLE policy: a valid policy carries no amount
    /// ceiling, no rate limit and no rolling budget, so there is no second
    /// parameter here that could be misordered, left at a default, or
    /// become the binding restriction on a legitimate treasury operation.
    #[test]
    fn a_valid_allowlist_is_the_entire_policy() {
        let reserve = Pubkey::new_unique();
        let t = Pubkey::new_unique();
        assert!(validate_rebalance_policy(&[t], &reserve).is_ok());
    }
}
