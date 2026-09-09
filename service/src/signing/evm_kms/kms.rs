//! The one-method signing abstraction the server depends on, and the
//! startup proof that the configured key really controls the configured
//! address.
//!
//! # Why a trait
//!
//! Two reasons, in order of importance:
//!
//! 1. **The surface is deliberately one method wide.** A custody process
//!    whose internal signing abstraction can only do "sign these 32 bytes
//!    I already decided on" cannot grow an encrypt, export, or
//!    generate-key path by accident. [`KmsDigestSigner`] has no method
//!    that returns key material and no method that takes a message to be
//!    hashed.
//! 2. **The whole decision path is testable without AWS.** Every policy,
//!    authentication, conversion and identity test in this module runs
//!    against a fake implementation; `cargo test` never makes a network
//!    call, never needs credentials, and never depends on a KMS key
//!    existing.
//!
//! The trait is NOT a portability layer: [`super::aws`] is the only
//! implementation shipped, and its docs record why AWS KMS's `MessageType
//! = DIGEST` is the only correct call shape here.

use std::future::Future;
use std::pin::Pin;

use crate::evm::keccak::keccak256;
use crate::evm::EvmAddress;

use super::der::{kms_der_to_evm_signature, DerConversionError};

/// Same shape as [`crate::robinhood::signer::BoxFut`], so an
/// implementation can be a network round trip behind a `dyn` boundary.
pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a signing backend did not produce a signature.
///
/// Split by whether retrying could plausibly help, because that is the
/// distinction the HTTP layer turns into a status code and the bridge
/// client turns into [`crate::signing::signers::SignerError::Unavailable`]
/// (retriable next tick) versus a hard refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KmsError {
    /// The backend could not be reached, timed out, or is throttling.
    #[error("the signing backend is unavailable: {0}")]
    Unavailable(String),
    /// The backend answered, and the answer was "no" — a permissions
    /// failure, a disabled key, an unsupported algorithm. Retrying the
    /// same request produces the same answer.
    #[error("the signing backend refused the request: {0}")]
    Refused(String),
    /// A `200`-shaped answer with nothing usable in it.
    #[error("the signing backend returned no signature material")]
    EmptySignature,
    /// The key exists but is not one that can produce EVM-verifiable
    /// secp256k1 signatures at all.
    #[error("the configured key is not usable for EVM authorization signing: {0}")]
    WrongKeyShape(String),
}

impl KmsError {
    /// A short, stable, secret-free category for log lines and metrics.
    /// Deliberately not the message: messages are for humans reading one
    /// incident, categories are for counting.
    pub fn category(&self) -> &'static str {
        match self {
            KmsError::Unavailable(_) => "unavailable",
            KmsError::Refused(_) => "refused",
            KmsError::EmptySignature => "empty_signature",
            KmsError::WrongKeyShape(_) => "wrong_key_shape",
        }
    }

    /// Whether a later attempt could plausibly succeed. Drives the `5xx`
    /// the HTTP layer returns.
    pub fn is_retriable(&self) -> bool {
        matches!(self, KmsError::Unavailable(_))
    }
}

/// One asymmetric signing backend, addressed by digest.
///
/// There is no method here that hashes, that takes a message, that
/// exports a key, or that names an algorithm — the implementation owns
/// all of that, and a caller therefore cannot ask for a different one.
pub trait KmsDigestSigner: Send + Sync {
    /// A public, non-secret label for logs: the key id, ARN or alias.
    fn key_label(&self) -> &str;

    /// Signs `digest` exactly as given, returning the backend's raw DER
    /// ECDSA signature. Conversion to the EVM's compact form — including
    /// proving the recovery byte — is [`super::der`]'s job, not the
    /// backend's.
    fn sign_digest<'a>(&'a self, digest: &'a [u8; 32]) -> BoxFut<'a, Result<Vec<u8>, KmsError>>;
}

// --------------------------------------------------- the startup challenge --

/// The fixed, domain-separated preimage of the startup identity
/// challenge.
///
/// It exists so the process can PROVE, at startup, that the configured
/// KMS key controls the configured EVM address, instead of taking the
/// operator's word for it. The alternative — `kms:GetPublicKey` plus a
/// hand-written SubjectPublicKeyInfo DER parser — would add a second,
/// separately-fallible parser for exactly the same fact.
pub const STARTUP_CHALLENGE_DOMAIN: &[u8] =
    b"glc.reserve-bridge.robinhood.kms-signer.identity-challenge.v1";

/// The 32 bytes the startup check signs.
///
/// # This can never be an authorization
///
/// It is `keccak256` of an ASCII domain string. Every digest this signer
/// will ever be asked to authorize is an EIP-712 typed-data hash, i.e.
/// `keccak256(0x19 || 0x01 || domainSeparator || structHash)` — a
/// 66-byte preimage whose first two bytes are `0x19 0x01`
/// ([`crate::evm::eip712`]). The two preimages differ in length and in
/// their first byte, so a startup-challenge signature is not a signature
/// over any authorization, present or future, and finding a collision
/// would mean breaking keccak-256.
///
/// The signature it produces is discarded after the address is recovered
/// from it. It authorizes nothing, moves nothing, and is not stored.
pub fn startup_challenge_digest() -> [u8; 32] {
    keccak256(STARTUP_CHALLENGE_DOMAIN)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityProofError {
    #[error("the startup identity challenge could not be signed: {0}")]
    Kms(#[from] KmsError),
    #[error(
        "the startup identity challenge signature could not be converted: {0}. This service will \
         not start without proof that the configured key controls the configured address"
    )]
    Conversion(#[from] DerConversionError),
}

/// Proves that `kms`'s key controls `expected`.
///
/// Signs [`startup_challenge_digest`] and requires the recovered address
/// to equal `expected`. The proof is the conversion itself:
/// [`kms_der_to_evm_signature`] only returns a signature when exactly one
/// recovery byte recovers to the address it was given, so a key that does
/// not control `expected` produces [`DerConversionError::NoRecoveryMatch`]
/// rather than a signature.
///
/// Called once, before the listener is bound. A failure here is a startup
/// failure, never a warning: serving `GET /v2/evm-identity` with an
/// address the process cannot actually sign as would let the bridge
/// build a quorum around a signer that will never contribute to it.
pub async fn prove_key_controls_address(
    kms: &dyn KmsDigestSigner,
    expected: EvmAddress,
) -> Result<(), IdentityProofError> {
    let digest = startup_challenge_digest();
    let der = kms.sign_digest(&digest).await?;
    // The signature is used for exactly one thing and then dropped. It is
    // not logged: it proves nothing to a reader that the recovered
    // address does not already state.
    let _converted = kms_der_to_evm_signature(&der, &digest, expected)?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod fake {
    //! A deterministic in-process signing backend for the tests.
    //!
    //! Real secp256k1 signing over a real key — not a canned byte string
    //! — so the DER parsing, low-`s` normalisation and recovery-byte
    //! proof in [`super::super::der`] are exercised for what they are,
    //! not mocked around.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use libsecp256k1::{Message, SecretKey};

    use super::{BoxFut, KmsDigestSigner, KmsError};
    use crate::evm::secp::EvmSecretKey;
    use crate::evm::EvmAddress;

    /// What the fake should do when asked to sign.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum FakeBehaviour {
        /// A normal, canonical DER signature.
        SignNormally,
        /// The malleable high-`s` twin of the same signature — what a
        /// backend that does not normalise would return.
        SignHighS,
        /// Structurally valid DER from a DIFFERENT key.
        SignWithWrongKey,
        /// Bytes that are not DER at all.
        ReturnGarbage,
        /// A backend-side failure.
        Fail(KmsError),
    }

    pub(crate) struct FakeKms {
        secret: SecretKey,
        wrong_secret: SecretKey,
        label: String,
        behaviour: Mutex<FakeBehaviour>,
        calls: AtomicUsize,
        /// Every digest the fake was asked to sign, in order — so a test
        /// can assert WHICH digest reached the backend, not merely that
        /// one did.
        signed: Mutex<Vec<[u8; 32]>>,
    }

    impl FakeKms {
        pub(crate) fn new(seed: u8) -> FakeKms {
            FakeKms {
                secret: secret_key(seed),
                wrong_secret: secret_key(seed.wrapping_add(0x40)),
                label: format!("alias/fake-signer-{seed:02x}"),
                behaviour: Mutex::new(FakeBehaviour::SignNormally),
                calls: AtomicUsize::new(0),
                signed: Mutex::new(Vec::new()),
            }
        }

        pub(crate) fn address(&self) -> EvmAddress {
            EvmSecretKey::from_bytes(&secret_bytes(&self.secret))
                .expect("a valid secret key")
                .address()
        }

        /// The address the `SignWithWrongKey` behaviour signs as.
        pub(crate) fn wrong_address(&self) -> EvmAddress {
            EvmSecretKey::from_bytes(&secret_bytes(&self.wrong_secret))
                .expect("a valid secret key")
                .address()
        }

        pub(crate) fn set_behaviour(&self, behaviour: FakeBehaviour) {
            *self.behaviour.lock().expect("not poisoned") = behaviour;
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        pub(crate) fn signed_digests(&self) -> Vec<[u8; 32]> {
            self.signed.lock().expect("not poisoned").clone()
        }
    }

    fn secret_key(seed: u8) -> SecretKey {
        let mut bytes = [1u8; 32];
        bytes[31] = seed.max(1);
        bytes[0] = seed.wrapping_mul(3).max(1);
        SecretKey::parse(&bytes).expect("a valid secret key")
    }

    fn secret_bytes(key: &SecretKey) -> [u8; 32] {
        key.serialize()
    }

    /// DER-encodes an `(r, s)` pair by hand.
    ///
    /// Needed because `libsecp256k1`'s own `serialize_der` will happily
    /// encode whatever `s` the `Signature` holds, but constructing the
    /// high-`s` twin means negating the scalar first — see
    /// [`FakeBehaviour::SignHighS`].
    pub(crate) fn der_encode(signature: &libsecp256k1::Signature) -> Vec<u8> {
        signature.serialize_der().as_ref().to_vec()
    }

    /// The malleable twin: `(r, n - s)`.
    pub(crate) fn negate_s(mut signature: libsecp256k1::Signature) -> libsecp256k1::Signature {
        signature.s = -signature.s;
        signature
    }

    impl KmsDigestSigner for FakeKms {
        fn key_label(&self) -> &str {
            &self.label
        }

        fn sign_digest<'a>(
            &'a self,
            digest: &'a [u8; 32],
        ) -> BoxFut<'a, Result<Vec<u8>, KmsError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.signed.lock().expect("not poisoned").push(*digest);
                let behaviour = self.behaviour.lock().expect("not poisoned").clone();
                let message = Message::parse(digest);
                match behaviour {
                    FakeBehaviour::SignNormally => {
                        let (signature, _) = libsecp256k1::sign(&message, &self.secret);
                        Ok(der_encode(&signature))
                    }
                    FakeBehaviour::SignHighS => {
                        let (mut signature, _) = libsecp256k1::sign(&message, &self.secret);
                        signature.normalize_s();
                        Ok(der_encode(&negate_s(signature)))
                    }
                    FakeBehaviour::SignWithWrongKey => {
                        let (signature, _) = libsecp256k1::sign(&message, &self.wrong_secret);
                        Ok(der_encode(&signature))
                    }
                    FakeBehaviour::ReturnGarbage => Ok(vec![0xde, 0xad, 0xbe, 0xef]),
                    FakeBehaviour::Fail(e) => Err(e),
                }
            })
        }
    }
}
