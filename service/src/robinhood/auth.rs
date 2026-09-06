//! The `GlcRobinhoodBridge` EIP-712 authorization payloads: payout,
//! refund and settlement.
//!
//! # This is a transcription, not a design
//!
//! Every constant, every field and every field ORDER below is copied from
//! `contracts/src/GlcRobinhoodBridge.sol` and must match the deployed
//! bytecode exactly. A single character's difference in a type string
//! produces a different `typeHash`, therefore a different digest,
//! therefore a signature the contract refuses — or, in the case that
//! actually matters, a signature over a payload that says something other
//! than what this service intended.
//!
//! Because a Rust transcription of a Solidity constant is exactly the
//! kind of thing that drifts silently, this module is not trusted on its
//! own: `tests::golden` cross-checks every digest against
//! `contracts/test/fixtures/eip712-golden.json`, a file that
//! `contracts/test/GoldenDigests.t.sol` independently asserts the
//! DEPLOYED CONTRACT produces. Neither side generates the file. Both must
//! agree with it, so a drift on either side fails a test on that side.
//!
//! # There is exactly one definition of each message
//!
//! The brief's instruction — "never hand-roll a second incompatible
//! typed-data definition" — is enforced structurally: the struct-hash
//! functions below are reachable only through [`PayoutAuth`],
//! [`RefundAuth`] and [`SettlementAuth`], each of which takes typed,
//! already-validated values. There is no way to call the encoder with a
//! loose `u8` route or a raw byte array standing in for an amount.
//!
//! # Abandonment is deliberately absent
//!
//! `AbandonmentAuth` exists on-chain and closes an obligation WITHOUT
//! paying out and WITHOUT refunding — it retains a depositor's principal.
//! That is a decision no automated tick loop should be able to make, so
//! this module does not implement it and nothing in this service can
//! produce such a signature. If the retained-principal path is ever
//! needed, it belongs in a deliberate, separately reviewed operator tool.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::eip712::{
    encode_address, encode_uint128, encode_uint256, typed_data_hash, Eip712Domain,
};
use crate::evm::keccak::{keccak256, keccak256_concat};
use crate::evm::{EvmAddress, EvmChainId, EvmU256};
use crate::routes::Route;

/// The EIP-712 domain `name`. `GlcRobinhoodBridge.EIP712_NAME`.
pub const EIP712_NAME: &str = "GlcRobinhoodBridge";

/// The EIP-712 domain `version`. `GlcRobinhoodBridge.EIP712_VERSION`.
///
/// The SIGNING SCHEME's version, not the software's — changing it
/// invalidates every outstanding signature and is a protocol-version
/// event, exactly as the contract's own comment says.
pub const EIP712_VERSION: &str = "1";

/// Action discriminator: pay reserve GLC out of the custody contract.
pub const ACTION_PAYOUT: u8 = 0x01;
/// Action discriminator: return one obligation's principal to its depositor.
pub const ACTION_REFUND: u8 = 0x02;
/// Action discriminator: record one obligation as settled elsewhere.
pub const ACTION_SETTLE: u8 = 0x03;

/// `PayoutAuth`'s type string, character for character.
pub const PAYOUT_TYPE: &str = "PayoutAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,\
uint64 protocolDestChainId,address token,bytes32 requestId,address recipient,uint256 amount,\
uint64 signerEpoch,uint64 expiry)";

/// `RefundAuth`'s type string, character for character.
pub const REFUND_TYPE: &str = "RefundAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,\
uint64 protocolDestChainId,address token,bytes32 requestId,uint256 obligationIndex,\
address recipient,uint256 amount,uint64 signerEpoch,uint64 expiry)";

/// `SettlementAuth`'s type string, character for character.
///
/// Note the absent `token` field — a settlement moves no tokens, so the
/// contract does not bind one. Adding it here "for symmetry" would break
/// every signature.
pub const SETTLEMENT_TYPE: &str =
    "SettlementAuth(uint8 action,uint8 route,uint64 protocolSourceChainId,\
uint64 protocolDestChainId,bytes32 requestId,uint256 obligationIndex,uint64 signerEpoch,\
uint64 expiry)";

/// The pair of NAMESPACED protocol chain ids a route resolves to on the
/// contract — its immutable `PROTOCOL_CHAIN_GOLDCOIN`/
/// `PROTOCOL_CHAIN_ROBINHOOD`/`PROTOCOL_CHAIN_SOLANA`.
///
/// These are emphatically NOT the EIP-155 chain id (4663). They are the
/// bridge's own network-qualified identifiers, fixed at the contract's
/// construction, and this service reads them from the deployed contract
/// rather than assuming them — see [`super::calls::read_route_chains`].
/// Binding the wrong pair produces a digest the contract will not accept,
/// which is the safe failure; the unsafe one would be binding a pair that
/// happens to be valid for a DIFFERENT route, which is why the route byte
/// is bound alongside them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolChainPair {
    pub source: u64,
    pub dest: u64,
}

/// Why an authorization payload could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// A route the custody contract does not model — the two Solana-only
    /// routes, which have no discriminator at all.
    #[error(
        "route {route} has no GlcRobinhoodBridge route discriminator: it does not touch the \
         Robinhood custody contract"
    )]
    NotAContractRoute { route: &'static str },
    /// A payout was requested on a route that DEPOSITS into the contract,
    /// or a settlement/refund on one that pays out of it. The contract
    /// enforces this too (`NotAPayoutRoute`/`NotADepositRoute`); refusing
    /// here means the mistake never becomes a broadcast transaction that
    /// reverts after consuming a nonce.
    #[error("route {route} is not an outbound (payout) route on the custody contract")]
    NotAPayoutRoute { route: &'static str },
    #[error("route {route} is not an inbound (deposit) route on the custody contract")]
    NotADepositRoute { route: &'static str },
}

/// Whether a route brings GLC INTO the Robinhood custody contract (a
/// deposit) or takes it out (a payout).
///
/// Mirrors `GlcRobinhoodBridge._routeLegs`'s `inbound` flag, and is
/// stated per route as data rather than derived by comparing chain ids —
/// the same reasoning the contract records for writing the flag out per
/// row.
pub fn is_deposit_route(route: Route) -> Result<bool, AuthError> {
    match route {
        Route::RhnToGlc | Route::RhnToSol => Ok(true),
        Route::GlcToRhn | Route::SolToRhn => Ok(false),
        Route::GlcToSol | Route::SolToGlc => Err(AuthError::NotAContractRoute {
            route: route.as_str(),
        }),
    }
}

/// The contract's route discriminator, or a typed refusal.
fn contract_route(route: Route) -> Result<u8, AuthError> {
    route
        .contract_route_id()
        .ok_or(AuthError::NotAContractRoute {
            route: route.as_str(),
        })
}

/// The EIP-712 domain for one deployed `GlcRobinhoodBridge`.
///
/// Both `chainId` and `verifyingContract` are mandatory here even though
/// [`Eip712Domain`] allows either to be absent: they are what stop a
/// signature gathered for testnet being replayed on mainnet, and one
/// gathered for this deployment being replayed against its successor.
/// [`crate::evm::eip712`]'s docs say that requirement belongs with the
/// messages it protects — this is that place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeDomain {
    pub chain_id: EvmChainId,
    pub verifying_contract: EvmAddress,
}

impl BridgeDomain {
    pub fn new(chain_id: EvmChainId, verifying_contract: EvmAddress) -> BridgeDomain {
        BridgeDomain {
            chain_id,
            verifying_contract,
        }
    }

    /// The full domain, with the name and version this contract pins.
    pub fn to_eip712(self) -> Eip712Domain {
        Eip712Domain::new()
            .with_name(EIP712_NAME)
            .with_version(EIP712_VERSION)
            .with_chain_id(self.chain_id)
            .with_verifying_contract(self.verifying_contract)
    }

    /// `hashStruct(eip712Domain)` — what the contract's
    /// `domainSeparator()` view returns.
    pub fn separator(self) -> [u8; 32] {
        self.to_eip712().separator()
    }

    /// The final 32 bytes a signer signs, for an already-built struct
    /// hash.
    pub fn digest(self, struct_hash: &[u8; 32]) -> [u8; 32] {
        typed_data_hash(&self.separator(), struct_hash)
    }
}

/// A payout authorization: pay reserve GLC to a Robinhood recipient
/// against a deposit observed on the route's SOURCE network.
///
/// The field order below is `PAYOUT_TYPE`'s, which is
/// `_payoutStructHash`'s, which is what the contract hashes. Reordering
/// two fields of the same width would still compile and would still
/// produce a 32-byte digest — one the contract rejects, or worse, one
/// that means something else. That is why the golden fixture exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutAuth {
    pub route: Route,
    pub chains: ProtocolChainPair,
    /// The ERC-20 the contract custodies — `address(TOKEN)`, read from
    /// the contract, never assumed.
    pub token: EvmAddress,
    pub request_id: [u8; 32],
    pub recipient: EvmAddress,
    /// Robinhood 18-decimal atomic units. Typed, so a canonical 8-decimal
    /// amount cannot reach this field: it is 10^10 times too small and
    /// the mistake would be invisible in a decimal log line.
    pub amount: RobinhoodAtomic,
    pub signer_epoch: u64,
    pub expiry: u64,
}

impl PayoutAuth {
    /// `hashStruct(PayoutAuth)`.
    pub fn struct_hash(&self) -> Result<[u8; 32], AuthError> {
        let route = contract_route(self.route)?;
        if is_deposit_route(self.route)? {
            return Err(AuthError::NotAPayoutRoute {
                route: self.route.as_str(),
            });
        }
        Ok(keccak256_concat(&[
            &keccak256(PAYOUT_TYPE.as_bytes()),
            &encode_uint128(u128::from(ACTION_PAYOUT)),
            &encode_uint128(u128::from(route)),
            &encode_uint128(u128::from(self.chains.source)),
            &encode_uint128(u128::from(self.chains.dest)),
            &encode_address(self.token),
            &self.request_id,
            &encode_address(self.recipient),
            &encode_uint256(self.amount.to_u256()),
            &encode_uint128(u128::from(self.signer_epoch)),
            &encode_uint128(u128::from(self.expiry)),
        ]))
    }

    /// The 32 bytes a signer signs for this payout under `domain`.
    pub fn digest(&self, domain: BridgeDomain) -> Result<[u8; 32], AuthError> {
        Ok(domain.digest(&self.struct_hash()?))
    }
}

/// A refund authorization: return one obligation's principal to the
/// wallet that deposited it.
///
/// `recipient` and `amount` are caller-supplied on the wire but are NOT
/// a choice: the contract compares both against the obligation's own
/// recorded `depositor` and `amount` and reverts on any difference. This
/// service must therefore fill them from the obligation it read back from
/// the chain, never from anything an operator or a ledger row supplies
/// independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundAuth {
    /// The OBLIGATION's route, as recorded on-chain when the depositor's
    /// transfer landed. Not a signer's choice; the contract reads it from
    /// storage and binds that value, so a payload built with any other
    /// route simply fails to verify.
    pub route: Route,
    pub chains: ProtocolChainPair,
    pub token: EvmAddress,
    pub request_id: [u8; 32],
    pub obligation_index: u64,
    pub recipient: EvmAddress,
    pub amount: RobinhoodAtomic,
    pub signer_epoch: u64,
    pub expiry: u64,
}

impl RefundAuth {
    pub fn struct_hash(&self) -> Result<[u8; 32], AuthError> {
        let route = contract_route(self.route)?;
        if !is_deposit_route(self.route)? {
            return Err(AuthError::NotADepositRoute {
                route: self.route.as_str(),
            });
        }
        Ok(keccak256_concat(&[
            &keccak256(REFUND_TYPE.as_bytes()),
            &encode_uint128(u128::from(ACTION_REFUND)),
            &encode_uint128(u128::from(route)),
            &encode_uint128(u128::from(self.chains.source)),
            &encode_uint128(u128::from(self.chains.dest)),
            &encode_address(self.token),
            &self.request_id,
            &encode_uint256(EvmU256::from_u128(u128::from(self.obligation_index))),
            &encode_address(self.recipient),
            &encode_uint256(self.amount.to_u256()),
            &encode_uint128(u128::from(self.signer_epoch)),
            &encode_uint128(u128::from(self.expiry)),
        ]))
    }

    pub fn digest(&self, domain: BridgeDomain) -> Result<[u8; 32], AuthError> {
        Ok(domain.digest(&self.struct_hash()?))
    }
}

/// A settlement authorization: record that one obligation was paid out on
/// its destination network and is therefore no longer refundable.
///
/// Moves no tokens. Binds no token address — see [`SETTLEMENT_TYPE`].
/// Its type NAME differs from `AbandonmentAuth`'s even though the field
/// list is identical, which is a second, independent separation on top of
/// the action byte: a signature for one can never verify as the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementAuth {
    pub route: Route,
    pub chains: ProtocolChainPair,
    pub request_id: [u8; 32],
    pub obligation_index: u64,
    pub signer_epoch: u64,
    pub expiry: u64,
}

impl SettlementAuth {
    pub fn struct_hash(&self) -> Result<[u8; 32], AuthError> {
        let route = contract_route(self.route)?;
        if !is_deposit_route(self.route)? {
            return Err(AuthError::NotADepositRoute {
                route: self.route.as_str(),
            });
        }
        Ok(keccak256_concat(&[
            &keccak256(SETTLEMENT_TYPE.as_bytes()),
            &encode_uint128(u128::from(ACTION_SETTLE)),
            &encode_uint128(u128::from(route)),
            &encode_uint128(u128::from(self.chains.source)),
            &encode_uint128(u128::from(self.chains.dest)),
            &self.request_id,
            &encode_uint256(EvmU256::from_u128(u128::from(self.obligation_index))),
            &encode_uint128(u128::from(self.signer_epoch)),
            &encode_uint128(u128::from(self.expiry)),
        ]))
    }

    pub fn digest(&self, domain: BridgeDomain) -> Result<[u8; 32], AuthError> {
        Ok(domain.digest(&self.struct_hash()?))
    }
}

/// `keccak256(PAYOUT_TYPE)`. Exposed for the golden fixture and operator
/// tooling.
pub fn payout_typehash() -> [u8; 32] {
    keccak256(PAYOUT_TYPE.as_bytes())
}

/// `keccak256(REFUND_TYPE)`.
pub fn refund_typehash() -> [u8; 32] {
    keccak256(REFUND_TYPE.as_bytes())
}

/// `keccak256(SETTLEMENT_TYPE)`.
pub fn settlement_typehash() -> [u8; 32] {
    keccak256(SETTLEMENT_TYPE.as_bytes())
}

/// Domain string for [`derive_request_id`]'s preimage.
pub const REQUEST_ID_DOMAIN: &[u8] = b"glc.reserve-bridge.robinhood.request-id.v1";

/// Domain-separated derivation of the `bytes32 requestId` every
/// authorization carries.
///
/// # Why derived rather than random
///
/// The contract consumes `(action, requestId)` exactly once. A random id
/// would have to be persisted BEFORE the signatures were gathered, and a
/// crash between generating it and persisting it would leave the service
/// unable to tell whether an id it cannot see was already consumed
/// on-chain. A derivation from durable identity has no such window: after
/// any restart, from any point, the same operation re-derives the same
/// id, so re-running the flow re-attempts the SAME on-chain request
/// rather than creating a second one.
///
/// # What goes into the preimage, and why each part
///
/// - A version-tagged domain string, so an id from this scheme can never
///   collide with one from a future scheme.
/// - The action byte, so the same operation's payout/refund/settlement
///   ids differ even though the contract already keys on the action.
/// - The route byte, so an id is bound to the leg it belongs to.
/// - The verifying contract and the EVM chain id, so an id derived for
///   one deployment or one network is not the id for another.
/// - The operation's own durable identity, supplied by the caller.
pub fn derive_request_id(
    action: u8,
    route: Route,
    domain: BridgeDomain,
    identity: &[u8],
) -> Result<[u8; 32], AuthError> {
    let route_byte = contract_route(route)?;
    Ok(keccak256_concat(&[
        REQUEST_ID_DOMAIN,
        &[action],
        &[route_byte],
        domain.verifying_contract.as_bytes(),
        &domain.chain_id.get().to_be_bytes(),
        identity,
    ]))
}

/// The durable identity of a GOLDCOIN-sourced payout: the deposit
/// outpoint that funded it, plus the ledger row it created.
///
/// The outpoint alone would be enough — a Goldcoin UTXO is spent once —
/// and the row id alone would not be (row ids are local to one database
/// file). Both are included so the id is stable under a ledger restore
/// AND unique without depending on that stability.
pub fn goldcoin_source_identity(txid: [u8; 32], vout: u32, request_id: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(&txid);
    out.extend_from_slice(&vout.to_be_bytes());
    out.extend_from_slice(&request_id.to_be_bytes());
    out
}

/// The durable identity of a Robinhood obligation: its contract-local
/// index. The contract address is already in the preimage, so the index
/// alone completes the identity.
pub fn obligation_identity(obligation_index: u64) -> Vec<u8> {
    obligation_index.to_be_bytes().to_vec()
}

#[cfg(test)]
mod tests;
