//! The EVM transaction submitter: the single EOA that pays gas and
//! broadcasts, and the durable nonce manager that keeps its transactions
//! from colliding with each other.
//!
//! # The submitter is not a bridge authority
//!
//! It pays gas. It broadcasts. It authorizes nothing. Every value-moving
//! call carries a 2-of-3 EIP-712 quorum inside its calldata, verified by
//! the contract against its own signer set, and no authorized path in
//! `GlcRobinhoodBridge` reads `msg.sender`. A stolen submitter key can
//! waste gas and can refuse to broadcast — a denial of service — and can
//! do nothing else. It cannot move one atomic unit of GLC.
//!
//! # The nonce manager, and the property it exists to guarantee
//!
//! An EVM account's transactions are ordered by a strictly increasing
//! nonce. Two transactions sharing a nonce are competing REPLACEMENTS of
//! each other: at most one mines, and which one is the network's choice,
//! not this service's. So a nonce is not a number to compute — it is a
//! resource to OWN, durably, before anything is signed.
//!
//! The rules, each with the failure it prevents:
//!
//! | Rule | Prevents |
//! |---|---|
//! | Allocate from the ledger's own maximum, in the same transaction that stores it | two operations racing to the same nonce between two RPC round trips |
//! | Persist the nonce BEFORE signing, and the signed bytes BEFORE broadcasting | a crash leaving a broadcast nobody can find or re-send |
//! | A retry re-broadcasts the IDENTICAL bytes under the SAME nonce | a second transaction racing the first |
//! | `eth_getTransactionCount("pending")` is a FLOOR, never the allocator | a lagging replica handing out a nonce already in use |
//! | A replacement bumps the fee and keeps the nonce | the same |
//! | An uncertain broadcast NEVER reallocates | duplicate payouts |
//! | A REVERTED transaction is terminal, never auto-retried | spending gas repeatedly on a call whose precondition is false |
//!
//! ## Why "uncertain" never means "allocate a new nonce"
//!
//! This is the single most important rule in the module. When
//! `eth_sendRawTransaction` fails at the transport layer, the question
//! was never answered: the bytes may be in the node's mempool, may be
//! propagating, may already be mined, or may never have arrived. Every
//! one of those is consistent with the same observation.
//!
//! Allocating a fresh nonce and building a second transaction would mean
//! that in the "already arrived" case BOTH could mine — two payouts, two
//! settlements, two refunds. Re-broadcasting the identical bytes has no
//! such case: whatever their fate, they are one transaction. `already
//! known` and `nonce too low` are therefore routine, expected answers,
//! not errors.
//!
//! ## What resolves an uncertain broadcast
//!
//! Not a guess, and not a timeout. Three sources of truth, in order of
//! strength:
//!
//! 1. A receipt for the recorded transaction hash — direct evidence.
//! 2. The contract's own replay guard,
//!    `requestExecuted(action, requestId)` — evidence the operation
//!    happened, whichever transaction carried it. This survives a
//!    receipt aging out of a node's index, which is exactly the case a
//!    time-based heuristic gets wrong.
//! 3. `eth_getTransactionCount` moving past the nonce with no receipt and
//!    no replay-guard hit — evidence that SOMETHING else consumed the
//!    nonce. That is not a resolution; it is an incident, and it stops
//!    for a human.
//!
//! # Fees, and the envelope
//!
//! The envelope is configured and cross-checked against the chain
//! ([`super::preflight`]); this module signs whichever one it is told to
//! and never chooses. Fee VALUES are read live and bounded by
//! configuration: a node's suggestion is an input, the configured
//! ceiling is a hard cap, and a suggestion above the cap is a refusal to
//! build rather than a silent clamp — a transaction signed at a fee the
//! operator did not sanction is worse than one that does not go out.

use std::time::Duration;

use crate::evm::{
    EvmAddress, EvmSecretKey, EvmTxHash, EvmU256, SignedTransaction, TxEnvelope, TxFees,
    UnsignedTransaction,
};
use crate::ledger::{Ledger, LedgerError, RobinhoodTx, RobinhoodTxState};

use super::rpc::{EvmBroadcastOutcome, EvmCall, EvmRpcError, EvmSubmitRpc};
use super::settlement_config::RobinhoodSettlementConfig;

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error(transparent)]
    Ledger(#[from] LedgerError),
    #[error("Robinhood RPC while {doing}: {source}")]
    Rpc {
        doing: &'static str,
        #[source]
        source: EvmRpcError,
    },
    #[error(
        "the submitter {address} holds {balance} wei but this deployment requires at least \
         {required} before it will build a transaction — fund it before any Robinhood route is \
         opened"
    )]
    SubmitterUnderfunded {
        address: String,
        balance: String,
        required: u128,
    },
    #[error(
        "the node suggests a per-gas fee of {suggested} wei, above the configured ceiling of \
         {ceiling} — refusing to sign at a price the operator did not sanction"
    )]
    FeeAboveCeiling { suggested: u128, ceiling: u128 },
    #[error(
        "the configured envelope is {envelope} but the chain's latest block {evidence} — \
         refusing to sign a transaction the chain's fee market does not match"
    )]
    EnvelopeMismatch {
        envelope: &'static str,
        evidence: &'static str,
    },
    #[error(
        "gas estimation for this operation failed: {detail}. `eth_estimateGas` EXECUTES the \
         call, so a failure here means the real transaction would have reverted — caught before \
         a nonce was consumed"
    )]
    EstimateFailed { detail: String },
    #[error(
        "operation {id} has been replaced {attempts} times, at the configured budget of {max}; \
         a transaction that will not mine after that many fee bumps is not a fee problem"
    )]
    ReplacementBudgetExhausted { id: i64, attempts: i64, max: u32 },
    #[error("operation {id} is missing {what}, which every broadcast requires")]
    Incomplete { id: i64, what: &'static str },
    #[error(
        "operation {id}'s persisted transaction recovers to {recovered}, not the configured \
         submitter {expected} — this ledger's transaction was signed by a key this process does \
         not hold"
    )]
    SubmitterChanged {
        id: i64,
        expected: String,
        recovered: String,
    },
}

/// The gas-paying account and the policy it operates under.
///
/// Holds the key. Nothing else in this crate does, and nothing reads it
/// back out: [`EvmSecretKey`] has no accessor that yields its bytes and
/// its `Debug` is redacted.
pub struct Submitter {
    key: EvmSecretKey,
    address: EvmAddress,
    config: RobinhoodSettlementConfig,
}

impl std::fmt::Debug for Submitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The address is public and is what an operator needs; the key
        // has no rendering at all.
        f.debug_struct("Submitter")
            .field("address", &self.address)
            .field("envelope", &self.config.tx_envelope.as_str())
            .finish_non_exhaustive()
    }
}

/// Why a submitter key could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubmitterKeyError {
    #[error("the environment variable named by robinhood.settlement.submitter_key_env is not set")]
    Missing,
    #[error(
        "the submitter key environment variable is set but empty — an empty value is not a key, \
         and treating it as one would produce a deterministic, publicly-known account"
    )]
    Empty,
    #[error(
        "the submitter key environment variable does not hold 32 hex-encoded bytes (with or \
         without a 0x prefix)"
    )]
    Malformed,
    #[error("the submitter key environment variable does not hold a valid secp256k1 key")]
    InvalidKey,
    #[error(
        "the loaded submitter key controls {actual}, but robinhood.settlement.submitter_address \
         says {expected} — the configured address and the key in the environment are for \
         different accounts"
    )]
    AddressMismatch { expected: String, actual: String },
}

impl Submitter {
    /// Loads the key from the environment variable the config NAMES.
    ///
    /// # Nothing secret is ever echoed
    ///
    /// Every failure below reports what went wrong WITHOUT reproducing
    /// the value: not in an error message, not in a log line, not in a
    /// `Debug` rendering. A malformed key is "not 32 hex bytes", never
    /// "`0xdead…` is not 32 hex bytes". The variable's NAME is also not
    /// echoed, because an operator who has pasted a key into the name
    /// field would otherwise see it in their terminal.
    ///
    /// The loaded key's address is cross-checked against the
    /// independently configured `submitter_address`, so the wrong
    /// environment — the right shape of value from a different deployment
    /// — is a refusal to start rather than a stream of transactions from
    /// an account nobody funded.
    pub fn load(config: &RobinhoodSettlementConfig) -> Result<Submitter, SubmitterKeyError> {
        let raw =
            std::env::var(&config.submitter_key_env).map_err(|_| SubmitterKeyError::Missing)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SubmitterKeyError::Empty);
        }
        let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(SubmitterKeyError::Malformed);
        }
        let mut bytes = [0u8; 32];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| SubmitterKeyError::Malformed)?;
        }
        let key = EvmSecretKey::from_bytes(&bytes).map_err(|_| SubmitterKeyError::InvalidKey)?;
        let address = key.address();
        if address != config.submitter_address {
            return Err(SubmitterKeyError::AddressMismatch {
                expected: config.submitter_address.to_checksum_string(),
                actual: address.to_checksum_string(),
            });
        }
        Ok(Submitter {
            key,
            address,
            config: config.clone(),
        })
    }

    /// Builds one from an already-loaded key. Used by tests and by any
    /// future key source that is not an environment variable.
    pub fn from_key(
        key: EvmSecretKey,
        config: &RobinhoodSettlementConfig,
    ) -> Result<Submitter, SubmitterKeyError> {
        let address = key.address();
        if address != config.submitter_address {
            return Err(SubmitterKeyError::AddressMismatch {
                expected: config.submitter_address.to_checksum_string(),
                actual: address.to_checksum_string(),
            });
        }
        Ok(Submitter {
            key,
            address,
            config: config.clone(),
        })
    }

    pub fn address(&self) -> EvmAddress {
        self.address
    }

    pub fn config(&self) -> &RobinhoodSettlementConfig {
        &self.config
    }

    /// Reconciles this ledger's nonce cursor against the chain.
    ///
    /// Called at startup and whenever a broadcast reports a nonce
    /// disagreement. Records `eth_getTransactionCount(submitter,
    /// "pending")`, which the allocator then uses as a FLOOR — never as
    /// the allocator itself. See the module docs.
    pub async fn reconcile_nonce<R: EvmSubmitRpc>(
        &self,
        rpc: &R,
        ledger: &mut Ledger,
        now: i64,
    ) -> Result<u64, SubmitError> {
        let pending = rpc
            .pending_nonce(self.address)
            .await
            .map_err(|source| SubmitError::Rpc {
                doing: "reconciling the submitter's nonce",
                source,
            })?;
        Ok(ledger.record_evm_submitter_nonce(
            self.address.to_bytes(),
            self.config.chain_id.get(),
            pending,
            now,
        )?)
    }

    /// Verifies the submitter can afford what it is about to commit to.
    ///
    /// Checked BEFORE a nonce is allocated, so an underfunded submitter
    /// produces no half-built operation and burns no nonce. A nonce
    /// allocated to an operation that then cannot be signed is a gap in
    /// the sequence that every later transaction queues behind.
    pub async fn check_funding<R: EvmSubmitRpc>(&self, rpc: &R) -> Result<(), SubmitError> {
        let balance = rpc
            .balance(self.address)
            .await
            .map_err(|source| SubmitError::Rpc {
                doing: "reading the submitter's gas balance",
                source,
            })?;
        let sufficient = match balance.try_to_u128() {
            Ok(value) => value >= self.config.min_submitter_balance_wei,
            // A balance too large for a u128 is comfortably above any
            // configured minimum.
            Err(_) => true,
        };
        if sufficient {
            Ok(())
        } else {
            Err(SubmitError::SubmitterUnderfunded {
                address: self.address.to_checksum_string(),
                balance: balance.to_string(),
                required: self.config.min_submitter_balance_wei,
            })
        }
    }

    /// Reads the live fee parameters for the configured envelope, and
    /// cross-checks the envelope against the chain's own evidence.
    ///
    /// # The cross-check
    ///
    /// A block header carrying `baseFeePerGas` proves the chain has an
    /// EIP-1559 fee market; one without proves it does not. A configured
    /// envelope that disagrees is refused rather than used, because
    /// either direction of mismatch produces a transaction whose fee
    /// fields are not the ones that will be charged.
    ///
    /// This check runs on every fee read, not only at startup: a chain
    /// can activate London while this process is running, and a service
    /// that checked only at boot would keep signing the wrong envelope
    /// until it was restarted.
    ///
    /// # `bump` and replacements
    ///
    /// A replacement must offer a strictly higher fee or the node refuses
    /// it as `replacement underpriced`. `bump` is the replacement attempt
    /// count; each one raises the fee by 12.5% over the live suggestion,
    /// which is the minimum increment geth requires, compounded.
    pub async fn read_fees<R: EvmSubmitRpc>(
        &self,
        rpc: &R,
        bump: u32,
    ) -> Result<TxFees, SubmitError> {
        let base_fee = rpc
            .latest_base_fee()
            .await
            .map_err(|source| SubmitError::Rpc {
                doing: "reading the chain's base fee",
                source,
            })?;
        match (self.config.tx_envelope, base_fee) {
            (TxEnvelope::Eip1559, None) => {
                return Err(SubmitError::EnvelopeMismatch {
                    envelope: "eip1559",
                    evidence: "carries no baseFeePerGas, so this chain has no EIP-1559 fee \
                               market and cannot price a type-0x02 transaction",
                })
            }
            (TxEnvelope::Legacy, Some(_)) => {
                return Err(SubmitError::EnvelopeMismatch {
                    envelope: "legacy",
                    evidence: "carries a baseFeePerGas, so this chain HAS an EIP-1559 fee \
                               market and a legacy transaction's gasPrice is not the price it \
                               will actually be charged",
                })
            }
            _ => {}
        }

        let ceiling = self.config.max_fee_per_gas_wei;
        match self.config.tx_envelope {
            TxEnvelope::Legacy => {
                let suggested = rpc.gas_price().await.map_err(|source| SubmitError::Rpc {
                    doing: "reading the suggested gas price",
                    source,
                })?;
                let bumped = bump_fee(suggested, bump);
                if bumped > ceiling {
                    return Err(SubmitError::FeeAboveCeiling {
                        suggested: bumped,
                        ceiling,
                    });
                }
                Ok(TxFees::Legacy { gas_price: bumped })
            }
            TxEnvelope::Eip1559 => {
                let base = base_fee.unwrap_or(0);
                // A node that does not implement `eth_maxPriorityFeePerGas`
                // is answering "I have no suggestion", not "zero": the
                // configured tip is used, which is the operator's own
                // decision rather than a guessed one.
                let tip = rpc
                    .max_priority_fee_per_gas()
                    .await
                    .map_err(|source| SubmitError::Rpc {
                        doing: "reading the suggested priority fee",
                        source,
                    })?
                    .unwrap_or(self.config.priority_fee_wei)
                    .max(self.config.priority_fee_wei);
                let tip = bump_fee(tip, bump);
                // Headroom of 2x the current base fee, the conventional
                // allowance for the base fee rising while the transaction
                // waits: it can grow by at most 12.5% per block, so 2x
                // covers roughly six blocks of sustained increase.
                let max_fee = bump_fee(base.saturating_mul(2).saturating_add(tip), bump);
                if max_fee > ceiling {
                    return Err(SubmitError::FeeAboveCeiling {
                        suggested: max_fee,
                        ceiling,
                    });
                }
                Ok(TxFees::Eip1559 {
                    max_fee_per_gas: max_fee,
                    // The tip is paid out of the ceiling, so it can never
                    // exceed it.
                    max_priority_fee_per_gas: tip.min(max_fee),
                })
            }
        }
    }

    /// Estimates gas for a call, and turns a failing estimate into a
    /// typed refusal.
    ///
    /// `eth_estimateGas` EXECUTES the call against current state, so a
    /// revert here means the real transaction would have reverted too —
    /// caught before a nonce was consumed and before gas was spent. It is
    /// the cheapest possible dry run and it is deliberately not skipped
    /// in favour of a configured constant.
    pub async fn estimate_gas<R: EvmSubmitRpc>(
        &self,
        rpc: &R,
        call: &EvmCall,
    ) -> Result<u64, SubmitError> {
        match rpc.estimate_gas(self.address, call).await {
            Ok(estimate) => Ok(self.config.gas_limit_for(estimate)),
            // A definitive refusal is the dry run failing, which is
            // information rather than an outage.
            Err(EvmRpcError::Method { message, .. }) => {
                Err(SubmitError::EstimateFailed { detail: message })
            }
            Err(source) => Err(SubmitError::Rpc {
                doing: "estimating gas",
                source,
            }),
        }
    }

    /// Builds and signs the transaction for an operation whose nonce is
    /// already allocated.
    ///
    /// Takes the nonce as a parameter rather than allocating one: the
    /// allocation is a LEDGER transaction and belongs there, and a
    /// signing function that could allocate would be a signing function
    /// that could allocate twice.
    pub fn sign(
        &self,
        nonce: u64,
        gas_limit: u64,
        fees: TxFees,
        call: &EvmCall,
    ) -> SignedTransaction {
        UnsignedTransaction {
            chain_id: self.config.chain_id,
            nonce,
            gas_limit,
            to: call.to,
            // Every call this bridge makes is non-payable: the reserve is
            // an ERC-20, so no gas token ever moves with a call.
            value: EvmU256::ZERO,
            data: call.data.clone(),
            fees,
        }
        .sign(&self.key)
    }

    /// Re-broadcasts an operation's persisted bytes, exactly as stored.
    ///
    /// # The bytes are verified before they are sent
    ///
    /// A persisted raw transaction is re-signed by nobody: it is sent
    /// verbatim. Before that, its sender is RECOVERED and required to be
    /// this process's configured submitter. That catches a ledger
    /// restored alongside a rotated key — a case where re-broadcasting
    /// would send a transaction from an account this deployment no longer
    /// controls, under a nonce sequence that is not its own.
    ///
    /// # Every outcome is a definite answer, or an explicit uncertainty
    ///
    /// `Accepted`, `AlreadyKnown` and `NonceTooLow` are all reported to
    /// the caller as-is. None of them is an error and none of them is a
    /// reason to reallocate. A transport failure propagates as
    /// [`SubmitError::Rpc`], which the caller treats as "unresolved,
    /// retry the same bytes".
    pub async fn broadcast<R: EvmSubmitRpc>(
        &self,
        rpc: &R,
        tx: &RobinhoodTx,
    ) -> Result<EvmBroadcastOutcome, SubmitError> {
        let raw = tx.raw_tx.as_ref().ok_or(SubmitError::Incomplete {
            id: tx.id,
            what: "signed transaction bytes",
        })?;
        let stored_submitter = tx.submitter.ok_or(SubmitError::Incomplete {
            id: tx.id,
            what: "an allocated submitter and nonce",
        })?;
        if stored_submitter != self.address.to_bytes() {
            return Err(SubmitError::SubmitterChanged {
                id: tx.id,
                expected: self.address.to_checksum_string(),
                recovered: EvmAddress::from_bytes(stored_submitter).to_checksum_string(),
            });
        }
        rpc.send_raw_transaction(raw)
            .await
            .map_err(|source| SubmitError::Rpc {
                doing: "broadcasting a Robinhood transaction",
                source,
            })
    }

    /// Whether an in-flight operation is due for a fee-bumped
    /// replacement, and refuses once the budget is exhausted.
    pub fn replacement_due(&self, tx: &RobinhoodTx, now: i64) -> Result<bool, SubmitError> {
        if tx.state != RobinhoodTxState::Broadcast {
            return Ok(false);
        }
        let Some(last) = tx.last_broadcast_at else {
            return Ok(false);
        };
        if now - last < self.config.rebroadcast_after.as_secs() as i64 {
            return Ok(false);
        }
        if tx.replacement_attempts >= i64::from(self.config.max_replacements) {
            return Err(SubmitError::ReplacementBudgetExhausted {
                id: tx.id,
                attempts: tx.replacement_attempts,
                max: self.config.max_replacements,
            });
        }
        Ok(true)
    }

    /// The transaction hash this operation is currently tracking.
    pub fn tracked_hash(tx: &RobinhoodTx) -> Option<EvmTxHash> {
        tx.tx_hash.map(EvmTxHash::from_bytes)
    }
}

/// Raises `fee` by 12.5% per bump, compounded.
///
/// 12.5% is geth's `PriceBump` — the minimum increment a replacement must
/// offer over the transaction it replaces, or the node refuses it as
/// `replacement underpriced`. Compounding it per attempt means each
/// successive replacement clears the previous one rather than the
/// original.
///
/// Saturating: a fee large enough to overflow `u128` is already far above
/// any configured ceiling, and the ceiling check that follows will refuse
/// it either way.
pub(crate) fn bump_fee(fee: u128, bumps: u32) -> u128 {
    let mut value = fee;
    for _ in 0..bumps {
        // +12.5% == +1/8, computed as a division so it cannot overflow on
        // the multiply for any realistic fee.
        value = value.saturating_add(value / 8).saturating_add(1);
    }
    value
}

/// How long an operation's broadcast may go unresolved — no receipt, and
/// the contract's replay guard still saying it has not executed — before
/// the situation stops being "in flight" and becomes an incident.
///
/// # Why a constant and not configuration
///
/// This is not a fee-market parameter to tune per deployment. It is the
/// point at which "we cannot tell what happened" stops being a normal
/// pending state, and the correct answer at that point is always the
/// same: stop, and let a human look. Making it configurable would invite
/// raising it to silence an alarm.
///
/// Thirty minutes is far past any plausible inclusion delay for a
/// transaction that has been re-broadcast repeatedly at a rising fee, and
/// well short of a shift change.
pub const UNRESOLVED_BROADCAST_INCIDENT_SECS: i64 = 30 * 60;

/// Whether an in-flight operation has been unresolved long enough to stop
/// for a human.
pub fn broadcast_is_stale(tx: &RobinhoodTx, now: i64) -> bool {
    tx.first_broadcast_at
        .is_some_and(|first| now - first >= UNRESOLVED_BROADCAST_INCIDENT_SECS)
}

/// A one-line, operator-facing rendering of the fee fields actually
/// signed. Stored on the row so an operator can see what a transaction
/// committed to without decoding its RLP.
pub fn describe_fees(fees: TxFees) -> String {
    match fees {
        TxFees::Legacy { gas_price } => format!("legacy gasPrice={gas_price}"),
        TxFees::Eip1559 {
            max_fee_per_gas,
            max_priority_fee_per_gas,
        } => format!(
            "eip1559 maxFeePerGas={max_fee_per_gas} maxPriorityFeePerGas={max_priority_fee_per_gas}"
        ),
    }
}

/// The default timeout applied to every authorization signer call, when a
/// caller does not supply one. Mirrors the existing signer timeout
/// discipline (`signing::signers` module docs).
pub const DEFAULT_SIGNER_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests;
