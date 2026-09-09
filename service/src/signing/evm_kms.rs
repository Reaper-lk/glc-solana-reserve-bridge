//! `glc-robinhood-kms-signer`: the custody domain's SERVER half of the
//! `/v2/` EVM authorization protocol that [`super::remote`] speaks as a
//! client.
//!
//! # What this module is, and what changed
//!
//! [`super::evm_policy`]'s docs say the HTTP shim in front of an HSM/KMS
//! "is each custody domain's own process, and this repository does not
//! ship it". It ships one now — for AWS KMS specifically — and everything
//! else in that sentence still holds:
//!
//! - The bridge client ([`super::remote::RemoteEvmAuthSigner`]) is
//!   untouched. This module implements the protocol that client already
//!   speaks; it does not extend, negotiate, or version it.
//! - The *decision* is still [`super::evm_policy::EvmSignerPolicy::
//!   evaluate`], reused verbatim. Nothing here re-implements EIP-712
//!   reconstruction, action/route validation, amount or TTL ceilings, or
//!   the `expected_digest` cross-check. A second copy of that logic is
//!   the one thing that could make the two sides disagree about what an
//!   authorization means.
//! - No private key is held, imported, or exported by this process. The
//!   key lives in AWS KMS and never leaves it; this binary holds a key
//!   ID and an IAM identity, and nothing else.
//!
//! # The one digest that may reach KMS
//!
//! [`super::evm_policy::EvmAuthDecision::digest`], and only that. It is
//! an OUTPUT of a successful `evaluate()`, recomputed from the request's
//! structured fields by [`crate::robinhood::auth`]'s single encoder.
//! `EvmAuthSignRequest::expected_digest` is never signed: `evaluate()`
//! already compares it against the digest it derived and refuses on
//! disagreement, which is the whole point of carrying it.
//!
//! There is deliberately **no endpoint that accepts bytes or a digest to
//! sign**. `POST /v1/sign` — the older, byte-oriented protocol
//! [`super::remote`] uses for Goldcoin sighashes and Solana claims — is
//! not implemented here and answers `404`. A Robinhood EVM authorization
//! signer that could be handed 32 arbitrary bytes would be the blind
//! oracle [`super::policy`]'s module docs exist to describe.
//!
//! # Why the identity check signs something
//!
//! The configured signer address is not merely trusted. At startup the
//! process asks KMS to sign one fixed, domain-separated challenge digest
//! ([`kms::startup_challenge_digest`]) and requires the recovered address
//! to equal the configured one ([`kms::prove_key_controls_address`]).
//! That proves control of the key rather than asserting it, and it does
//! so without a second SubjectPublicKeyInfo DER parser: the DER ->
//! `r || s || v` conversion the signing path uses is the same code, so
//! the startup check exercises it too. The challenge authorizes nothing —
//! it is not, and structurally cannot be, an EIP-712 typed-data hash (see
//! that function's docs).
//!
//! # Module map
//!
//! | module | what it holds |
//! |---|---|
//! | [`config`] | the `GLC_RHN_SIGNER_*` environment contract, and the independently-held [`super::evm_policy::EvmSignerPolicy`] it builds |
//! | [`der`] | AWS KMS's DER ECDSA signature -> the contract's 65-byte `r \|\| s \|\| v`, low-`s`, recovery byte proven not guessed |
//! | [`kms`] | the one-method signing abstraction the server depends on, plus the startup identity proof |
//! | [`aws`] | the only AWS-SDK-aware code in this crate |
//! | [`server`] | the two endpoints, bearer authentication, body cap, and the fail-closed error mapping |

pub mod aws;
pub mod config;
pub mod der;
pub mod kms;
pub mod server;

pub use config::{SignerConfig, SignerConfigError};
pub use der::{kms_der_to_evm_signature, ConvertedSignature, DerConversionError};
pub use kms::{
    prove_key_controls_address, startup_challenge_digest, IdentityProofError, KmsDigestSigner,
    KmsError,
};
pub use server::{serve, serve_on, SignerService, SystemClock, UnixClock};
