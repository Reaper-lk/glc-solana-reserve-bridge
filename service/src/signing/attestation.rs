//! Internal ed25519 attestation signer group (docs/02-trust-model.md,
//! docs/12-management-decisions.md item 1: 2-of-3 internal threshold
//! custody, **not** federation). Mirrors
//! [`crate::signing::goldcoin_vault`]'s independent-re-derivation
//! discipline, adapted to ed25519/Solana and to the two message families in
//! `glc_reserve_bridge_shared::claim`.
//!
//! # DEV/TEST KEY POSTURE ONLY
//!
//! [`DevAttestationSigner`] holds a plain in-memory `solana_sdk::signature::
//! Keypair` — this is a **non-production stand-in** for local development
//! and testing only (see IMPLEMENTATION_LOG.md). Production signing keys
//! must live in genuinely separate HSM/KMS custody domains
//! (docs/12-management-decisions.md item 2, a distinct later piece of
//! work).
//!
//! # Independent re-derivation, from two genuinely separate sources
//!
//! Each signer reconstructs the claim it signs from two independent reads:
//! this service's own [`Ledger`] (its own confirmed observation of
//! source-chain state) AND a live [`SolanaRpc`] read of the on-chain
//! `AttestationKeySet`/`BridgeConfig`/`WithdrawalObligation` (never a
//! cached or handed-in epoch, mint, or destination). This is what makes
//! constraint 3 ("never release/record based only on a requester's claim")
//! and constraint 4 ("verify source-chain state independently") concrete: a
//! signer is handed only a `request_id` and is structurally incapable of
//! being handed a pre-built message to blindly sign.

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use thiserror::Error;

use glc_reserve_bridge_shared::claim::{
    goldcoin_completion_message, release_claim_message, COMPLETION_MESSAGE_LEN,
    RELEASE_CLAIM_MESSAGE_LEN,
};

use crate::ledger::{Direction, Ledger, LedgerError, RequestState};
use crate::solana::accounts::{self, PROGRAM_ID};
use crate::solana::rpc::{SolanaRpc, SolanaRpcError};

/// Must match `programs/glc-reserve-bridge/src/constants.rs::PROTOCOL_VERSION`.
pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub enum AttestationError {
    #[error("bridge request {0} not found")]
    RequestNotFound(i64),
    #[error("bridge request {0} is not a GlcToSol request")]
    WrongDirectionForRelease(i64),
    #[error("bridge request {0} is not in SourceFinalized state (found {1:?}) — refusing to attest a release")]
    NotSourceFinalized(i64, RequestState),
    #[error(
        "bridge request {0} is missing its source Goldcoin txid/vout — cannot attest a release"
    )]
    MissingSourceBinding(i64),
    #[error("bridge request {0}'s recipient is not a 32-byte Solana pubkey")]
    InvalidRecipient(i64),
    #[error("bridge request {0} is not a SolToGlc request")]
    WrongDirectionForCompletion(i64),
    #[error(
        "bridge request {0} has no recorded Solana obligation index — cannot attest completion"
    )]
    MissingObligationIndex(i64),
    #[error("no Goldcoin payout record exists for request {0}")]
    PayoutNotFound(i64),
    #[error("bridge request {0}'s Goldcoin payout is not yet Confirmed (found state {1:?}) — refusing to attest completion")]
    PayoutNotConfirmed(i64, String),
    #[error("bridge request {0}'s Goldcoin payout is missing its mined txid/height — refusing to attest completion")]
    MissingPayoutMinedData(i64),
    #[error("on-chain account {0} is not yet initialized")]
    NotInitialized(Pubkey),
    #[error("on-chain WithdrawalObligation amount ({onchain}) does not match this service's recorded payout amount ({recorded}) for request {request_id} — refusing to attest")]
    ObligationAmountMismatch {
        request_id: i64,
        onchain: u64,
        recorded: u64,
    },
    #[error("ledger error: {0}")]
    Ledger(#[from] LedgerError),
    #[error("solana rpc error: {0}")]
    SolanaRpc(#[from] SolanaRpcError),
}

/// One internal attestation signer's ed25519 keypair.
///
/// # DEV/TEST KEY POSTURE ONLY — see module docs.
pub struct DevAttestationSigner {
    pub keypair: Keypair,
}

impl DevAttestationSigner {
    /// Generates a fresh, random, NON-PRODUCTION keypair. Never used for
    /// anything but local dev/test — see module docs.
    pub fn generate() -> Self {
        DevAttestationSigner {
            keypair: Keypair::new(),
        }
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }
}

/// Independently re-derives the `release_claim_message` for `request_id`
/// from the [`Ledger`] (source binding, amount, recipient — this service's
/// own confirmed observation of the Goldcoin deposit) and a live
/// [`SolanaRpc`] read (attestation epoch, reserve mint), then signs it.
/// Never accepts a pre-built message.
pub async fn independently_attest_release<R: SolanaRpc>(
    signer: &DevAttestationSigner,
    ledger: &Ledger,
    rpc: &R,
    request_id: i64,
) -> Result<(Pubkey, Signature, [u8; RELEASE_CLAIM_MESSAGE_LEN]), AttestationError> {
    let request = ledger
        .get_request(request_id)?
        .ok_or(AttestationError::RequestNotFound(request_id))?;
    if request.direction != Direction::GlcToSol {
        return Err(AttestationError::WrongDirectionForRelease(request_id));
    }
    if request.state != RequestState::SourceFinalized {
        return Err(AttestationError::NotSourceFinalized(
            request_id,
            request.state,
        ));
    }
    let txid = request
        .source_txid
        .ok_or(AttestationError::MissingSourceBinding(request_id))?;
    let vout = request
        .source_vout
        .ok_or(AttestationError::MissingSourceBinding(request_id))?;
    let recipient: [u8; 32] = request
        .recipient
        .clone()
        .try_into()
        .map_err(|_| AttestationError::InvalidRecipient(request_id))?;

    let key_set = fetch_attestation_key_set(rpc).await?;
    let config = fetch_bridge_config(rpc).await?;

    let message = release_claim_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        key_set.epoch,
        &txid,
        vout,
        request.amount_atomic,
        &recipient,
        &config.reserve_token_mint.to_bytes(),
    );
    let (pubkey, signature) = crate::solana::ed25519::sign(&signer.keypair, &message);
    Ok((pubkey, signature, message))
}

/// Independently re-derives the `goldcoin_completion_message` for
/// `request_id` from the [`Ledger`] (this service's own confirmed
/// observation that the Goldcoin payout mined and reached its required
/// confirmation depth) and a live [`SolanaRpc`] read of the
/// `WithdrawalObligation` (destination commitment, amount cross-check,
/// attestation epoch), then signs it.
pub async fn independently_attest_completion<R: SolanaRpc>(
    signer: &DevAttestationSigner,
    ledger: &Ledger,
    rpc: &R,
    request_id: i64,
) -> Result<(Pubkey, Signature, [u8; COMPLETION_MESSAGE_LEN]), AttestationError> {
    let request = ledger
        .get_request(request_id)?
        .ok_or(AttestationError::RequestNotFound(request_id))?;
    if request.direction != Direction::SolToGlc {
        return Err(AttestationError::WrongDirectionForCompletion(request_id));
    }
    let obligation_index = request
        .source_obligation_index
        .ok_or(AttestationError::MissingObligationIndex(request_id))?;

    let payout = ledger
        .get_goldcoin_payout(request_id)?
        .ok_or(AttestationError::PayoutNotFound(request_id))?;
    if payout.state != "Confirmed" && payout.state != "Completed" {
        return Err(AttestationError::PayoutNotConfirmed(
            request_id,
            payout.state,
        ));
    }
    let payout_txid = payout
        .txid
        .ok_or(AttestationError::MissingPayoutMinedData(request_id))?;
    let payout_height = payout
        .mined_height
        .ok_or(AttestationError::MissingPayoutMinedData(request_id))?;

    let key_set = fetch_attestation_key_set(rpc).await?;

    let obligation_pda = accounts::withdrawal_obligation_pda(obligation_index);
    let obligation_account = rpc
        .get_account(&obligation_pda)
        .await?
        .ok_or(AttestationError::NotInitialized(obligation_pda))?;
    let obligation = accounts::decode_withdrawal_obligation(&obligation_account.data)?;

    if obligation.amount != payout.payout_atomic {
        return Err(AttestationError::ObligationAmountMismatch {
            request_id,
            onchain: obligation.amount,
            recorded: payout.payout_atomic,
        });
    }

    let digest = Sha256::digest(&obligation.glc_address);
    let dest_commitment: [u8; 32] = digest.as_slice().try_into().unwrap();

    let message = goldcoin_completion_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        key_set.epoch,
        obligation_index,
        &payout_txid,
        payout_height as u64,
        payout.payout_atomic,
        &dest_commitment,
    );
    let (pubkey, signature) = crate::solana::ed25519::sign(&signer.keypair, &message);
    Ok((pubkey, signature, message))
}

pub(crate) async fn fetch_attestation_key_set<R: SolanaRpc>(
    rpc: &R,
) -> Result<accounts::AttestationKeySetSnapshot, AttestationError> {
    let pda = accounts::attestation_key_set_pda();
    let account = rpc
        .get_account(&pda)
        .await?
        .ok_or(AttestationError::NotInitialized(pda))?;
    Ok(accounts::decode_attestation_key_set(&account.data)?)
}

pub(crate) async fn fetch_bridge_config<R: SolanaRpc>(
    rpc: &R,
) -> Result<accounts::BridgeConfigSnapshot, AttestationError> {
    let pda = accounts::bridge_config_pda();
    let account = rpc
        .get_account(&pda)
        .await?
        .ok_or(AttestationError::NotInitialized(pda))?;
    Ok(accounts::decode_bridge_config(&account.data)?)
}

#[cfg(test)]
mod tests;
