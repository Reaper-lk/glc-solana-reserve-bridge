//! Signer-policy tests.
//!
//! The one that matters most is
//! [`the_incident_payload_is_refused_by_a_signer_with_policy`]: it builds
//! the exact bytes the 2026-09-02 attacker asked the signers to sign and
//! asserts a policy-enforcing signer refuses them. Everything else here
//! defends the edges of that result.

use super::*;
use glc_reserve_bridge_shared::claim;
use glc_reserve_bridge_shared::governance;

fn program() -> Pubkey {
    Pubkey::new_from_array(glc_reserve_bridge_shared::PROGRAM_ID_BYTES)
}

const MINT: [u8; 32] = [0x44; 32];
const RESERVE_TA: [u8; 32] = [0x99; 32];

fn mint() -> Pubkey {
    Pubkey::new_from_array(MINT)
}

/// The posture a RESERVE-WITHDRAWAL credential grants: one approved
/// treasury, a ceiling, no governance pre-approvals.
fn withdrawal_policy(treasuries: Vec<Pubkey>) -> SignerPolicy {
    SignerPolicy {
        program_id: program(),
        reserve_mint: mint(),
        allowed_classes: vec![ActionClass::ReserveWithdrawal],
        allowed_treasuries: treasuries,
        max_withdrawal_amount: 100_000,
        approved_governance_commitments: Vec::new(),
    }
}

/// The posture the BRIDGE DAEMON's continuously-used credential grants:
/// settlement only. This is the credential that was on the compromised
/// host.
fn daemon_policy() -> SignerPolicy {
    SignerPolicy {
        program_id: program(),
        reserve_mint: mint(),
        allowed_classes: vec![ActionClass::Settlement],
        allowed_treasuries: Vec::new(),
        max_withdrawal_amount: 0,
        approved_governance_commitments: Vec::new(),
    }
}

fn treasury_payload(destination: &Pubkey, amount: u64) -> Vec<u8> {
    claim::treasury_withdraw_claim_message(
        1,
        &program().to_bytes(),
        0,
        1,
        amount,
        &destination.to_bytes(),
        &MINT,
        &RESERVE_TA,
        0,
    )
    .to_vec()
}

fn refund_payload(destination: &Pubkey, requester: &Pubkey, amount: u64) -> Vec<u8> {
    claim::refund_withdraw_claim_message(
        1,
        &program().to_bytes(),
        0,
        1 << 63,
        amount,
        &destination.to_bytes(),
        &MINT,
        &RESERVE_TA,
        7,
        &requester.to_bytes(),
    )
    .to_vec()
}

fn release_payload(recipient: &Pubkey, amount: u64) -> Vec<u8> {
    claim::release_claim_message(
        1,
        &program().to_bytes(),
        0,
        &[0x22; 32],
        0,
        amount,
        &recipient.to_bytes(),
        &MINT,
    )
    .to_vec()
}

// =====================================================================
// The incident.
// =====================================================================

/// A signer holding a withdrawal credential and its own treasury
/// allowlist refuses to sign a withdrawal to an address it never agreed
/// to — which is precisely what the attacker asked for, and precisely
/// what a blind oracle happily signed.
#[test]
fn the_incident_payload_is_refused_by_a_signer_with_policy() {
    let treasury = Pubkey::new_unique();
    let attacker = Pubkey::new_unique();
    let policy = withdrawal_policy(vec![treasury]);

    let err = policy
        .evaluate(&treasury_payload(&attacker, 50_000))
        .unwrap_err();
    assert_eq!(
        err,
        PolicyError::DestinationNotAllowlisted {
            destination: attacker
        }
    );
    // The refusal must name the destination, so an operator reading the
    // signer's log sees immediately WHERE someone tried to send funds.
    assert!(err.to_string().contains(&attacker.to_string()));
}

/// The single highest-value control: the credential that lives on the
/// bridge host cannot authorize a reserve withdrawal AT ALL — not to an
/// attacker's address, and not even to the legitimate treasury.
#[test]
fn the_bridge_hosts_own_credential_cannot_authorize_any_reserve_withdrawal() {
    let treasury = Pubkey::new_unique();
    let attacker = Pubkey::new_unique();
    let daemon = daemon_policy();

    for destination in [treasury, attacker] {
        let err = daemon
            .evaluate(&treasury_payload(&destination, 1))
            .unwrap_err();
        assert_eq!(
            err,
            PolicyError::ActionClassNotPermitted {
                requested: ActionClass::ReserveWithdrawal,
                allowed: vec![ActionClass::Settlement],
            }
        );
    }

    // Nor a refund, which is the other member of that class.
    let err = daemon
        .evaluate(&refund_payload(
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            1,
        ))
        .unwrap_err();
    assert!(matches!(err, PolicyError::ActionClassNotPermitted { .. }));
}

/// …while the daemon's ordinary settlement traffic keeps working. Scoping
/// the credential must not break the bridge's actual job.
#[test]
fn the_bridge_hosts_credential_still_authorizes_settlement() {
    let daemon = daemon_policy();
    let recipient = Pubkey::new_unique();
    let approved = daemon
        .evaluate(&release_payload(&recipient, 1_000))
        .unwrap();
    assert_eq!(approved.class(), ActionClass::Settlement);
    assert!(approved.summary().starts_with("SETTLEMENT release 1000"));
}

/// The reverse scoping: an operator withdrawal credential must not be
/// usable to sign settlement traffic either. Least privilege runs both
/// ways, so a leaked operator credential cannot be used to grind out
/// fraudulent releases.
#[test]
fn the_withdrawal_credential_cannot_authorize_settlement() {
    let policy = withdrawal_policy(vec![Pubkey::new_unique()]);
    let err = policy
        .evaluate(&release_payload(&Pubkey::new_unique(), 1))
        .unwrap_err();
    assert_eq!(
        err,
        PolicyError::ActionClassNotPermitted {
            requested: ActionClass::Settlement,
            allowed: vec![ActionClass::ReserveWithdrawal],
        }
    );
}

// =====================================================================
// Approval of the legitimate case.
// =====================================================================

#[test]
fn an_allowlisted_treasury_within_the_ceiling_is_approved() {
    let treasury = Pubkey::new_unique();
    let policy = withdrawal_policy(vec![treasury]);
    let approved = policy
        .evaluate(&treasury_payload(&treasury, 100_000))
        .unwrap();
    match approved {
        ClaimRequest::TreasuryWithdraw {
            amount,
            destination,
            policy_version,
            reserve_token_account,
            ..
        } => {
            assert_eq!(amount, 100_000);
            assert_eq!(destination, treasury);
            assert_eq!(policy_version, 0);
            assert_eq!(reserve_token_account, Pubkey::new_from_array(RESERVE_TA));
        }
        other => panic!("expected a treasury withdrawal, got {other:?}"),
    }
    assert!(approved.summary().contains("RESERVE WITHDRAWAL"));
}

#[test]
fn the_signers_own_ceiling_is_enforced_independently_of_the_chain() {
    let treasury = Pubkey::new_unique();
    let policy = withdrawal_policy(vec![treasury]);
    assert_eq!(
        policy
            .evaluate(&treasury_payload(&treasury, 100_001))
            .unwrap_err(),
        PolicyError::AmountAboveCeiling {
            amount: 100_001,
            ceiling: 100_000,
        }
    );
}

#[test]
fn a_refund_is_approved_and_reports_the_depositor() {
    let requester = Pubkey::new_unique();
    let destination = Pubkey::new_unique();
    let policy = withdrawal_policy(vec![Pubkey::new_unique()]);
    let approved = policy
        .evaluate(&refund_payload(&destination, &requester, 5_000))
        .unwrap();
    match approved {
        ClaimRequest::RefundWithdraw {
            obligation_index,
            requester: r,
            amount,
            ..
        } => {
            assert_eq!(obligation_index, 7);
            assert_eq!(r, requester);
            assert_eq!(amount, 5_000);
        }
        other => panic!("expected a refund, got {other:?}"),
    }
    // A refund destination is NOT allowlist-checked — it is derived on
    // chain — so an unlisted destination here is correct, not a bypass.
    assert!(!policy.allowed_treasuries.contains(&destination));
}

// =====================================================================
// Parsing rigour: fail closed on anything not fully understood.
// =====================================================================

/// The retired unrestricted withdrawal claim is refused outright. Its
/// on-chain instruction is inert, so a signature would be harmless — but
/// a request for one means something is running pre-hardening tooling, or
/// probing, and either deserves a refusal in the log.
#[test]
fn the_retired_rebalance_claim_is_refused() {
    let policy = withdrawal_policy(vec![Pubkey::new_unique()]);
    let payload = claim::rebalance_withdraw_claim_message(
        1,
        &program().to_bytes(),
        0,
        1,
        50_000,
        &Pubkey::new_unique().to_bytes(),
        &MINT,
    )
    .to_vec();
    assert_eq!(
        policy.evaluate(&payload).unwrap_err(),
        PolicyError::UnknownAction {
            action: claim::ACTION_REBALANCE_WITHDRAW
        }
    );
}

#[test]
fn an_unknown_domain_tag_is_refused() {
    let mut payload = treasury_payload(&Pubkey::new_unique(), 1);
    payload[0..16].copy_from_slice(b"NOT_OUR_TAG_0000");
    assert_eq!(
        parse_claim(&payload).unwrap_err(),
        PolicyError::UnknownDomainTag
    );
}

#[test]
fn an_unknown_action_byte_is_refused() {
    let mut payload = treasury_payload(&Pubkey::new_unique(), 1);
    payload[57] = 0x7F;
    assert_eq!(
        parse_claim(&payload).unwrap_err(),
        PolicyError::UnknownAction { action: 0x7F }
    );
    // 0x00 is permanently invalid and must be refused like any other.
    payload[57] = 0x00;
    assert_eq!(
        parse_claim(&payload).unwrap_err(),
        PolicyError::UnknownAction { action: 0x00 }
    );
}

/// A payload wearing one family's action byte at another family's length
/// is exactly the shape of a confusion attack. Length and action must
/// agree.
#[test]
fn an_action_byte_at_the_wrong_length_is_refused() {
    let mut payload = treasury_payload(&Pubkey::new_unique(), 1);
    payload[57] = claim::ACTION_REFUND_WITHDRAW; // refund action, treasury length
    assert_eq!(
        parse_claim(&payload).unwrap_err(),
        PolicyError::ActionLengthMismatch {
            action: claim::ACTION_REFUND_WITHDRAW,
            len: claim::TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN,
        }
    );
}

#[test]
fn a_truncated_payload_is_refused_rather_than_padded() {
    let payload = treasury_payload(&Pubkey::new_unique(), 1);
    for cut in [0usize, 1, 40, 57, 100, 177] {
        assert!(
            parse_claim(&payload[..cut]).is_err(),
            "a {cut}-byte payload must never parse"
        );
    }
}

#[test]
fn an_over_long_payload_is_refused_rather_than_truncated() {
    let mut payload = treasury_payload(&Pubkey::new_unique(), 1);
    payload.push(0);
    assert!(parse_claim(&payload).is_err());
}

#[test]
fn a_request_for_a_different_program_is_refused() {
    let treasury = Pubkey::new_unique();
    let policy = withdrawal_policy(vec![treasury]);
    let other_program = Pubkey::new_unique();
    let payload = claim::treasury_withdraw_claim_message(
        1,
        &other_program.to_bytes(),
        0,
        1,
        1_000,
        &treasury.to_bytes(),
        &MINT,
        &RESERVE_TA,
        0,
    )
    .to_vec();
    assert_eq!(
        policy.evaluate(&payload).unwrap_err(),
        PolicyError::WrongProgram {
            expected: program(),
            actual: other_program,
        }
    );
}

#[test]
fn a_request_for_a_different_reserve_mint_is_refused() {
    let treasury = Pubkey::new_unique();
    let policy = withdrawal_policy(vec![treasury]);
    let other_mint = [0x55u8; 32];
    let payload = claim::treasury_withdraw_claim_message(
        1,
        &program().to_bytes(),
        0,
        1,
        1_000,
        &treasury.to_bytes(),
        &other_mint,
        &RESERVE_TA,
        0,
    )
    .to_vec();
    assert_eq!(
        policy.evaluate(&payload).unwrap_err(),
        PolicyError::WrongReserveMint {
            expected: mint(),
            actual: Pubkey::new_from_array(other_mint),
        }
    );
}

// =====================================================================
// Governance.
// =====================================================================

/// Governance defaults to refused. A proposal must be reviewed out of
/// band and its commitment entered deliberately — a signer that approves
/// governance by default would let a quorum-capable attacker rewrite the
/// allowlist that the rest of this module depends on.
#[test]
fn governance_is_refused_unless_pre_approved_out_of_band() {
    let commitment = [0xAB; 32];
    let payload = governance::governance_message(
        1,
        &program().to_bytes(),
        0,
        governance::ACTION_PROPOSE_REBALANCE_POLICY,
        &commitment,
    )
    .to_vec();

    let mut policy = withdrawal_policy(vec![Pubkey::new_unique()]);
    policy.allowed_classes = vec![ActionClass::Governance];
    assert_eq!(
        policy.evaluate(&payload).unwrap_err(),
        PolicyError::GovernanceNotPreApproved { commitment }
    );

    policy.approved_governance_commitments.push(commitment);
    let approved = policy.evaluate(&payload).unwrap();
    assert_eq!(approved.class(), ActionClass::Governance);
    assert!(approved.summary().contains("GOVERNANCE"));
}

/// A governance payload presented under a withdrawal credential is
/// refused on the class check, before the commitment is even consulted.
#[test]
fn governance_under_a_withdrawal_credential_is_refused_on_class() {
    let commitment = [0xAB; 32];
    let payload = governance::governance_message(
        1,
        &program().to_bytes(),
        0,
        governance::ACTION_PROPOSE_REBALANCE_POLICY,
        &commitment,
    )
    .to_vec();
    let mut policy = withdrawal_policy(vec![Pubkey::new_unique()]);
    policy.approved_governance_commitments.push(commitment);
    assert_eq!(
        policy.evaluate(&payload).unwrap_err(),
        PolicyError::ActionClassNotPermitted {
            requested: ActionClass::Governance,
            allowed: vec![ActionClass::ReserveWithdrawal],
        }
    );
}

// =====================================================================
// Classification.
// =====================================================================

#[test]
fn every_family_lands_in_the_class_its_credential_scoping_assumes() {
    let recipient = Pubkey::new_unique();
    assert_eq!(
        parse_claim(&release_payload(&recipient, 1))
            .unwrap()
            .class(),
        ActionClass::Settlement
    );
    let completion = claim::goldcoin_completion_message(
        1,
        &program().to_bytes(),
        0,
        3,
        &[0x55; 32],
        10,
        1_000,
        &[0x66; 32],
    )
    .to_vec();
    assert_eq!(
        parse_claim(&completion).unwrap().class(),
        ActionClass::Settlement
    );
    assert_eq!(
        parse_claim(&treasury_payload(&recipient, 1))
            .unwrap()
            .class(),
        ActionClass::ReserveWithdrawal
    );
    assert_eq!(
        parse_claim(&refund_payload(&recipient, &recipient, 1))
            .unwrap()
            .class(),
        ActionClass::ReserveWithdrawal
    );
    let gov = governance::governance_message(
        1,
        &program().to_bytes(),
        0,
        governance::ACTION_PROPOSE_ROTATION,
        &[0; 32],
    )
    .to_vec();
    assert_eq!(parse_claim(&gov).unwrap().class(), ActionClass::Governance);
}

/// Round-trip: every field a signer reports in its audit log must be the
/// field that was actually signed, at the offset the shared crate wrote
/// it to. A parser that silently read the wrong offset would produce a
/// confident, wrong summary — worse than no summary.
#[test]
fn parsed_fields_round_trip_the_builders_exactly() {
    let destination = Pubkey::new_unique();
    let payload = claim::treasury_withdraw_claim_message(
        1,
        &program().to_bytes(),
        0x0102030405060708,
        0x1122334455667788,
        0x0A0B0C0D0E0F1011,
        &destination.to_bytes(),
        &MINT,
        &RESERVE_TA,
        0x00000000000000AB,
    )
    .to_vec();
    match parse_claim(&payload).unwrap() {
        ClaimRequest::TreasuryWithdraw {
            program_id,
            attestation_epoch,
            nonce,
            amount,
            destination: d,
            reserve_mint,
            reserve_token_account,
            policy_version,
        } => {
            assert_eq!(program_id, program());
            assert_eq!(attestation_epoch, 0x0102030405060708);
            assert_eq!(nonce, 0x1122334455667788);
            assert_eq!(amount, 0x0A0B0C0D0E0F1011);
            assert_eq!(d, destination);
            assert_eq!(reserve_mint, mint());
            assert_eq!(reserve_token_account, Pubkey::new_from_array(RESERVE_TA));
            assert_eq!(policy_version, 0xAB);
        }
        other => panic!("expected a treasury withdrawal, got {other:?}"),
    }
}
