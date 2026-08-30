//! Internal ed25519 attestation signer group (docs/02-trust-model.md,
//! docs/12-management-decisions.md item 1: 2-of-3 internal threshold
//! custody, **not** federation). Mirrors
//! [`crate::signing::goldcoin_vault`]'s independent-re-derivation
//! discipline, adapted to ed25519/Solana and to the two message families in
//! `glc_reserve_bridge_shared::claim`.
//!
//! # Signing is behind a trait — `DevAttestationSigner` is one implementation
//!
//! [`independently_attest_release`]/[`independently_attest_completion`]
//! take `&dyn `[`crate::signing::signers::AttestationSigner`] — never a
//! concrete signer type — so a real HSM/KMS-backed implementation is a
//! drop-in replacement, not a change to this attestation logic.
//! [`DevAttestationSigner`] holds a plain in-memory `solana_sdk::signature::
//! Keypair` — this is a **non-production stand-in** for local development
//! and testing only (see IMPLEMENTATION_LOG.md), used strictly in that role
//! from here on. Production signing keys must live in genuinely separate
//! HSM/KMS custody domains (docs/12-management-decisions.md item 2, a
//! distinct later piece of work).
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

use std::time::Duration;

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use thiserror::Error;

use glc_reserve_bridge_shared::claim::{
    goldcoin_completion_message, release_claim_message, COMPLETION_MESSAGE_LEN,
    RELEASE_CLAIM_MESSAGE_LEN,
};

use crate::amount_conversion::{self, ConversionError};
use crate::ledger::{Direction, Ledger, LedgerError, RequestState};
use crate::signing::signers::{AttestationSigner, BoxFut, SignerError};
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
    #[error(
        "bridge request {request_id}'s amount cannot be converted to the reserve mint's live \
         decimals: {source}"
    )]
    Conversion {
        request_id: i64,
        source: ConversionError,
    },
    #[error("attestation signer error: {0}")]
    Signer(#[from] SignerError),
}

/// Reads the reserve mint's `decimals` live from chain state — never
/// cached or handed in — the same independent-re-derivation discipline
/// this module's docs describe for epoch/mint/destination. Mint decimals
/// are immutable once set (no SPL/Token-2022 instruction can change them
/// after `InitializeMint`), so this is safe to call once per attestation
/// without worrying about staleness.
async fn fetch_reserve_mint_decimals<R: SolanaRpc>(
    rpc: &R,
    reserve_token_mint: &Pubkey,
) -> Result<u8, AttestationError> {
    let account = rpc
        .get_account(reserve_token_mint)
        .await?
        .ok_or(AttestationError::NotInitialized(*reserve_token_mint))?;
    Ok(accounts::decode_mint_basics(&account.data)?.decimals)
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

impl AttestationSigner for DevAttestationSigner {
    fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    fn sign_message<'a>(&'a self, message: &'a [u8]) -> BoxFut<'a, Result<Signature, SignerError>> {
        Box::pin(async move { Ok(self.keypair.sign_message(message)) })
    }
}

/// Independently re-derives the `release_claim_message` for `request_id`
/// from the [`Ledger`] (source binding, amount, recipient — this service's
/// own confirmed observation of the Goldcoin deposit) and a live
/// [`SolanaRpc`] read (attestation epoch, reserve mint), then signs it.
/// Never accepts a pre-built message.
///
/// `signer` is a trait object (`dyn AttestationSigner`) deliberately —
/// see the matching note on `signing::goldcoin_vault::independently_sign`.
/// `signer_timeout` bounds only the signing call itself, as defense in
/// depth (`signing::signers` module docs).
pub async fn independently_attest_release<R: SolanaRpc>(
    signer: &dyn AttestationSigner,
    ledger: &Ledger,
    rpc: &R,
    request_id: i64,
    signer_timeout: Duration,
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
    let solana_decimals = fetch_reserve_mint_decimals(rpc, &config.reserve_token_mint).await?;

    // `request.gross_amount_atomic` is canonical (matches exactly what was
    // verified against the real Goldcoin deposit); the claim this service
    // attests to, and what `release_from_reserve` actually transfers on
    // Solana, is the NET amount (after the bridge fee,
    // docs/20-bridge-fee.md) in the reserve mint's own live decimals.
    // Fee/net are always recomputed here from gross, never read from the
    // ledger's own stored fee/net columns — this only ever signs a value
    // it derived itself, and fails closed if the stored record has
    // somehow diverged from what the canonical formula produces.
    let fee_breakdown = amount_conversion::verify_fee_breakdown(
        request.gross_amount_atomic,
        request.fee_bps,
        request.fee_amount_atomic,
        request.net_amount_atomic,
    )
    .map_err(|source| AttestationError::Conversion { request_id, source })?;
    let solana_amount = fee_breakdown
        .net
        .to_solana(solana_decimals)
        .map_err(|source| AttestationError::Conversion { request_id, source })?
        .0;

    let message = release_claim_message(
        PROTOCOL_VERSION,
        &PROGRAM_ID.to_bytes(),
        key_set.epoch,
        &txid,
        vout,
        solana_amount,
        &recipient,
        &config.reserve_token_mint.to_bytes(),
    );
    let signature = sign_with_timeout(signer, &message, signer_timeout).await?;
    Ok((signer.pubkey(), signature, message))
}

/// Independently re-derives the `goldcoin_completion_message` for
/// `request_id` from the [`Ledger`] (this service's own confirmed
/// observation that the Goldcoin payout mined and reached its required
/// confirmation depth) and a live [`SolanaRpc`] read of the
/// `WithdrawalObligation` (destination commitment, amount cross-check,
/// attestation epoch), then signs it.
pub async fn independently_attest_completion<R: SolanaRpc>(
    signer: &dyn AttestationSigner,
    ledger: &Ledger,
    rpc: &R,
    request_id: i64,
    signer_timeout: Duration,
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
    let config = fetch_bridge_config(rpc).await?;
    let solana_decimals = fetch_reserve_mint_decimals(rpc, &config.reserve_token_mint).await?;

    let obligation_pda = accounts::withdrawal_obligation_pda(obligation_index);
    let obligation_account = rpc
        .get_account(&obligation_pda)
        .await?
        .ok_or(AttestationError::NotInitialized(obligation_pda))?;
    let obligation = accounts::decode_withdrawal_obligation(&obligation_account.data)?;

    // `obligation.amount` is the immutable, ground-truth Solana-native
    // GROSS amount the user actually deposited on-chain (widening to
    // canonical is always exact — Goldcoin has more decimals than the
    // canonical mint); `payout.payout_atomic` is this service's own record
    // of what it actually paid out on Goldcoin, which must be the NET
    // amount after the bridge fee (docs/20-bridge-fee.md), not the gross
    // deposit. Recomputed from the ground truth forward (never from the
    // ledger's own stored fee/net columns) so this check is anchored to
    // what actually happened on Solana and fails closed on any divergence
    // — including a stale/tampered `payout_atomic` record.
    let gross_canonical = amount_conversion::SolanaAtomic(obligation.amount)
        .to_canonical(solana_decimals)
        .map_err(|source| AttestationError::Conversion { request_id, source })?;
    let expected_payout_atomic =
        amount_conversion::compute_fee_at_bps(gross_canonical, request.fee_bps)
            .map_err(|source| AttestationError::Conversion { request_id, source })?
            .net
            .0;
    if expected_payout_atomic != payout.payout_atomic {
        return Err(AttestationError::ObligationAmountMismatch {
            request_id,
            onchain: expected_payout_atomic,
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
    let signature = sign_with_timeout(signer, &message, signer_timeout).await?;
    Ok((signer.pubkey(), signature, message))
}

/// Wraps a signer's `sign_message` call in `signer_timeout` as defense in
/// depth (see `signing::signers` module docs) and maps both the signer's
/// own error and a timed-out call into `AttestationError`.
async fn sign_with_timeout(
    signer: &dyn AttestationSigner,
    message: &[u8],
    signer_timeout: Duration,
) -> Result<Signature, AttestationError> {
    match tokio::time::timeout(signer_timeout, signer.sign_message(message)).await {
        Ok(Ok(signature)) => Ok(signature),
        Ok(Err(e)) => Err(AttestationError::Signer(e)),
        Err(_) => Err(AttestationError::Signer(SignerError::Timeout {
            identity: signer.pubkey().to_string(),
            millis: signer_timeout.as_millis() as u64,
        })),
    }
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
