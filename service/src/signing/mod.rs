//! Internal threshold-custody signing clients (docs/02-trust-model.md).
//! [`signers`] defines the `VaultSigner`/`AttestationSigner` traits every
//! settlement-signing call site depends on; [`goldcoin_vault`]/
//! [`attestation`] hold the dev/test in-memory implementations plus the
//! independent-re-derivation logic that calls whichever implementation is
//! configured; [`remote`] holds the production-capable HTTPS remote
//! signer implementation (docs/26-production-signer-deployment.md).

pub mod attestation;
pub mod goldcoin_vault;
pub mod remote;
pub mod signers;
