//! Integration tests for the timelocked program-upgrade mechanism
//! (docs/12-management-decisions.md item 3, option (c)). Proves the
//! mechanism is fully real and testable — including the actual
//! `bpf_loader_upgradeable` CPIs — while never touching any production
//! key or performing any real deployment: every test runs against a fresh
//! litesvm instance and fresh, throwaway dev keypairs.

mod common;

use solana_sdk::signature::{Keypair, Signer};

use common::*;
use glc_reserve_bridge::errors::BridgeError;

/// Before `accept_upgrade_authority` is ever called, the real loader-level
/// authority is still the external deployer key — proving that shipping
/// this mechanism changes nothing about a deployment on its own.
#[test]
fn before_acceptance_the_real_upgrade_authority_is_still_the_external_key() {
    let authority = Keypair::new();
    let svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);
    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(authority.pubkey())
    );
}

#[test]
fn accept_upgrade_authority_hands_real_control_to_the_program_pda() {
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let ix = accept_upgrade_authority_ix(&authority.pubkey());
    send(&mut svm, ix, &authority, &[]).expect("accept should succeed");

    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(upgrade_authority_pda()),
        "the real, loader-owned upgrade authority must now be the program's own PDA"
    );
}

#[test]
fn accept_upgrade_authority_rejects_a_signer_that_is_not_the_real_current_authority() {
    let authority = Keypair::new();
    let impostor = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);
    svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let ix = accept_upgrade_authority_ix(&impostor.pubkey());
    let result = send(&mut svm, ix, &impostor, &[]);
    assert_bridge_error(result, BridgeError::NotCurrentUpgradeAuthority);
    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(authority.pubkey()),
        "a rejected handoff attempt must never change the real authority"
    );
}

#[test]
fn propose_upgrade_rejects_a_non_admin_signer() {
    let authority = Keypair::new();
    let impostor = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);
    svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let buffer = Keypair::new().pubkey();
    let ix = propose_upgrade_ix(&impostor.pubkey(), buffer);
    let result = send(&mut svm, ix, &impostor, &[]);
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn cancel_upgrade_rejects_a_non_admin_signer() {
    let authority = Keypair::new();
    let impostor = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);
    svm.airdrop(&impostor.pubkey(), 10_000_000_000).unwrap();

    let buffer = Keypair::new().pubkey();
    let propose = propose_upgrade_ix(&authority.pubkey(), buffer);
    send(&mut svm, propose, &authority, &[]).expect("propose should succeed");

    let cancel = cancel_upgrade_ix(&impostor.pubkey());
    let result = send(&mut svm, cancel, &impostor, &[]);
    assert_bridge_error(result, BridgeError::UnauthorizedAdmin);
}

#[test]
fn a_second_proposal_is_rejected_while_one_is_already_pending() {
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let first = propose_upgrade_ix(&authority.pubkey(), Keypair::new().pubkey());
    send(&mut svm, first, &authority, &[]).expect("first proposal should succeed");

    let second = propose_upgrade_ix(&authority.pubkey(), Keypair::new().pubkey());
    let result = send(&mut svm, second, &authority, &[]);
    assert!(
        result.is_err(),
        "a second proposal must be rejected structurally (the singleton PDA already exists), \
         the same replay/idempotency guard governance actions rely on"
    );
}

#[test]
fn cancel_upgrade_closes_the_pending_account_and_a_fresh_proposal_can_follow() {
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let first_buffer = Keypair::new().pubkey();
    let propose = propose_upgrade_ix(&authority.pubkey(), first_buffer);
    send(&mut svm, propose, &authority, &[]).expect("propose should succeed");

    let cancel = cancel_upgrade_ix(&authority.pubkey());
    send(&mut svm, cancel, &authority, &[]).expect("cancel should succeed");

    let second_buffer = Keypair::new().pubkey();
    let propose_again = propose_upgrade_ix(&authority.pubkey(), second_buffer);
    send(&mut svm, propose_again, &authority, &[])
        .expect("re-proposal after cancel should succeed");

    let pending = get_pending_upgrade(&svm);
    assert_eq!(pending.buffer_address, second_buffer);
}

#[test]
fn execute_upgrade_before_the_timelock_elapses_is_rejected() {
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let accept = accept_upgrade_authority_ix(&authority.pubkey());
    send(&mut svm, accept, &authority, &[]).expect("accept should succeed");

    let buffer = Keypair::new().pubkey();
    svm.set_account(
        buffer,
        buffer_account(Some(upgrade_authority_pda()), &program_bytes()),
    )
    .unwrap();

    let propose = propose_upgrade_ix(&authority.pubkey(), buffer);
    send(&mut svm, propose, &authority, &[]).expect("propose should succeed");

    // No time has passed — execution must be refused.
    let execute = execute_upgrade_ix(&authority.pubkey(), buffer);
    let result = send(&mut svm, execute, &authority, &[]);
    assert_bridge_error(result, BridgeError::UpgradeTimelockNotElapsed);
}

#[test]
fn execute_upgrade_fails_closed_when_authority_was_never_accepted() {
    // Timelock elapses, but `accept_upgrade_authority` was never called —
    // the mechanism must refuse to no-op as "success"; it must fail with
    // a specific, distinguishable error.
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let buffer = Keypair::new().pubkey();
    svm.set_account(
        buffer,
        // Buffer authority is the PDA even though the PDA doesn't yet
        // hold real program authority — proves the check that matters is
        // against `program_data`, not the buffer.
        buffer_account(Some(upgrade_authority_pda()), &program_bytes()),
    )
    .unwrap();

    let propose = propose_upgrade_ix(&authority.pubkey(), buffer);
    send(&mut svm, propose, &authority, &[]).expect("propose should succeed");

    warp_seconds(&mut svm, DEFAULT_UPGRADE_TIMELOCK + 1);

    let execute = execute_upgrade_ix(&authority.pubkey(), buffer);
    let result = send(&mut svm, execute, &authority, &[]);
    assert_bridge_error(result, BridgeError::UpgradeAuthorityNotYetAccepted);
    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(authority.pubkey()),
        "a refused execution must never have touched real upgrade authority"
    );
}

#[test]
fn execute_upgrade_rejects_a_buffer_other_than_the_one_proposed() {
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let accept = accept_upgrade_authority_ix(&authority.pubkey());
    send(&mut svm, accept, &authority, &[]).expect("accept should succeed");

    let proposed_buffer = Keypair::new().pubkey();
    let propose = propose_upgrade_ix(&authority.pubkey(), proposed_buffer);
    send(&mut svm, propose, &authority, &[]).expect("propose should succeed");

    warp_seconds(&mut svm, DEFAULT_UPGRADE_TIMELOCK + 1);

    // A different buffer than what was proposed and timelocked.
    let swapped_buffer = Keypair::new().pubkey();
    svm.set_account(
        swapped_buffer,
        buffer_account(Some(upgrade_authority_pda()), &program_bytes()),
    )
    .unwrap();
    let execute = execute_upgrade_ix(&authority.pubkey(), swapped_buffer);
    let result = send(&mut svm, execute, &authority, &[]);
    assert_bridge_error(result, BridgeError::WrongUpgradeBuffer);
}

/// The full, real, end-to-end mechanism: hand off real authority, propose
/// an upgrade, wait out the timelock, and execute a genuine
/// `bpf_loader_upgradeable::upgrade` CPI — proving the timelock actually
/// enforces something rather than merely logging an intent. Upgrades to
/// the SAME program bytes (a safe, deterministic choice for a test — the
/// property under test is the CPI itself succeeding under real loader
/// semantics, not a change in behavior).
#[test]
fn full_lifecycle_hands_off_authority_and_executes_a_real_upgrade_after_the_timelock() {
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let accept = accept_upgrade_authority_ix(&authority.pubkey());
    send(&mut svm, accept, &authority, &[]).expect("accept should succeed");
    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(upgrade_authority_pda())
    );

    let elf = program_bytes();
    let buffer = Keypair::new().pubkey();
    svm.set_account(buffer, buffer_account(Some(upgrade_authority_pda()), &elf))
        .unwrap();

    let propose = propose_upgrade_ix(&authority.pubkey(), buffer);
    send(&mut svm, propose, &authority, &[]).expect("propose should succeed");

    warp_seconds(&mut svm, DEFAULT_UPGRADE_TIMELOCK + 1);
    // The loader refuses to upgrade a program within the same slot it was
    // (re)deployed in — advance the slot too, not just the clock.
    svm.warp_to_slot(100);

    // Permissionless: a fresh, unrelated fee payer may execute.
    let executor = Keypair::new();
    svm.airdrop(&executor.pubkey(), 10_000_000_000).unwrap();
    let execute = execute_upgrade_ix(&executor.pubkey(), buffer);
    send(&mut svm, execute, &executor, &[]).expect("execute should succeed after the timelock");

    // Real, loader-level state: the program is still owned by this
    // program's own PDA (the CPI never regressed authority), and the
    // pending-upgrade singleton is gone (replay/idempotency: a second
    // execute_upgrade cannot possibly reference the same closed account).
    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(upgrade_authority_pda())
    );
    let pending_after = svm.get_account(&pending_upgrade_pda());
    assert!(
        pending_after.is_none() || pending_after.unwrap().lamports == 0,
        "the pending-upgrade account must be closed (rent refunded, discriminator cleared) — a \
         second execute_upgrade can never reference this same request again"
    );

    // The buffer account is closed by the loader as part of a real
    // upgrade (its lamports are reclaimed) — further real-runtime proof
    // this was a genuine `Upgrade` CPI, not a no-op that merely emitted
    // an event.
    let buffer_after = svm.get_account(&buffer);
    assert!(
        buffer_after.is_none() || buffer_after.unwrap().lamports == 0,
        "the loader must have closed/drained the consumed buffer account"
    );
}

#[test]
fn execute_upgrade_is_permissionless_but_confers_no_authority() {
    // Anyone may submit `execute_upgrade` once the timelock has elapsed —
    // the authorization was the admin's proposal, not the executor's
    // identity (same discipline as attestation-key-rotation execution).
    let authority = Keypair::new();
    let mut svm = setup_initialized_with(&authority, pubkeys(&keys(3)), 2);

    let accept = accept_upgrade_authority_ix(&authority.pubkey());
    send(&mut svm, accept, &authority, &[]).unwrap();

    let buffer = Keypair::new().pubkey();
    svm.set_account(
        buffer,
        buffer_account(Some(upgrade_authority_pda()), &program_bytes()),
    )
    .unwrap();
    let propose = propose_upgrade_ix(&authority.pubkey(), buffer);
    send(&mut svm, propose, &authority, &[]).unwrap();
    warp_seconds(&mut svm, DEFAULT_UPGRADE_TIMELOCK + 1);
    svm.warp_to_slot(100);

    let stranger = Keypair::new();
    svm.airdrop(&stranger.pubkey(), 10_000_000_000).unwrap();
    let execute = execute_upgrade_ix(&stranger.pubkey(), buffer);
    send(&mut svm, execute, &stranger, &[]).expect("any fee payer may execute");

    // Real upgrade authority remains the program's own PDA — the
    // executor gained nothing.
    assert_eq!(
        get_programdata_upgrade_authority(&svm),
        Some(upgrade_authority_pda())
    );
}
