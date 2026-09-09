//! Internal threshold-custody signing clients (docs/02-trust-model.md).
//! [`signers`] defines the `VaultSigner`/`AttestationSigner` traits every
//! settlement-signing call site depends on; [`goldcoin_vault`]/
//! [`attestation`] hold the dev/test in-memory implementations plus the
//! independent-re-derivation logic that calls whichever implementation is
//! configured; [`remote`] holds the production-capable HTTPS remote
//! signer implementation (docs/26-production-signer-deployment.md);
//! [`evm_kms`] holds the custody-domain SERVER half of that protocol's
//! `/v2/` EVM authorization endpoints, backed by AWS KMS — the
//! `glc-robinhood-kms-signer` binary.

pub mod attestation;
pub mod evm_kms;
pub mod evm_policy;
pub mod goldcoin_split;
pub mod goldcoin_vault;
pub mod policy;
pub mod remote;
pub mod signers;
