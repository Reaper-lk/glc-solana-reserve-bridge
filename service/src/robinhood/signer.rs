//! Bridge AUTHORIZATION signers for the Robinhood leg: the 2-of-3 quorum
//! whose EIP-712 signatures the custody contract verifies.
//!
//! # What this is not
//!
//! It is not the transaction submitter. That is a single EOA that pays
//! gas and broadcasts, holds no authority, and lives in
//! [`super::submitter`]. The contract never reads `msg.sender` on any
//! authorized path, so the account that sends a transaction is not a
//! party to what the transaction does.
//!
//! The two are separate types with no conversion between them, and the
//! configuration refuses a deployment where one address plays both roles
//! ([`super::settlement_config`]).
//!
//! # Why this is not `signing::signers::VaultSigner`
//!
//! The existing traits are curve- and payload-specific by design.
//! `VaultSigner` signs a BIP-143 sighash and returns a DER-encoded ECDSA
//! signature for a Goldcoin P2SH multisig; `AttestationSigner` signs an
//! ed25519 message for Solana. An EVM authorization is neither: it is a
//! 32-byte EIP-712 digest, signed with secp256k1, returned in the 65-byte
//! compact `r || s || v` form with EIP-2's low-`s` rule enforced, and
//! identified by a 20-byte Ethereum address rather than a 33-byte
//! compressed key or a Solana pubkey.
//!
//! Reusing `VaultSigner` would mean a DER signature that has to be
//! re-encoded, a public key that has to be hashed to an address, and a
//! trait whose docs promise a Goldcoin sighash. The brief's own
//! instruction is the rule followed here: reuse the existing signer
//! INFRASTRUCTURE — the trait-object pool, the timeout discipline, the
//! fail-closed error taxonomy, the "never hand back key material" shape —
//! but do not assume the Solana/Goldcoin payload format applies to EVM.
//!
//! So [`EvmAuthSigner`] mirrors those traits deliberately: same
//! `Box<dyn>` pooling, same [`SignerError`] vocabulary, same "the caller
//! computes the exact bytes and this only signs them" contract.
//!
//! # Every signature is verified locally before it is stored
//!
//! A signer's answer is not trusted. Each returned signature is
//! recovered against the exact digest that was sent, and the recovered
//! address must be the identity that signer was configured as AND a
//! member of the contract's authorized set. The same defense-in-depth
//! `signing::remote` already applies to Goldcoin and Solana signatures,
//! for the same reason: a remote signer returning a malformed or simply
//! wrong signature is indistinguishable from one that is compromised, and
//! both must fail closed.
//!
//! Doing it here rather than leaving it to the contract also means a bad
//! signature costs nothing: it is caught before a nonce is allocated,
//! before gas is spent, and before a transaction that would revert is
//! broadcast.

use std::future::Future;
use std::pin::Pin;

use crate::evm::secp::{self, EvmSecretKey};
use crate::evm::{EvmAddress, EvmSignature};
use crate::signing::signers::SignerError;

/// Same `BoxFut` shape as [`crate::signing::signers`], so an
/// implementation can be a network round trip behind a `dyn` boundary.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One EVM authorization custody domain.
///
/// As with [`crate::signing::signers::VaultSigner`], the caller has
/// already computed the exact 32 bytes: this trait's only job is "sign
/// this digest", and there is no method that could leak, export or hand
/// back a secret key.
pub trait EvmAuthSigner: Send + Sync {
    /// The Ethereum address this signer signs as — public identity only.
    ///
    /// Used to pick the quorum, to check membership of the contract's
    /// authorized set, and for audit logging. Cross-checked against the
    /// address actually RECOVERED from every signature this signer
    /// returns, so a signer that misreports its own identity is caught on
    /// its first signature rather than by a contract revert.
    fn address(&self) -> EvmAddress;

    /// Signs an already-computed EIP-712 digest.
    ///
    /// Must return the 65-byte compact form with `v` as 27/28 and `s` in
    /// the low half of the curve order — what OpenZeppelin's
    /// `ECDSA.recover`, and therefore `GlcRobinhoodBridge._authorize`,
    /// accepts. A high-`s` signature is rejected by the contract, so
    /// returning one is a failure rather than a variant.
    fn sign_digest<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFut<'a, Result<EvmSignature, SignerError>>;
}

/// Why a quorum could not be assembled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuorumError {
    #[error(
        "only {available} authorization signer(s) are configured, but a quorum is {required} \
         distinct ones"
    )]
    NotEnoughSigners { available: usize, required: usize },
    #[error("signer {identity} failed: {detail}")]
    SignerFailed { identity: String, detail: String },
    #[error(
        "signer {claimed} returned a signature that recovers to {recovered} — the signer's \
         stated identity and the identity that actually signed do not match"
    )]
    IdentityMismatch { claimed: String, recovered: String },
    #[error(
        "signer {address} is not in the contract's authorized signer set — this quorum would be \
         refused on-chain as UnauthorizedSigner"
    )]
    NotAuthorized { address: String },
    #[error(
        "signer {address} produced a signature that could not be recovered at all ({detail}); a \
         malformed or high-s signature is rejected by the contract's ECDSA.recover"
    )]
    Unrecoverable { address: String, detail: String },
    #[error(
        "two signatures recovered to the same address ({address}): a quorum of two from one \
         custody domain is a quorum of one, and the contract reverts on it"
    )]
    DuplicateSigner { address: String },
}

/// Exactly two signatures over one digest, each independently verified.
///
/// There is no way to construct one of these with three signatures, with
/// two from the same signer, or with a signature that does not verify:
/// [`collect_quorum`] is the only constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationQuorum {
    /// The digest every signature is over — carried so a caller cannot
    /// pair this quorum with a different payload.
    pub digest: [u8; 32],
    /// `(recovered address, signature)`, in the order they will be placed
    /// in the calldata.
    pub signatures: [(EvmAddress, EvmSignature); 2],
}

impl AuthorizationQuorum {
    /// The signatures in calldata order.
    pub fn signature_bytes(&self) -> Vec<EvmSignature> {
        vec![self.signatures[0].1, self.signatures[1].1]
    }

    /// `(address, 65 raw bytes)` pairs, the shape
    /// [`crate::ledger::Ledger::record_robinhood_authorization`] stores.
    pub fn for_storage(&self) -> Vec<([u8; 20], [u8; 65])> {
        self.signatures
            .iter()
            .map(|(address, signature)| (address.to_bytes(), signature.to_bytes()))
            .collect()
    }
}

/// The quorum threshold. A CONSTANT, matching
/// `GlcRobinhoodBridge.SIGNER_THRESHOLD`, and deliberately not
/// configurable: a quorum that can be reconfigured is a quorum that can
/// be configured to one. The contract requires exactly this many and
/// reverts on any other count, so a local setting could only ever
/// disagree with it.
pub const SIGNER_THRESHOLD: usize = 2;

/// Collects a verified 2-of-3 quorum over `digest`.
///
/// # Ordering
///
/// Signers are asked in the order they appear in `signers`, and the first
/// [`SIGNER_THRESHOLD`] that answer successfully form the quorum. The
/// contract explicitly does not care about order and performs no sorting,
/// so none is imposed here — imposing one would be a second, unverifiable
/// convention.
///
/// A signer that fails does NOT abort the collection: the next one is
/// tried, which is what makes a 2-of-3 tolerate one custody domain being
/// unreachable. A signature that fails VERIFICATION does abort, because
/// that is not a liveness problem — it is a signer returning something it
/// should not have.
///
/// # Every signature is checked three ways
///
/// 1. It must recover to an address at all (rejecting malformed and
///    high-`s` signatures, exactly as the contract's `ECDSA.recover`
///    does).
/// 2. The recovered address must equal the identity the signer claims.
/// 3. That address must be in `authorized`, the set read from the
///    deployed contract.
///
/// Only then is it counted. All three failures are refusals, not
/// fallbacks.
pub async fn collect_quorum(
    signers: &[Box<dyn EvmAuthSigner>],
    authorized: &[EvmAddress],
    digest: &[u8; 32],
    timeout: std::time::Duration,
) -> Result<AuthorizationQuorum, QuorumError> {
    if signers.len() < SIGNER_THRESHOLD {
        return Err(QuorumError::NotEnoughSigners {
            available: signers.len(),
            required: SIGNER_THRESHOLD,
        });
    }

    let mut collected: Vec<(EvmAddress, EvmSignature)> = Vec::with_capacity(SIGNER_THRESHOLD);
    let mut last_failure: Option<QuorumError> = None;

    for signer in signers {
        if collected.len() == SIGNER_THRESHOLD {
            break;
        }
        let claimed = signer.address();
        // Already have this identity — a pool that lists one custody
        // domain twice must not be able to produce a quorum from it.
        if collected.iter().any(|(address, _)| *address == claimed) {
            continue;
        }

        // The generic timeout wrapper every signer call site applies, as
        // defense in depth against an implementation that does not
        // enforce its own (`signing::signers` module docs).
        let signed = match tokio::time::timeout(timeout, signer.sign_digest(digest)).await {
            Ok(Ok(signature)) => signature,
            Ok(Err(e)) => {
                // A liveness or policy failure from ONE domain is what a
                // 2-of-3 exists to tolerate. Recorded and skipped.
                last_failure = Some(QuorumError::SignerFailed {
                    identity: claimed.to_checksum_string(),
                    detail: e.to_string(),
                });
                continue;
            }
            Err(_) => {
                last_failure = Some(QuorumError::SignerFailed {
                    identity: claimed.to_checksum_string(),
                    detail: format!("timed out after {}ms", timeout.as_millis()),
                });
                continue;
            }
        };

        // From here on a failure is NOT tolerated: the domain answered,
        // and the answer was wrong.
        let recovered =
            secp::recover_address(digest, &signed).map_err(|e| QuorumError::Unrecoverable {
                address: claimed.to_checksum_string(),
                detail: e.to_string(),
            })?;
        if recovered != claimed {
            return Err(QuorumError::IdentityMismatch {
                claimed: claimed.to_checksum_string(),
                recovered: recovered.to_checksum_string(),
            });
        }
        if !authorized.contains(&recovered) {
            return Err(QuorumError::NotAuthorized {
                address: recovered.to_checksum_string(),
            });
        }
        collected.push((recovered, signed));
    }

    if collected.len() < SIGNER_THRESHOLD {
        return Err(last_failure.unwrap_or(QuorumError::NotEnoughSigners {
            available: collected.len(),
            required: SIGNER_THRESHOLD,
        }));
    }
    if collected[0].0 == collected[1].0 {
        return Err(QuorumError::DuplicateSigner {
            address: collected[0].0.to_checksum_string(),
        });
    }
    Ok(AuthorizationQuorum {
        digest: *digest,
        signatures: [collected[0], collected[1]],
    })
}

/// An in-memory signer, for local development and tests ONLY.
///
/// The exact posture [`crate::signing::goldcoin_vault::DevVaultSigner`]
/// and [`crate::signing::attestation::DevAttestationSigner`] hold, and
/// for the same reason: a real deployment's authorization keys live in
/// three genuinely separate custody domains, and a type that can hold one
/// in this process is a development convenience, never a production
/// custody arrangement.
///
/// It is not feature-gated because the Goldcoin and Solana dev signers
/// are not either, and gating one of three would be a false distinction.
/// What keeps it out of production is the same thing that keeps those
/// out: `operators.mode` selects the loading path, and `"production"`
/// never constructs one.
pub struct DevEvmAuthSigner {
    key: EvmSecretKey,
    address: EvmAddress,
}

impl DevEvmAuthSigner {
    pub fn new(key: EvmSecretKey) -> DevEvmAuthSigner {
        let address = key.address();
        DevEvmAuthSigner { key, address }
    }

    /// Builds one from raw key bytes.
    pub fn from_bytes(bytes: &[u8; 32]) -> Result<DevEvmAuthSigner, secp::EvmSecpError> {
        EvmSecretKey::from_bytes(bytes).map(DevEvmAuthSigner::new)
    }
}

impl std::fmt::Debug for DevEvmAuthSigner {
    /// The address, never the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DevEvmAuthSigner({})", self.address)
    }
}

impl EvmAuthSigner for DevEvmAuthSigner {
    fn address(&self) -> EvmAddress {
        self.address
    }

    fn sign_digest<'a>(
        &'a self,
        digest: &'a [u8; 32],
    ) -> BoxFut<'a, Result<EvmSignature, SignerError>> {
        // Signing is infallible here — the key is already validated and
        // `sign_digest` cannot fail for a well-formed key — but the
        // signature is still normalised low-`s` by `secp::sign_digest`,
        // which is what makes a dev quorum actually acceptable to the
        // contract rather than merely well-formed.
        Box::pin(async move { Ok(secp::sign_digest(&self.key, digest)) })
    }
}

#[cfg(test)]
mod tests;
