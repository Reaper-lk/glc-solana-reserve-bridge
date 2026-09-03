//! Signer-side policy: what an attestation signer must independently
//! decide before it puts a signature on anything.
//!
//! # Why this module exists
//!
//! On 2026-09-02 the reserve was drained through the legitimate operator
//! withdrawal workflow. Every cryptographic control worked: the admin
//! signature was genuine, and the 2-of-3 threshold attestation was three
//! genuine signatures from three genuine current attestation keys over the
//! exact bytes that executed.
//!
//! It worked because the attestation signers were **blind oracles**. Their
//! entire authorization decision was "did this request carry a valid bearer
//! token?" — and the bearer tokens lived on the same production host as the
//! admin keypair. Two factors that share one host are one factor. The
//! threshold did not distribute trust across three custody domains; it
//! reduced to possession of two secrets from one filesystem.
//!
//! The on-chain allowlist added by the same hardening patch bounds the
//! damage regardless of what the signers do. This module addresses the
//! other half: making possession of a bridge-host credential insufficient,
//! by giving each custody domain the means to understand and refuse what it
//! is being asked to sign.
//!
//! # What a custody domain runs, and what it does not
//!
//! This crate is a *client*; it never holds signing keys (see
//! [`crate::signing::remote`]). The HTTP shim in front of an HSM/KMS is
//! each custody domain's own process, and this repository does not and
//! should not ship it.
//!
//! What it can ship — and what this module is — is the part that must be
//! identical everywhere: the parser for the canonical claim bytes, and the
//! policy decision over the parsed result. A domain links this, feeds it
//! the `payload_hex` from `POST /v1/sign`, and signs only on `Ok`.
//! `docs/28-signer-policy.md` is the operator-facing companion.
//!
//! # The rules, and why each one is here
//!
//! 1. **Parse, never pattern-match on length alone.** A signer that only
//!    checked "is this 138 bytes?" would have signed the incident payload.
//!    [`parse_claim`] recognizes the domain tag, the action byte and the
//!    exact family length, and rejects anything else outright.
//! 2. **Fail closed on the unknown.** An unrecognized domain tag or action
//!    byte is a refusal, not a shrug. A future protocol version must be
//!    deployed to the signers before it is deployed to the bridge, not the
//!    other way around.
//! 3. **Hold the treasury allowlist independently.** The whole point is
//!    that this list does not come from the requester. A domain that
//!    configured its allowlist by reading it from the bridge host would
//!    have rebuilt the original vulnerability with extra steps.
//! 4. **Scope credentials by action.** The daemon's continuously-used
//!    credential authorizes settlement actions only
//!    ([`ActionClass::Settlement`]). Reserve withdrawals require a
//!    credential that never exists on the bridge host. This is expressed
//!    here as [`SignerPolicy::allowed_classes`] and is the single change
//!    that most directly closes the incident path.
//! 5. **Bound the amount independently too.** The on-chain per-withdrawal
//!    limit is threshold-governed and cannot be raised from the bridge
//!    host — but a signer that also refuses above its own ceiling means an
//!    attacker must compromise the governance quorum AND every custody
//!    domain's configuration, not just the quorum.

use solana_sdk::pubkey::Pubkey;

use glc_reserve_bridge_shared::claim::{
    ACTION_REBALANCE_WITHDRAW, ACTION_RECORD_GOLDCOIN_COMPLETION, ACTION_REFUND_WITHDRAW,
    ACTION_RELEASE_FROM_RESERVE, ACTION_TREASURY_WITHDRAW, CLAIM_DOMAIN_TAG,
    COMPLETION_MESSAGE_LEN, REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN,
    REFUND_WITHDRAW_CLAIM_MESSAGE_LEN, RELEASE_CLAIM_MESSAGE_LEN,
    TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN,
};
use glc_reserve_bridge_shared::governance::{GOVERNANCE_DOMAIN_TAG, GOVERNANCE_MESSAGE_LEN};

/// Offset of the action byte. Shared by every claim and governance family
/// — the first 57 bytes are a common prefix (domain tag, protocol
/// version, program id, attestation epoch).
const ACTION_OFFSET: usize = 57;
const PROGRAM_ID_OFFSET: usize = 17;
const EPOCH_OFFSET: usize = 49;

/// Which broad category of action a payload belongs to. Credentials are
/// scoped to these, not to individual actions: an operator credential that
/// can authorize a treasury withdrawal can also authorize the refund of a
/// user's own deposit, and separating those two would add ceremony without
/// adding safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionClass {
    /// `release_from_reserve` and `record_goldcoin_completion` — the
    /// bridge's ordinary, continuous settlement traffic. The daemon signs
    /// thousands of these unattended, so its credential must be scoped to
    /// exactly this and nothing else.
    Settlement,
    /// `treasury_withdraw` and `refund_withdraw` — operator-initiated
    /// reserve movements. Rare, deliberate, and human-approved. The
    /// credential authorizing these MUST NOT exist on the bridge host.
    ReserveWithdrawal,
    /// Attestation-key rotation and rebalance-policy changes. Rarest of
    /// all, and the most dangerous to get wrong: these change who and what
    /// is trusted. Deserves its own credential and its own out-of-band
    /// approval.
    Governance,
}

/// A parsed, understood signing request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimRequest {
    /// Settling a confirmed Goldcoin deposit by releasing reserve GLC.
    Release {
        program_id: Pubkey,
        attestation_epoch: u64,
        txid: [u8; 32],
        vout: u32,
        amount: u64,
        recipient: Pubkey,
        reserve_mint: Pubkey,
    },
    /// Recording that a Goldcoin payout completed.
    Completion {
        program_id: Pubkey,
        attestation_epoch: u64,
        obligation_index: u64,
        amount: u64,
    },
    /// Moving reserve funds to a treasury.
    TreasuryWithdraw {
        program_id: Pubkey,
        attestation_epoch: u64,
        nonce: u64,
        amount: u64,
        destination: Pubkey,
        reserve_mint: Pubkey,
        reserve_token_account: Pubkey,
        policy_version: u64,
    },
    /// Returning a deposit to the wallet that made it.
    RefundWithdraw {
        program_id: Pubkey,
        attestation_epoch: u64,
        nonce: u64,
        amount: u64,
        destination: Pubkey,
        reserve_mint: Pubkey,
        obligation_index: u64,
        requester: Pubkey,
    },
    /// A governance message (key rotation or policy change). The
    /// parameters are behind a SHA-256 commitment rather than inline, so a
    /// signer cannot read them out of the payload — it must reconstruct
    /// the commitment from an independently-obtained proposal. See
    /// `docs/28-signer-policy.md`.
    Governance {
        program_id: Pubkey,
        attestation_epoch: u64,
        action: u8,
        params_commitment: [u8; 32],
    },
}

impl ClaimRequest {
    pub fn class(&self) -> ActionClass {
        match self {
            ClaimRequest::Release { .. } | ClaimRequest::Completion { .. } => {
                ActionClass::Settlement
            }
            ClaimRequest::TreasuryWithdraw { .. } | ClaimRequest::RefundWithdraw { .. } => {
                ActionClass::ReserveWithdrawal
            }
            ClaimRequest::Governance { .. } => ActionClass::Governance,
        }
    }

    pub fn program_id(&self) -> Pubkey {
        match self {
            ClaimRequest::Release { program_id, .. }
            | ClaimRequest::Completion { program_id, .. }
            | ClaimRequest::TreasuryWithdraw { program_id, .. }
            | ClaimRequest::RefundWithdraw { program_id, .. }
            | ClaimRequest::Governance { program_id, .. } => *program_id,
        }
    }

    pub fn attestation_epoch(&self) -> u64 {
        match self {
            ClaimRequest::Release {
                attestation_epoch, ..
            }
            | ClaimRequest::Completion {
                attestation_epoch, ..
            }
            | ClaimRequest::TreasuryWithdraw {
                attestation_epoch, ..
            }
            | ClaimRequest::RefundWithdraw {
                attestation_epoch, ..
            }
            | ClaimRequest::Governance {
                attestation_epoch, ..
            } => *attestation_epoch,
        }
    }

    /// A one-line human summary for the signer's own audit log. A custody
    /// domain that logs nothing else should log this.
    pub fn summary(&self) -> String {
        match self {
            ClaimRequest::Release {
                amount, recipient, ..
            } => format!("SETTLEMENT release {amount} to {recipient}"),
            ClaimRequest::Completion {
                obligation_index,
                amount,
                ..
            } => format!("SETTLEMENT completion of obligation {obligation_index} ({amount})"),
            ClaimRequest::TreasuryWithdraw {
                amount,
                destination,
                policy_version,
                ..
            } => format!(
                "RESERVE WITHDRAWAL {amount} to treasury {destination} (policy v{policy_version})"
            ),
            ClaimRequest::RefundWithdraw {
                amount,
                destination,
                obligation_index,
                requester,
                ..
            } => format!(
                "REFUND {amount} for obligation {obligation_index} to {requester}'s ATA \
                 {destination}"
            ),
            ClaimRequest::Governance { action, .. } => {
                format!("GOVERNANCE action {action:#04x}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error(
        "payload is {len} bytes, which is not the exact length of any known message family — \
         refusing to sign bytes this signer does not understand"
    )]
    UnknownLength { len: usize },
    #[error(
        "payload carries an unrecognized domain tag — refusing to sign bytes that are not a \
         claim or governance message for this bridge"
    )]
    UnknownDomainTag,
    #[error(
        "payload carries action byte {action:#04x}, which this signer does not recognize. A new \
         protocol action must be deployed to every custody domain BEFORE it is used"
    )]
    UnknownAction { action: u8 },
    #[error(
        "payload's action byte {action:#04x} does not match its length {len} — a malformed or \
         deliberately confusing request"
    )]
    ActionLengthMismatch { action: u8, len: usize },
    #[error(
        "request is for program {actual}, but this signer is configured for {expected}. Refusing \
         to sign for a deployment it does not serve"
    )]
    WrongProgram { expected: Pubkey, actual: Pubkey },
    #[error(
        "request concerns reserve mint {actual}, but this signer is configured for {expected}"
    )]
    WrongReserveMint { expected: Pubkey, actual: Pubkey },
    #[error(
        "the presented credential authorizes {allowed:?} actions only; this request is a \
         {requested:?} action. Reserve withdrawals require a credential that does not exist on \
         the bridge host"
    )]
    ActionClassNotPermitted {
        requested: ActionClass,
        allowed: Vec<ActionClass>,
    },
    #[error(
        "destination {destination} is not in THIS signer's independently-held treasury \
         allowlist. Refusing — a treasury address that this custody domain has not separately \
         agreed to is exactly what a compromised bridge host would ask for"
    )]
    DestinationNotAllowlisted { destination: Pubkey },
    #[error(
        "amount {amount} exceeds this signer's own ceiling of {ceiling} for reserve withdrawals"
    )]
    AmountAboveCeiling { amount: u64, ceiling: u64 },
    #[error(
        "governance requests require an out-of-band approved parameter commitment; this signer \
         has none matching {commitment:?}"
    )]
    GovernanceNotPreApproved { commitment: [u8; 32] },
}

/// Parses the exact bytes a `POST /v1/sign` request asked to be signed.
///
/// Deliberately strict on three axes at once — domain tag, action byte and
/// total length must all agree — so that a payload crafted to look like
/// one family while being another is rejected rather than merely
/// mis-summarized in a log.
pub fn parse_claim(payload: &[u8]) -> Result<ClaimRequest, PolicyError> {
    if payload.len() < ACTION_OFFSET + 1 {
        return Err(PolicyError::UnknownLength { len: payload.len() });
    }
    let tag = &payload[0..16];
    let action = payload[ACTION_OFFSET];
    let program_id = read_pubkey(payload, PROGRAM_ID_OFFSET);
    let attestation_epoch = read_u64(payload, EPOCH_OFFSET);

    if tag == GOVERNANCE_DOMAIN_TAG.as_slice() {
        if payload.len() != GOVERNANCE_MESSAGE_LEN {
            return Err(PolicyError::ActionLengthMismatch {
                action,
                len: payload.len(),
            });
        }
        let mut params_commitment = [0u8; 32];
        params_commitment.copy_from_slice(&payload[58..90]);
        return Ok(ClaimRequest::Governance {
            program_id,
            attestation_epoch,
            action,
            params_commitment,
        });
    }

    if tag != CLAIM_DOMAIN_TAG.as_slice() {
        return Err(PolicyError::UnknownDomainTag);
    }

    // Every arm below asserts the family's EXACT length before reading a
    // single field, so no read can run off the end and no short payload
    // can be interpreted as a longer family with zeroed tail bytes.
    let len = payload.len();
    match action {
        ACTION_RELEASE_FROM_RESERVE => {
            require_len(action, len, RELEASE_CLAIM_MESSAGE_LEN)?;
            let mut txid = [0u8; 32];
            txid.copy_from_slice(&payload[58..90]);
            Ok(ClaimRequest::Release {
                program_id,
                attestation_epoch,
                txid,
                vout: u32::from_le_bytes(payload[90..94].try_into().unwrap()),
                amount: read_u64(payload, 94),
                recipient: read_pubkey(payload, 102),
                reserve_mint: read_pubkey(payload, 134),
            })
        }
        ACTION_RECORD_GOLDCOIN_COMPLETION => {
            require_len(action, len, COMPLETION_MESSAGE_LEN)?;
            Ok(ClaimRequest::Completion {
                program_id,
                attestation_epoch,
                obligation_index: read_u64(payload, 58),
                amount: read_u64(payload, 106),
            })
        }
        ACTION_TREASURY_WITHDRAW => {
            require_len(action, len, TREASURY_WITHDRAW_CLAIM_MESSAGE_LEN)?;
            Ok(ClaimRequest::TreasuryWithdraw {
                program_id,
                attestation_epoch,
                nonce: read_u64(payload, 58),
                amount: read_u64(payload, 66),
                destination: read_pubkey(payload, 74),
                reserve_mint: read_pubkey(payload, 106),
                reserve_token_account: read_pubkey(payload, 138),
                policy_version: read_u64(payload, 170),
            })
        }
        ACTION_REFUND_WITHDRAW => {
            require_len(action, len, REFUND_WITHDRAW_CLAIM_MESSAGE_LEN)?;
            Ok(ClaimRequest::RefundWithdraw {
                program_id,
                attestation_epoch,
                nonce: read_u64(payload, 58),
                amount: read_u64(payload, 66),
                destination: read_pubkey(payload, 74),
                reserve_mint: read_pubkey(payload, 106),
                obligation_index: read_u64(payload, 170),
                requester: read_pubkey(payload, 178),
            })
        }
        // The retired unrestricted operator withdrawal. Its on-chain
        // instruction now fails closed, so a signature over these bytes
        // authorizes nothing — but a signer asked for one is being asked
        // by tooling that predates the hardening, or by someone probing.
        // Refuse loudly either way rather than sign a harmless-but-
        // meaningless signature.
        ACTION_REBALANCE_WITHDRAW => {
            require_len(action, len, REBALANCE_WITHDRAW_CLAIM_MESSAGE_LEN)?;
            Err(PolicyError::UnknownAction { action })
        }
        other => Err(PolicyError::UnknownAction { action: other }),
    }
}

fn require_len(action: u8, actual: usize, expected: usize) -> Result<(), PolicyError> {
    if actual == expected {
        Ok(())
    } else {
        Err(PolicyError::ActionLengthMismatch {
            action,
            len: actual,
        })
    }
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::try_from(&data[offset..offset + 32]).expect("32 bytes is always a valid pubkey")
}

/// One custody domain's independently-held signing policy.
///
/// Every field here must be provisioned by that domain's own operators,
/// through that domain's own change process. Nothing in it may be derived
/// from, fetched from, or defaulted to anything the bridge host controls —
/// if it were, this whole module would be theatre.
#[derive(Debug, Clone)]
pub struct SignerPolicy {
    /// The one deployment this signer serves.
    pub program_id: Pubkey,
    /// The one reserve mint this signer serves.
    pub reserve_mint: Pubkey,
    /// Which action classes the credential presented on this request may
    /// authorize. The daemon's credential carries
    /// `[ActionClass::Settlement]` and nothing more.
    pub allowed_classes: Vec<ActionClass>,
    /// Treasury token accounts this domain has independently agreed to.
    /// Should mirror the on-chain `RebalancePolicy` allowlist, but must be
    /// configured separately — the point is that an attacker has to
    /// subvert both.
    pub allowed_treasuries: Vec<Pubkey>,
    /// This domain's own ceiling on a single reserve withdrawal, in the
    /// reserve mint's atomic units. The program enforces no amount bound
    /// of its own — the on-chain policy is the destination allowlist and
    /// nothing else — so this is the only ceiling standing between one
    /// approval and the whole reserve, and each signing domain must set
    /// it independently.
    pub max_withdrawal_amount: u64,
    /// SHA-256 parameter commitments for governance proposals this domain
    /// has approved out of band. Empty means "refuse all governance",
    /// which is the correct default: a governance proposal should be
    /// reviewed and entered here deliberately, then removed afterwards.
    pub approved_governance_commitments: Vec<[u8; 32]>,
}

impl SignerPolicy {
    /// The full decision. Returns the parsed request on approval so the
    /// caller can log exactly what it agreed to
    /// ([`ClaimRequest::summary`]) alongside the signature it issues.
    ///
    /// Order matters: identity checks come before class checks, and class
    /// checks before content checks, so the error a caller sees names the
    /// first and most fundamental reason the request was refused rather
    /// than an incidental one.
    pub fn evaluate(&self, payload: &[u8]) -> Result<ClaimRequest, PolicyError> {
        let request = parse_claim(payload)?;

        if request.program_id() != self.program_id {
            return Err(PolicyError::WrongProgram {
                expected: self.program_id,
                actual: request.program_id(),
            });
        }

        let class = request.class();
        if !self.allowed_classes.contains(&class) {
            return Err(PolicyError::ActionClassNotPermitted {
                requested: class,
                allowed: self.allowed_classes.clone(),
            });
        }

        match &request {
            ClaimRequest::Release { reserve_mint, .. } => {
                self.check_mint(*reserve_mint)?;
            }
            ClaimRequest::Completion { .. } => {}
            ClaimRequest::TreasuryWithdraw {
                amount,
                destination,
                reserve_mint,
                ..
            } => {
                self.check_mint(*reserve_mint)?;
                // THE check. An address this domain did not separately
                // agree to is refused no matter what else is true of the
                // request or who presented it.
                if !self.allowed_treasuries.contains(destination) {
                    return Err(PolicyError::DestinationNotAllowlisted {
                        destination: *destination,
                    });
                }
                self.check_amount(*amount)?;
            }
            ClaimRequest::RefundWithdraw {
                amount,
                reserve_mint,
                ..
            } => {
                self.check_mint(*reserve_mint)?;
                // A refund's destination is derived on chain from the
                // depositor's own obligation, so there is no list to check
                // it against. A thorough signer should additionally read
                // the obligation from its own RPC and confirm the
                // requester, amount and derived ATA agree — see
                // `docs/28-signer-policy.md`. The amount ceiling still
                // applies as a blunt backstop.
                self.check_amount(*amount)?;
            }
            ClaimRequest::Governance {
                params_commitment, ..
            } => {
                if !self
                    .approved_governance_commitments
                    .contains(params_commitment)
                {
                    return Err(PolicyError::GovernanceNotPreApproved {
                        commitment: *params_commitment,
                    });
                }
            }
        }

        Ok(request)
    }

    fn check_mint(&self, actual: Pubkey) -> Result<(), PolicyError> {
        if actual == self.reserve_mint {
            Ok(())
        } else {
            Err(PolicyError::WrongReserveMint {
                expected: self.reserve_mint,
                actual,
            })
        }
    }

    fn check_amount(&self, amount: u64) -> Result<(), PolicyError> {
        if amount <= self.max_withdrawal_amount {
            Ok(())
        } else {
            Err(PolicyError::AmountAboveCeiling {
                amount,
                ceiling: self.max_withdrawal_amount,
            })
        }
    }
}

#[cfg(test)]
mod tests;
