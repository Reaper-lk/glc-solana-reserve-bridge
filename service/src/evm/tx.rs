//! EVM transaction envelopes: building, signing, and hashing the raw
//! transaction the submitter broadcasts.
//!
//! # Two envelopes, and why BOTH are implemented
//!
//! Robinhood Chain's fee-market behaviour is not established by anything
//! in this repository. A chain that has activated the London fork accepts
//! EIP-1559 (type `0x02`) transactions and exposes a `baseFeePerGas` on
//! every block header; a chain that has not accepts only the legacy
//! (EIP-155) envelope and has no base fee. Sending the wrong one is not a
//! degraded outcome — it is a transaction the node refuses, or worse, one
//! it accepts with fee fields it silently ignores.
//!
//! The Phase F brief's instruction on this point is explicit: determine
//! it from evidence, or make it configurable rather than guess. There is
//! no evidence in this repository — no captured block header, no
//! `eth_feeHistory` response, no chain documentation — so BOTH envelopes
//! are implemented, the choice is an explicit operator configuration
//! value with NO default ([`super::super::robinhood::submitter`]), and
//! the choice is additionally CROSS-CHECKED against the live chain before
//! anything is broadcast: a header carrying `baseFeePerGas` proves
//! EIP-1559 is active, and a header without one proves it is not. A
//! configured choice that disagrees with the chain fails closed.
//!
//! That is deliberately stronger than "make it configurable". A
//! configurable value that nothing checks is still a guess — it is just a
//! guess an operator made instead of a guess this file made.
//!
//! # What is NOT implemented
//!
//! Access lists (EIP-2930, type `0x01`), blob transactions (EIP-4844,
//! type `0x03`), and set-code transactions (EIP-7702, type `0x04`). None
//! is needed to call a contract function, and a `0x02` transaction's
//! access list is always encoded as the empty list here — present because
//! the envelope requires the field, never populated.
//!
//! # Signing hash versus transaction hash
//!
//! Two different keccak digests, easily confused:
//!
//! - The **signing hash** is over the UNSIGNED payload and is what the
//!   secret key signs.
//! - The **transaction hash** is over the SIGNED payload — the same
//!   fields plus the signature — and is the id the chain knows the
//!   transaction by, the one `eth_getTransactionReceipt` takes.
//!
//! Both are produced by [`SignedTransaction`], from one construction, so
//! a caller cannot pair a hash from one payload with bytes from another.

use super::address::EvmAddress;
use super::chain_id::EvmChainId;
use super::hash::EvmTxHash;
use super::keccak::keccak256;
use super::rlp;
use super::secp::{self, EvmSecretKey};
use super::signature::EvmSignature;
use super::u256::EvmU256;

/// Which envelope a transaction is built in.
///
/// Deliberately has no `Default`. There is no safe default: guessing
/// legacy on a London chain overpays or underpays through a field the
/// node reinterprets, and guessing 1559 on a pre-London chain produces a
/// transaction type the node does not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxEnvelope {
    /// EIP-155 legacy: `[nonce, gasPrice, gasLimit, to, value, data, v, r, s]`
    /// where `v = recovery + chainId*2 + 35`.
    Legacy,
    /// EIP-1559 type `0x02`: `0x02 || rlp([chainId, nonce,
    /// maxPriorityFeePerGas, maxFeePerGas, gasLimit, to, value, data,
    /// accessList, yParity, r, s])`.
    Eip1559,
}

impl TxEnvelope {
    pub fn as_str(self) -> &'static str {
        match self {
            TxEnvelope::Legacy => "legacy",
            TxEnvelope::Eip1559 => "eip1559",
        }
    }

    /// Whether this envelope requires the chain to have a base fee.
    /// Used by the startup cross-check described in the module docs.
    pub fn requires_base_fee(self) -> bool {
        matches!(self, TxEnvelope::Eip1559)
    }
}

impl std::str::FromStr for TxEnvelope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "legacy" => Ok(TxEnvelope::Legacy),
            "eip1559" => Ok(TxEnvelope::Eip1559),
            other => Err(format!(
                "unknown transaction envelope {other:?} — expected \"legacy\" or \"eip1559\""
            )),
        }
    }
}

/// The fee parameters for one transaction, in whichever shape its
/// envelope takes.
///
/// A single enum rather than three `Option` fields, so a legacy
/// transaction cannot carry a `maxFeePerGas` that is silently dropped and
/// a 1559 transaction cannot carry a `gasPrice` that is silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxFees {
    Legacy {
        gas_price: u128,
    },
    Eip1559 {
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
    },
}

impl TxFees {
    pub fn envelope(self) -> TxEnvelope {
        match self {
            TxFees::Legacy { .. } => TxEnvelope::Legacy,
            TxFees::Eip1559 { .. } => TxEnvelope::Eip1559,
        }
    }

    /// The absolute worst-case wei this transaction can spend on gas, for
    /// the caller's own balance check and for logging. `gasLimit` times
    /// the highest per-gas price the envelope permits.
    pub fn max_cost_wei(self, gas_limit: u64) -> Option<u128> {
        let per_gas = match self {
            TxFees::Legacy { gas_price } => gas_price,
            TxFees::Eip1559 {
                max_fee_per_gas, ..
            } => max_fee_per_gas,
        };
        per_gas.checked_mul(u128::from(gas_limit))
    }
}

/// An unsigned transaction: everything except the signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsignedTransaction {
    pub chain_id: EvmChainId,
    pub nonce: u64,
    pub gas_limit: u64,
    /// Always `Some` for this bridge — every transaction it sends is a
    /// call to the bridge contract. Contract CREATION (a `None` `to`) is
    /// deliberately unrepresentable: this service never deploys anything,
    /// and an envelope that could encode a deployment is one a bug could
    /// turn into one.
    pub to: EvmAddress,
    /// Always zero for this bridge: the reserve token is an ERC-20, so no
    /// call this service makes is payable. Kept as a field because the
    /// envelope has one and encoding a constant implicitly would hide it.
    pub value: EvmU256,
    pub data: Vec<u8>,
    pub fees: TxFees,
}

impl UnsignedTransaction {
    /// The 32 bytes a signer signs.
    ///
    /// For a legacy transaction this is `keccak(rlp([nonce, gasPrice,
    /// gas, to, value, data, chainId, 0, 0]))` — EIP-155's trailing
    /// `(chainId, 0, 0)`, which is what makes a legacy signature
    /// non-replayable onto another chain. For a 1559 transaction it is
    /// `keccak(0x02 || rlp([...nine fields...]))`.
    pub fn signing_hash(&self) -> [u8; 32] {
        keccak256(&self.signing_payload())
    }

    fn signing_payload(&self) -> Vec<u8> {
        match self.fees {
            TxFees::Legacy { gas_price } => rlp::encode_list(&[
                rlp::encode_uint(self.nonce),
                rlp::encode_u256(EvmU256::from_u128(gas_price)),
                rlp::encode_uint(self.gas_limit),
                rlp::encode_bytes(self.to.as_bytes()),
                rlp::encode_u256(self.value),
                rlp::encode_bytes(&self.data),
                // EIP-155's replay protection: the chain id followed by
                // two empty items, standing in for the r/s a signed
                // payload would carry.
                rlp::encode_uint(self.chain_id.get()),
                rlp::encode_uint(0),
                rlp::encode_uint(0),
            ]),
            TxFees::Eip1559 {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => {
                let mut out = vec![0x02u8];
                out.extend_from_slice(&rlp::encode_list(&[
                    rlp::encode_uint(self.chain_id.get()),
                    rlp::encode_uint(self.nonce),
                    rlp::encode_u256(EvmU256::from_u128(max_priority_fee_per_gas)),
                    rlp::encode_u256(EvmU256::from_u128(max_fee_per_gas)),
                    rlp::encode_uint(self.gas_limit),
                    rlp::encode_bytes(self.to.as_bytes()),
                    rlp::encode_u256(self.value),
                    rlp::encode_bytes(&self.data),
                    // The access list. Always empty; see the module docs.
                    rlp::encode_list(&[]),
                ]));
                out
            }
        }
    }

    /// Signs with `key` and produces the broadcastable form.
    pub fn sign(self, key: &EvmSecretKey) -> SignedTransaction {
        let signing_hash = self.signing_hash();
        let signature = secp::sign_digest(key, &signing_hash);
        self.attach(signature)
    }

    /// Attaches an already-produced signature. Split from [`Self::sign`]
    /// so a remote/HSM-backed submitter key can be supported without this
    /// module ever seeing key material.
    pub fn attach(self, signature: EvmSignature) -> SignedTransaction {
        let raw = self.signed_payload(&signature);
        SignedTransaction {
            hash: EvmTxHash::from_bytes(keccak256(&raw)),
            raw,
            unsigned: self,
            signature,
        }
    }

    fn signed_payload(&self, signature: &EvmSignature) -> Vec<u8> {
        // Both envelopes strip leading zero bytes from r and s: they are
        // RLP integers, not fixed-width words, and a non-minimal encoding
        // changes the transaction hash.
        let r = trim_leading_zeros(signature.r());
        let s = trim_leading_zeros(signature.s());
        match self.fees {
            TxFees::Legacy { gas_price } => {
                // EIP-155: v = recovery + chainId*2 + 35. Computed in
                // u128 and only then narrowed, so a large chain id
                // cannot wrap into a plausible-looking small v.
                let v =
                    u128::from(signature.recovery_id()) + u128::from(self.chain_id.get()) * 2 + 35;
                rlp::encode_list(&[
                    rlp::encode_uint(self.nonce),
                    rlp::encode_u256(EvmU256::from_u128(gas_price)),
                    rlp::encode_uint(self.gas_limit),
                    rlp::encode_bytes(self.to.as_bytes()),
                    rlp::encode_u256(self.value),
                    rlp::encode_bytes(&self.data),
                    rlp::encode_u256(EvmU256::from_u128(v)),
                    rlp::encode_bytes(r),
                    rlp::encode_bytes(s),
                ])
            }
            TxFees::Eip1559 {
                max_fee_per_gas,
                max_priority_fee_per_gas,
            } => {
                let mut out = vec![0x02u8];
                out.extend_from_slice(&rlp::encode_list(&[
                    rlp::encode_uint(self.chain_id.get()),
                    rlp::encode_uint(self.nonce),
                    rlp::encode_u256(EvmU256::from_u128(max_priority_fee_per_gas)),
                    rlp::encode_u256(EvmU256::from_u128(max_fee_per_gas)),
                    rlp::encode_uint(self.gas_limit),
                    rlp::encode_bytes(self.to.as_bytes()),
                    rlp::encode_u256(self.value),
                    rlp::encode_bytes(&self.data),
                    rlp::encode_list(&[]),
                    // yParity, NOT an EIP-155 v: a typed transaction
                    // carries the chain id as its own field, so the
                    // recovery bit stands alone as 0 or 1.
                    rlp::encode_uint(u64::from(signature.recovery_id())),
                    rlp::encode_bytes(r),
                    rlp::encode_bytes(s),
                ]));
                out
            }
        }
    }
}

/// A signed, broadcastable transaction and the hash the chain will know
/// it by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    pub unsigned: UnsignedTransaction,
    pub signature: EvmSignature,
    /// The exact bytes for `eth_sendRawTransaction`.
    pub raw: Vec<u8>,
    /// `keccak256(raw)` — computed at construction from those same bytes,
    /// so the two can never describe different transactions.
    pub hash: EvmTxHash,
}

impl SignedTransaction {
    /// `0x`-prefixed hex of [`Self::raw`], the form
    /// `eth_sendRawTransaction` takes.
    pub fn raw_hex(&self) -> String {
        super::quantity::encode_data(&self.raw)
    }

    /// Recovers the address that signed this transaction — the check that
    /// proves a persisted raw transaction still belongs to the submitter
    /// this process is configured with, before it is re-broadcast after a
    /// restart.
    pub fn recover_sender(&self) -> Result<EvmAddress, secp::EvmSecpError> {
        secp::recover_address(&self.unsigned.signing_hash(), &self.signature)
    }
}

/// Strips leading zero bytes, leaving at least nothing (an all-zero input
/// yields an empty slice, which RLP encodes as the empty string).
fn trim_leading_zeros(bytes: &[u8; 32]) -> &[u8] {
    let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    &bytes[first..]
}

#[cfg(test)]
mod tests;
