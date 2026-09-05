//! EIP-712 building blocks: the domain separator, the `\x19\x01` digest, and
//! the primitive value encoders those are built from.
//!
//! # Scope: the generic half only
//!
//! [EIP-712] has two halves. One is generic and identical for every
//! application: the domain, the `hashStruct` construction, the `\x19\x01`
//! prefix, and how a primitive value becomes a 32-byte word. The other is
//! application-specific: which messages exist, what fields they carry, and
//! who is allowed to sign them.
//!
//! [EIP-712]: https://eips.ethereum.org/EIPS/eip-712
//!
//! This module implements the generic half and **only** the generic half.
//! There is no payout message, no refund message, no unpause, no signer or
//! guardian change, no limit change and no migration message here, and no
//! notion of an authorised signer, a quorum, or a nonce. Those are the
//! bridge's authorisation policy: they decide who can move money, they need
//! the ledger and the contract to exist to be meaningful, and each one
//! wants its own reviewable change with its own threat analysis. Building
//! them speculatively in a types phase would mean shipping unexercised
//! authorisation code, which is the worst kind to ship.
//!
//! What a later phase gets from here is that it never has to write a
//! keccak-of-a-concatenation by hand: it declares its own `typeHash`
//! constant and its own field order, encodes each field with the helpers
//! below, and takes the final digest from [`typed_data_hash`].
//!
//! # Nothing is hand-rolled
//!
//! The hash is [`crate::evm::keccak`] (i.e. RustCrypto's `sha3`), never a
//! local implementation. The encoding rules are transcribed from the EIP
//! and pinned to the EIP's own published test vector — the "Ether Mail"
//! example — so a transcription mistake fails a test rather than producing
//! a digest that looks fine and authorises the wrong message.
//!
//! # No signing, no verification, no keys
//!
//! This module produces a 32-byte digest. It does not sign one, does not
//! verify a signature over one, and never sees key material. See
//! [`crate::evm::signature`] for what is deferred and why.

use super::address::EvmAddress;
use super::chain_id::EvmChainId;
use super::keccak::{keccak256, keccak256_concat};
use super::u256::EvmU256;

/// The two-byte prefix EIP-191 gives EIP-712 structured data: `0x19` (the
/// "this is not RLP, so it cannot be a transaction" marker) followed by
/// `0x01` (the structured-data version).
///
/// Omitting it, or using EIP-191's personal-sign version `0x45` instead,
/// produces a digest a wallet will happily sign but a `verifyTypedData`
/// verifier will reject — or, worse, one that collides with a differently
/// intended message.
pub const EIP712_PREFIX: [u8; 2] = [0x19, 0x01];

/// The EIP-712 domain: what binds a signature to one application, one
/// chain, and one contract.
///
/// Every field is optional, exactly as the EIP specifies: the type string
/// and the encoding are built from the fields that are actually present, and
/// a field left `None` is omitted from both. Two domains that differ in
/// which fields they set therefore have different type hashes, which is the
/// intended behaviour — a signature is not transferable between them.
///
/// # Why the fields matter
///
/// - `chain_id` is what stops a signature gathered on testnet from being
///   replayed on mainnet. See [`crate::evm::networks`]: Robinhood mainnet
///   and testnet are different chain ids precisely so that this works.
/// - `verifying_contract` is what stops a signature for one deployment from
///   being replayed against its successor.
///
/// Neither is enforced as mandatory here, because the EIP does not make them
/// mandatory and this is a generic primitive. The authorisation policy that
/// arrives in a later phase should require both, and that requirement
/// belongs there, next to the messages it protects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Eip712Domain {
    /// `string name` — the application's name, e.g. `"GlcRobinhoodBridge"`.
    pub name: Option<String>,
    /// `string version` — the signing scheme's version, not the software's.
    pub version: Option<String>,
    /// `uint256 chainId` — the EIP-155 chain id the signature is valid on.
    pub chain_id: Option<EvmChainId>,
    /// `address verifyingContract` — the contract that will verify it.
    pub verifying_contract: Option<EvmAddress>,
    /// `bytes32 salt` — a last-resort disambiguator, rarely used.
    pub salt: Option<[u8; 32]>,
}

impl Eip712Domain {
    /// An empty domain. Fields are then set with the `with_*` builders, or
    /// directly — they are public, because a domain is a plain description
    /// with no invariant to protect.
    pub fn new() -> Eip712Domain {
        Eip712Domain::default()
    }

    /// Sets `name`.
    pub fn with_name(mut self, name: impl Into<String>) -> Eip712Domain {
        self.name = Some(name.into());
        self
    }

    /// Sets `version`.
    pub fn with_version(mut self, version: impl Into<String>) -> Eip712Domain {
        self.version = Some(version.into());
        self
    }

    /// Sets `chainId`.
    pub fn with_chain_id(mut self, chain_id: EvmChainId) -> Eip712Domain {
        self.chain_id = Some(chain_id);
        self
    }

    /// Sets `verifyingContract`.
    pub fn with_verifying_contract(mut self, contract: EvmAddress) -> Eip712Domain {
        self.verifying_contract = Some(contract);
        self
    }

    /// Sets `salt`.
    pub fn with_salt(mut self, salt: [u8; 32]) -> Eip712Domain {
        self.salt = Some(salt);
        self
    }

    /// The `encodeType` string for this domain, listing exactly the fields
    /// that are set, in the EIP's canonical order.
    ///
    /// The order is fixed by the EIP and is **not** the order the fields
    /// happen to be declared in a struct somewhere: `name`, `version`,
    /// `chainId`, `verifyingContract`, `salt`. Getting it wrong changes the
    /// type hash and therefore every signature.
    pub fn type_string(&self) -> String {
        let mut fields: Vec<&str> = Vec::with_capacity(5);
        if self.name.is_some() {
            fields.push("string name");
        }
        if self.version.is_some() {
            fields.push("string version");
        }
        if self.chain_id.is_some() {
            fields.push("uint256 chainId");
        }
        if self.verifying_contract.is_some() {
            fields.push("address verifyingContract");
        }
        if self.salt.is_some() {
            fields.push("bytes32 salt");
        }
        format!("EIP712Domain({})", fields.join(","))
    }

    /// `keccak256` of [`Eip712Domain::type_string`].
    pub fn type_hash(&self) -> [u8; 32] {
        keccak256(self.type_string().as_bytes())
    }

    /// The domain separator: `hashStruct(eip712Domain)`.
    ///
    /// The type hash followed by one 32-byte word per present field, in the
    /// same order as the type string — dynamic `string` fields as the hash
    /// of their bytes, `uint256` and `address` left-padded, `bytes32`
    /// verbatim.
    pub fn separator(&self) -> [u8; 32] {
        let type_hash = self.type_hash();
        let mut words: Vec<[u8; 32]> = Vec::with_capacity(5);
        if let Some(name) = &self.name {
            words.push(encode_string(name));
        }
        if let Some(version) = &self.version {
            words.push(encode_string(version));
        }
        if let Some(chain_id) = self.chain_id {
            words.push(encode_uint256(chain_id.to_u256()));
        }
        if let Some(contract) = self.verifying_contract {
            words.push(encode_address(contract));
        }
        if let Some(salt) = self.salt {
            words.push(salt);
        }

        let mut parts: Vec<&[u8]> = Vec::with_capacity(words.len() + 1);
        parts.push(&type_hash);
        for word in &words {
            parts.push(word);
        }
        keccak256_concat(&parts)
    }
}

/// The final EIP-712 digest — the 32 bytes that actually get signed:
/// `keccak256(0x19 || 0x01 || domainSeparator || hashStruct(message))`.
///
/// Both inputs are already 32-byte hashes, so this cannot be given a
/// half-encoded message by accident; producing `struct_hash` correctly for a
/// particular message type is that message's own job, using the encoders
/// below.
pub fn typed_data_hash(domain_separator: &[u8; 32], struct_hash: &[u8; 32]) -> [u8; 32] {
    keccak256_concat(&[&EIP712_PREFIX, domain_separator, struct_hash])
}

/// Encodes a dynamic `string` field: `keccak256` of its UTF-8 bytes.
///
/// The string's *contents* are hashed, not its ABI encoding — a dynamic type
/// in EIP-712 is always represented by the hash of its value.
pub fn encode_string(value: &str) -> [u8; 32] {
    keccak256(value.as_bytes())
}

/// Encodes a dynamic `bytes` field: `keccak256` of the bytes.
pub fn encode_bytes(value: &[u8]) -> [u8; 32] {
    keccak256(value)
}

/// Encodes a `uint256` field: the word itself, big-endian.
pub fn encode_uint256(value: EvmU256) -> [u8; 32] {
    value.to_be_bytes()
}

/// Encodes a `uint64`/`uint128`-style field as the `uint256` word the ABI
/// requires, left-padded with zeros.
pub fn encode_uint128(value: u128) -> [u8; 32] {
    EvmU256::from_u128(value).to_be_bytes()
}

/// Encodes an `address` field: the 20 bytes right-aligned in a 32-byte word,
/// left-padded with 12 zero bytes.
///
/// Right-aligned, not left-aligned. An address written into the high bytes
/// instead encodes a completely different (and enormous) value, and the
/// mistake is invisible in a hex dump unless you count the zeros — hence the
/// test that pins the padding down.
pub fn encode_address(value: EvmAddress) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(value.as_bytes());
    word
}

/// Encodes a `bool` field: `0x00..00` or `0x00..01`.
pub fn encode_bool(value: bool) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[31] = u8::from(value);
    word
}

#[cfg(test)]
mod tests;
