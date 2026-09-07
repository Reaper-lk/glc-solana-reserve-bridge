//! The resolved Robinhood SETTLEMENT configuration — everything the
//! service needs to build, authorize and broadcast a Robinhood
//! transaction, as distinct from [`super::config`], which is everything
//! it needs to watch the chain.
//!
//! # Two sections, deliberately not one
//!
//! `[robinhood.indexer]` can be present on its own: observing deposits is
//! useful before any route opens, and Phase E shipped exactly that.
//! `[robinhood.settlement]` cannot be present on its own — settling
//! requires observing — and its absence is what keeps a deployment
//! read-only. A process with no settlement section constructs no
//! submitter, loads no key, and has no code path that can reach
//! `eth_sendRawTransaction`.
//!
//! # Every field is required, and none has a default
//!
//! The same discipline [`super::config`] records, for the same reason: a
//! guessed value here is worse than an absent one. Two are worth calling
//! out specifically.
//!
//! ## The transaction envelope has no default, and is CROSS-CHECKED
//!
//! Whether Robinhood Chain has an EIP-1559 fee market is not established
//! by anything in this repository — no captured block header, no
//! `eth_feeHistory` response, no chain documentation. Guessing is not an
//! option: sending a type-`0x02` transaction to a pre-London node is a
//! transaction it does not recognise, and sending a legacy transaction to
//! a London node means the fee fields it actually uses are not the ones
//! that were signed.
//!
//! So `tx_envelope` is an explicit operator choice with no default AND it
//! is verified against the chain before anything is broadcast: a block
//! header carrying `baseFeePerGas` proves London is active, and one
//! without proves it is not ([`super::preflight`]). A configured value
//! that disagrees with the chain refuses to start. That is deliberately
//! stronger than "make it configurable" — a configurable value nothing
//! checks is still a guess, merely one an operator made instead of one
//! this code made.
//!
//! ## The submitter is not a bridge authority
//!
//! `submitter_key_env` names the environment variable holding the EOA's
//! private key. That key PAYS GAS and BROADCASTS. It authorizes nothing:
//! every value-moving call carries a 2-of-3 EIP-712 quorum inside its
//! calldata, verified by the contract against its own signer set, and the
//! contract never looks at `msg.sender` on any of those paths. A stolen
//! submitter key can waste gas and can decline to broadcast; it cannot
//! move one atomic unit of GLC.
//!
//! The two concepts are kept apart in the types as well as in prose: the
//! submitter key is an [`crate::evm::EvmSecretKey`] held by
//! [`super::submitter`], and the authorization signers are
//! [`super::signer::EvmAuthSigner`] trait objects that this process may
//! never hold key material for at all (in production they are remote
//! endpoints). Nothing converts one into the other.

use std::time::Duration;

use crate::evm::{EvmAddress, EvmChainId, TxEnvelope};

/// Fully resolved and validated; constructing one is
/// [`RobinhoodSettlementConfig::new`]'s job, and `crate::config` is its
/// only production caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RobinhoodSettlementConfig {
    /// The EIP-155 chain id every transaction is signed for. Must equal
    /// `[robinhood.indexer].chain_id` — cross-checked at resolve time, so
    /// a deployment cannot watch one network and settle on another.
    pub chain_id: EvmChainId,
    /// The deployed `GlcRobinhoodBridge`. Must equal the indexer's
    /// `bridge_contract`, for the same reason.
    pub bridge_contract: EvmAddress,
    /// Which transaction envelope to sign. No default; verified against
    /// the chain at preflight. See the module docs.
    pub tx_envelope: TxEnvelope,
    /// Name of the environment variable holding the submitter EOA's
    /// 32-byte private key, hex-encoded. NEVER the key itself — the same
    /// "config names the env var" discipline
    /// `signing::remote::RemoteSignerConfig::auth_token_env` uses.
    pub submitter_key_env: String,
    /// The address that key is expected to control, stated independently
    /// by the operator and cross-checked against the loaded key at
    /// startup.
    ///
    /// Redundant on purpose. It is what makes a wrong environment
    /// variable — the right shape of value, from the wrong deployment —
    /// a refusal to start rather than a stream of transactions from an
    /// account nobody funded.
    pub submitter_address: EvmAddress,
    /// The three addresses the deployed contract holds as its signer set,
    /// stated by the operator and cross-checked against the contract's
    /// own `signers()` at preflight.
    ///
    /// Carried so that a quorum can be checked LOCALLY — each collected
    /// signature is recovered and required to belong to this set — before
    /// a transaction is built. Without it, an unauthorized signature
    /// would only be caught by the contract, after gas was spent and a
    /// nonce consumed.
    pub authorized_signers: [EvmAddress; 3],
    /// How long an authorization is valid for once minted. Bound into
    /// every digest as `expiry`.
    ///
    /// Short enough that a signature which leaks cannot be replayed
    /// indefinitely, long enough to survive a slow chain and a retry or
    /// two. There is no "no expiry" option: the contract requires the
    /// field, and an unbounded authorization is a standing permission
    /// slip.
    pub authorization_ttl: Duration,
    /// Confirmations before an OUTBOUND transaction is treated as final
    /// and the bridge request is completed.
    ///
    /// Separate from the indexer's inbound `confirmation_depth` because
    /// the two answer different questions and a deployment may reasonably
    /// want them different: inbound depth decides when someone else's
    /// deposit is irreversible, outbound depth decides when this
    /// service's own payout is.
    pub required_confirmations: u64,
    /// Multiplier applied to `eth_estimateGas`, in percent (e.g. `130` =
    /// +30% headroom).
    ///
    /// An estimate is exact for the state it was taken against, and state
    /// moves: a route flag flipped, a rolling-limit bucket rolling over,
    /// or a first-write-to-a-zero-slot can all make the real execution
    /// cost more than the estimate did. Under-estimating means an
    /// out-of-gas revert, which consumes the nonce and the fee and
    /// achieves nothing.
    pub gas_limit_margin_percent: u64,
    /// Absolute ceiling on the gas limit any single transaction may
    /// carry, whatever the estimate said.
    ///
    /// A backstop against a malfunctioning or hostile RPC returning an
    /// enormous estimate: the gas limit is the maximum the submitter can
    /// be charged, so an unbounded one is an unbounded loss of gas token.
    pub max_gas_limit: u64,
    /// Absolute ceiling on the per-gas price this service will sign, in
    /// wei. Applies to `gasPrice` (legacy) and `maxFeePerGas` (1559)
    /// alike.
    pub max_fee_per_gas_wei: u128,
    /// Priority tip for the EIP-1559 envelope, in wei. Ignored under the
    /// legacy envelope, which has no such concept.
    pub priority_fee_wei: u128,
    /// How long an unmined broadcast may sit before a replacement is
    /// considered.
    ///
    /// A replacement reuses the SAME nonce and is a re-broadcast of the
    /// same operation at a higher fee — never a second operation. See
    /// [`super::submitter`].
    pub rebroadcast_after: Duration,
    /// How many fee-bumped replacements one operation may make before it
    /// stops and asks for a human.
    ///
    /// Bounded rather than unlimited: a transaction that will not mine
    /// after several bumps is not usually a fee problem, and escalating
    /// forever spends real gas token on a diagnosis nobody is reading.
    pub max_replacements: u32,
    /// Minimum gas-token balance the submitter must hold before this
    /// service will build a transaction, in wei.
    ///
    /// Checked BEFORE a nonce is allocated, so an underfunded submitter
    /// produces no half-built operation and burns no nonce.
    pub min_submitter_balance_wei: u128,
}

/// Why a `[robinhood.settlement]` section was refused.
///
/// Separate from `crate::config::ConfigError` so this type stays usable
/// (and testable) without the whole config-file machinery; `crate::config`
/// maps each variant onto its own `Invalid { field, detail }`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RobinhoodSettlementConfigError {
    #[error(
        "robinhood.settlement requires robinhood.indexer: a deployment that cannot observe \
         Robinhood deposits must not be able to settle them"
    )]
    IndexerMissing,
    #[error(
        "robinhood.settlement.chain_id ({settlement}) disagrees with robinhood.indexer.chain_id \
         ({indexer}) — one deployment watches and settles on exactly one network"
    )]
    ChainIdMismatch { settlement: u64, indexer: u64 },
    #[error(
        "robinhood.settlement.bridge_contract ({settlement}) disagrees with \
         robinhood.indexer.bridge_contract ({indexer}) — the contract that is watched must be \
         the contract that is settled against"
    )]
    BridgeContractMismatch { settlement: String, indexer: String },
    #[error("submitter_key_env must name an environment variable, not be empty")]
    EmptySubmitterKeyEnv,
    #[error(
        "submitter_key_env {name:?} looks like key material rather than a variable NAME — this \
         field names where the secret lives, and a secret must never appear in a config file"
    )]
    SubmitterKeyEnvLooksLikeASecret { name: String },
    #[error("submitter_address must not be the zero address")]
    ZeroSubmitterAddress,
    #[error(
        "the submitter address ({address}) is also listed as a bridge authorization signer — the \
         account that pays gas must not be one that authorizes transfers"
    )]
    SubmitterIsAnAuthorizedSigner { address: String },
    #[error("authorized_signers[{index}] must not be the zero address")]
    ZeroAuthorizedSigner { index: usize },
    #[error(
        "authorized_signers contains {address} twice — a 3-of-3 set with a duplicate is really a \
         2-address set, and its 2-of-3 quorum is really 1-of-2"
    )]
    DuplicateAuthorizedSigner { address: String },
    #[error(
        "the bridge contract ({address}) is also configured as the submitter or a signer — these \
         are three different kinds of account"
    )]
    BridgeContractReused { address: String },
    #[error("authorization_ttl_secs must be at least {min} and at most {max}, got {actual}")]
    AuthorizationTtlOutOfRange { actual: u64, min: u64, max: u64 },
    #[error(
        "required_confirmations must be at least 1 — a depth of 0 would complete a bridge \
         request before its payout was in any block"
    )]
    ZeroRequiredConfirmations,
    #[error(
        "gas_limit_margin_percent must be at least 100 (no headroom) and at most {max}, got \
         {actual} — a value below 100 would sign a gas limit BELOW the node's own estimate"
    )]
    GasMarginOutOfRange { actual: u64, max: u64 },
    #[error("max_gas_limit must be at least {min}, got {actual}")]
    MaxGasLimitTooLow { actual: u64, min: u64 },
    #[error("max_fee_per_gas_wei must be greater than zero")]
    ZeroMaxFee,
    #[error(
        "priority_fee_wei ({priority}) is not below max_fee_per_gas_wei ({max_fee}) — the tip is \
         paid out of the fee ceiling, so a tip at or above it leaves nothing for the base fee"
    )]
    PriorityFeeExceedsMaxFee { priority: u128, max_fee: u128 },
    #[error("rebroadcast_after_secs must be at least 1")]
    ZeroRebroadcastInterval,
    #[error("max_replacements must be at most {max}, got {actual}")]
    TooManyReplacements { actual: u32, max: u32 },
}

impl RobinhoodSettlementConfig {
    /// Below this an authorization can expire between being signed and
    /// being mined on a chain having a slow minute.
    pub const MIN_AUTHORIZATION_TTL_SECS: u64 = 60;
    /// Above this an authorization is effectively a standing permission
    /// slip. Twelve hours is generous for any recovery a human is
    /// actually watching.
    pub const MAX_AUTHORIZATION_TTL_SECS: u64 = 12 * 60 * 60;
    /// A margin above this is not headroom, it is an unbounded gas
    /// commitment wearing a percentage sign.
    pub const MAX_GAS_MARGIN_PERCENT: u64 = 300;
    /// The EVM's own floor for any transaction. A `max_gas_limit` below
    /// it could not pay for a bare transfer, let alone a contract call.
    pub const MIN_MAX_GAS_LIMIT: u64 = 21_000;
    /// Beyond a handful of fee bumps the problem is not the fee.
    pub const MAX_REPLACEMENT_CEILING: u32 = 10;

    /// Validates and constructs.
    ///
    /// `indexer` is taken as a parameter rather than resolved later
    /// because three of the checks below are agreements BETWEEN the two
    /// sections, and an agreement that is checked somewhere else is an
    /// agreement that can be forgotten.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        indexer: Option<&super::RobinhoodIndexerConfig>,
        chain_id: EvmChainId,
        bridge_contract: EvmAddress,
        tx_envelope: TxEnvelope,
        submitter_key_env: String,
        submitter_address: EvmAddress,
        authorized_signers: [EvmAddress; 3],
        authorization_ttl_secs: u64,
        required_confirmations: u64,
        gas_limit_margin_percent: u64,
        max_gas_limit: u64,
        max_fee_per_gas_wei: u128,
        priority_fee_wei: u128,
        rebroadcast_after_secs: u64,
        max_replacements: u32,
        min_submitter_balance_wei: u128,
    ) -> Result<RobinhoodSettlementConfig, RobinhoodSettlementConfigError> {
        use RobinhoodSettlementConfigError as E;

        let indexer = indexer.ok_or(E::IndexerMissing)?;
        if indexer.chain_id != chain_id {
            return Err(E::ChainIdMismatch {
                settlement: chain_id.get(),
                indexer: indexer.chain_id.get(),
            });
        }
        if indexer.bridge_contract != bridge_contract {
            return Err(E::BridgeContractMismatch {
                settlement: bridge_contract.to_checksum_string(),
                indexer: indexer.bridge_contract.to_checksum_string(),
            });
        }

        if submitter_key_env.trim().is_empty() {
            return Err(E::EmptySubmitterKeyEnv);
        }
        // A 64-hex-digit value (with or without the `0x`) in a field that
        // is supposed to hold a NAME is almost certainly the key itself,
        // pasted into the wrong place. Refusing is the only way to catch
        // it before it is committed to a config file.
        let candidate = submitter_key_env.trim().trim_start_matches("0x");
        if candidate.len() >= 64 && candidate.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(E::SubmitterKeyEnvLooksLikeASecret {
                name: "<redacted>".to_string(),
            });
        }

        if submitter_address.is_zero() {
            return Err(E::ZeroSubmitterAddress);
        }
        for (index, signer) in authorized_signers.iter().enumerate() {
            if signer.is_zero() {
                return Err(E::ZeroAuthorizedSigner { index });
            }
        }
        for i in 0..authorized_signers.len() {
            for j in (i + 1)..authorized_signers.len() {
                if authorized_signers[i] == authorized_signers[j] {
                    return Err(E::DuplicateAuthorizedSigner {
                        address: authorized_signers[i].to_checksum_string(),
                    });
                }
            }
        }
        // The separation this whole module exists to state, enforced
        // rather than only documented. The contract would not care — it
        // never reads `msg.sender` on an authorized path — but an
        // operator who has configured one key for both roles has
        // collapsed two custody domains into one without meaning to.
        if authorized_signers.contains(&submitter_address) {
            return Err(E::SubmitterIsAnAuthorizedSigner {
                address: submitter_address.to_checksum_string(),
            });
        }
        if bridge_contract == submitter_address || authorized_signers.contains(&bridge_contract) {
            return Err(E::BridgeContractReused {
                address: bridge_contract.to_checksum_string(),
            });
        }

        if !(Self::MIN_AUTHORIZATION_TTL_SECS..=Self::MAX_AUTHORIZATION_TTL_SECS)
            .contains(&authorization_ttl_secs)
        {
            return Err(E::AuthorizationTtlOutOfRange {
                actual: authorization_ttl_secs,
                min: Self::MIN_AUTHORIZATION_TTL_SECS,
                max: Self::MAX_AUTHORIZATION_TTL_SECS,
            });
        }
        if required_confirmations == 0 {
            return Err(E::ZeroRequiredConfirmations);
        }
        if !(100..=Self::MAX_GAS_MARGIN_PERCENT).contains(&gas_limit_margin_percent) {
            return Err(E::GasMarginOutOfRange {
                actual: gas_limit_margin_percent,
                max: Self::MAX_GAS_MARGIN_PERCENT,
            });
        }
        if max_gas_limit < Self::MIN_MAX_GAS_LIMIT {
            return Err(E::MaxGasLimitTooLow {
                actual: max_gas_limit,
                min: Self::MIN_MAX_GAS_LIMIT,
            });
        }
        if max_fee_per_gas_wei == 0 {
            return Err(E::ZeroMaxFee);
        }
        if priority_fee_wei >= max_fee_per_gas_wei {
            return Err(E::PriorityFeeExceedsMaxFee {
                priority: priority_fee_wei,
                max_fee: max_fee_per_gas_wei,
            });
        }
        if rebroadcast_after_secs == 0 {
            return Err(E::ZeroRebroadcastInterval);
        }
        if max_replacements > Self::MAX_REPLACEMENT_CEILING {
            return Err(E::TooManyReplacements {
                actual: max_replacements,
                max: Self::MAX_REPLACEMENT_CEILING,
            });
        }

        Ok(RobinhoodSettlementConfig {
            chain_id,
            bridge_contract,
            tx_envelope,
            submitter_key_env,
            submitter_address,
            authorized_signers,
            authorization_ttl: Duration::from_secs(authorization_ttl_secs),
            required_confirmations,
            gas_limit_margin_percent,
            max_gas_limit,
            max_fee_per_gas_wei,
            priority_fee_wei,
            rebroadcast_after: Duration::from_secs(rebroadcast_after_secs),
            max_replacements,
            min_submitter_balance_wei,
        })
    }

    /// The EIP-712 domain every authorization for this deployment is
    /// signed under.
    pub fn domain(&self) -> super::auth::BridgeDomain {
        super::auth::BridgeDomain::new(self.chain_id, self.bridge_contract)
    }

    /// The gas limit to sign, given the node's estimate: the estimate
    /// plus the configured margin, capped at [`Self::max_gas_limit`].
    ///
    /// Saturating rather than checked on the multiply: an estimate large
    /// enough to overflow `u64` when scaled is already far past the cap,
    /// and saturating lands on the cap, which is the same answer a
    /// checked path would have to produce anyway.
    pub fn gas_limit_for(&self, estimate: u64) -> u64 {
        let scaled = (estimate as u128).saturating_mul(self.gas_limit_margin_percent as u128) / 100;
        u64::try_from(scaled)
            .unwrap_or(u64::MAX)
            .min(self.max_gas_limit)
            // An estimate is never below the EVM's own floor in practice,
            // but a cap set at the floor must still produce a usable
            // limit rather than zero.
            .max(1)
    }
}

#[cfg(test)]
mod tests;
