//! Integration tests for attestation-key rotation governance. This is the
//! property the approved trust model depends on most directly
//! (docs/02-trust-model.md, docs/12-management-decisions.md item 1): no
//! single admin key can install attacker-controlled attestation keys, and a
//! threshold-approved rotation is still visible/cancellable for a full
//! timelock window before it takes effect.

mod common;

use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;

#[test]
fn rotation_cannot_execute_before_timelock_elapses() {
    let authority = Keypair::new();
    let (mut svm, signers) = setup_initialized_two_of_three(&authority);
    let new_keys = pubkeys(&keys(3));

    let key_set = get_attestation_key_set(&svm);
    let message = rotation_message(key_set.epoch, &new_keys, 2);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let propose = propose_rotation_ix(&authority.pubkey(), new_keys.clone(), 2);
    send_ixs(&mut svm, &[proof, propose], &authority, &[]).expect("propose should succeed");

    // No time has passed — execution must be refused.
    let execute = execute_rotation_ix(&authority.pubkey());
    let result = send(&mut svm, execute, &authority, &[]);
    assert_bridge_error(result, BridgeError::GovernanceTimelockNotElapsed);

    // The key set must be unchanged.
    let still_old = get_attestation_key_set(&svm);
    assert_eq!(still_old.epoch, key_set.epoch);
}

#[test]
fn rotation_executes_after_timelock_and_bumps_epoch() {
    let authority = Keypair::new();
    let (mut svm, signers) = setup_initialized_two_of_three(&authority);
    let new_keys = pubkeys(&keys(3));

    let key_set = get_attestation_key_set(&svm);
    let old_epoch = key_set.epoch;
    let message = rotation_message(old_epoch, &new_keys, 2);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let propose = propose_rotation_ix(&authority.pubkey(), new_keys.clone(), 2);
    send_ixs(&mut svm, &[proof, propose], &authority, &[]).expect("propose should succeed");

    warp_seconds(&mut svm, DEFAULT_TEST_TIMELOCK + 1);

    let execute = execute_rotation_ix(&authority.pubkey());
    send(&mut svm, execute, &authority, &[]).expect("execute should succeed after timelock");

    let rotated = get_attestation_key_set(&svm);
    assert_eq!(rotated.epoch, old_epoch + 1);
    assert_eq!(rotated.keys, new_keys);
}

#[test]
fn a_single_signature_cannot_propose_a_rotation() {
    // The most important property this whole module exists for: a single
    // compromised (or merely available) key cannot install a new,
    // attacker-controlled attestation key set — docs/02-trust-model.md's
    // "no single key can release reserves" extends to "no single key can
    // change WHO the keys are," or the property would be circumventable.
    let authority = Keypair::new();
    let (mut svm, signers) = setup_initialized_two_of_three(&authority);
    let new_keys = pubkeys(&keys(3));

    let key_set = get_attestation_key_set(&svm);
    let message = rotation_message(key_set.epoch, &new_keys, 2);
    let proof = ed25519_proof_ix(&[&signers[0]], &message);
    let propose = propose_rotation_ix(&authority.pubkey(), new_keys, 2);

    let result = send_ixs(&mut svm, &[proof, propose], &authority, &[]);
    assert_bridge_error(result, BridgeError::InsufficientSignatures);
}

#[test]
fn stale_attestation_epoch_after_rotation_is_rejected_for_release() {
    // A release attestation signed under the OLD epoch must not verify
    // after rotation — this is what makes rotation an actual containment
    // measure rather than cosmetic (docs/10-threat-model.md's rotation
    // rehearsal item).
    let authority = Keypair::new();
    let (mut svm, signers, mint) = setup_with_reserve(&authority, 1_000_000);
    let key_set = get_attestation_key_set(&svm);
    let old_epoch = key_set.epoch;

    let new_keys = pubkeys(&keys(3));
    let rmsg = rotation_message(old_epoch, &new_keys, 2);
    let rproof = ed25519_proof_ix(&[&signers[0], &signers[1]], &rmsg);
    let propose = propose_rotation_ix(&authority.pubkey(), new_keys, 2);
    send_ixs(&mut svm, &[rproof, propose], &authority, &[]).expect("propose should succeed");
    warp_seconds(&mut svm, DEFAULT_TEST_TIMELOCK + 1);
    let execute = execute_rotation_ix(&authority.pubkey());
    send(&mut svm, execute, &authority, &[]).expect("execute should succeed");

    // Now attempt a release using the OLD signers and OLD epoch number.
    let recipient = Keypair::new();
    let recipient_ata = create_ata(&mut svm, &recipient.pubkey(), &mint, 0);
    let txid = [0x77; 32];
    let message = release_claim_message(old_epoch, &txid, 0, 1_000, &recipient.pubkey(), &mint);
    let proof = ed25519_proof_ix(&[&signers[0], &signers[1]], &message);
    let release = release_from_reserve_ix(
        &authority.pubkey(),
        &mint,
        &recipient.pubkey(),
        &recipient_ata,
        txid,
        0,
        1_000,
        old_epoch,
    );

    let result = send_ixs(&mut svm, &[proof, release], &authority, &[]);
    assert_bridge_error(result, BridgeError::StaleAttestationEpoch);
}
