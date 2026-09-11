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
/// Action discriminator: move reserve GLC to the contract's immutable
/// `TREASURY`. `GlcRobinhoodBridge.ACTION_TREASURY_WITHDRAW`.
pub const ACTION_TREASURY_WITHDRAW: u8 = 0x0C;

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

/// `TreasuryWithdrawAuth`'s type string, character for character.
///
/// No `route` and no protocol chain pair, and that is the contract's
/// decision, not an omission here: a withdrawal is not a movement between
/// two networks. It binds the token (as a payout does) and the TREASURY
/// address, so the quorum signs the destination it can see and the
/// contract compares it to its immutable.
pub const TREASURY_WITHDRAW_TYPE: &str =
    "TreasuryWithdrawAuth(uint8 action,address token,bytes32 requestId,address treasury,\
uint256 amount,uint64 signerEpoch,uint64 expiry)";

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
/// A treasury-withdrawal authorization: move `amount` of reserve GLC to
/// the contract's immutable `TREASURY`.
///
/// `treasury` is on the wire so the signed payload NAMES its destination,
/// but it is not a choice: the contract reverts unless it equals the
/// immutable. This service fills it from `treasury()` read off the
/// contract, never from configuration or an operator argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreasuryWithdrawAuth {
    /// `address(TOKEN)`, read from the contract.
    pub token: EvmAddress,
    pub request_id: [u8; 32],
    /// The contract's `TREASURY`, read from the contract.
    pub treasury: EvmAddress,
    /// Robinhood 18-decimal atomic units — see [`PayoutAuth::amount`].
    pub amount: RobinhoodAtomic,
    pub signer_epoch: u64,
    pub expiry: u64,
}

impl TreasuryWithdrawAuth {
    /// `hashStruct(TreasuryWithdrawAuth)` — `_treasuryWithdrawStructHash`'s
    /// field order exactly.
    pub fn struct_hash(&self) -> Result<[u8; 32], AuthError> {
        Ok(keccak256_concat(&[
            &keccak256(TREASURY_WITHDRAW_TYPE.as_bytes()),
            &encode_uint128(u128::from(ACTION_TREASURY_WITHDRAW)),
            &encode_address(self.token),
            &self.request_id,
            &encode_address(self.treasury),
            &encode_uint256(self.amount.to_u256()),
            &encode_uint128(u128::from(self.signer_epoch)),
            &encode_uint128(u128::from(self.expiry)),
        ]))
    }

    pub fn digest(&self, domain: BridgeDomain) -> Result<[u8; 32], AuthError> {
        Ok(domain.digest(&self.struct_hash()?))
    }
}

pub fn treasury_withdraw_typehash() -> [u8; 32] {
    keccak256(TREASURY_WITHDRAW_TYPE.as_bytes())
}

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

/// [`derive_request_id`] for a treasury withdrawal, which has no route.
///
/// The preimage keeps the same layout with a `0x00` in the route slot —
/// permanently invalid as a route on the contract, so an id derived here
/// can never coincide with one derived for any route-bound operation
/// even before the action byte separates them.
pub fn derive_treasury_withdraw_request_id(domain: BridgeDomain, identity: &[u8]) -> [u8; 32] {
    keccak256_concat(&[
        REQUEST_ID_DOMAIN,
        &[ACTION_TREASURY_WITHDRAW],
        &[0u8],
        domain.verifying_contract.as_bytes(),
        &domain.chain_id.get().to_be_bytes(),
        identity,
    ])
}

/// The durable identity of a treasury withdrawal: the `rebalance_requests`
/// row that approved it.
///
/// The row id alone is local to one database file; `requested_at` and
/// the amount are included so the identity survives a ledger restore and
/// so two different approvals can never share an id even if row numbers
/// were ever reused.
pub fn treasury_withdrawal_identity(
    rebalance_id: i64,
    requested_at: i64,
    amount_canonical: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(&rebalance_id.to_be_bytes());
    out.extend_from_slice(&requested_at.to_be_bytes());
    out.extend_from_slice(&amount_canonical.to_be_bytes());
    out
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

/// One of the three authorization payloads this service can build, in a
/// single type.
///
/// # Why this exists
///
/// The three payload structs above are the *encoders*. This is the
/// *request*: the thing that gets handed to a signer. Phase F handed
/// signers a bare 32-byte digest, which was adequate while the only
/// implementation was an in-process dev key. It is not adequate for a
/// production custody domain, which must be able to understand and refuse
/// what it is being asked to sign — the lesson `crate::signing::policy`
/// records at length for the Solana/Goldcoin side.
///
/// A digest cannot be understood. It is a hash; there is nothing in it to
/// inspect. So the request that crosses the signer boundary carries the
/// STRUCTURED fields, and the signer recomputes the digest from them with
/// this same code. See [`crate::signing::evm_policy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvmAuthPayload {
    Payout(PayoutAuth),
    Refund(RefundAuth),
    Settlement(SettlementAuth),
    TreasuryWithdraw(TreasuryWithdrawAuth),
}

/// A complete authorization request: the payload plus the deployment
/// domain it is to be signed under.
///
/// Carrying the domain rather than only the struct hash is what lets a
/// signer bind the verifying contract and the EVM chain id independently
/// — the two fields that stop a signature gathered for one deployment
/// being replayed against another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmAuthRequest {
    pub domain: BridgeDomain,
    pub payload: EvmAuthPayload,
}

impl EvmAuthRequest {
    pub fn payout(domain: BridgeDomain, auth: PayoutAuth) -> EvmAuthRequest {
        EvmAuthRequest {
            domain,
            payload: EvmAuthPayload::Payout(auth),
        }
    }

    pub fn refund(domain: BridgeDomain, auth: RefundAuth) -> EvmAuthRequest {
        EvmAuthRequest {
            domain,
            payload: EvmAuthPayload::Refund(auth),
        }
    }

    pub fn settlement(domain: BridgeDomain, auth: SettlementAuth) -> EvmAuthRequest {
        EvmAuthRequest {
            domain,
            payload: EvmAuthPayload::Settlement(auth),
        }
    }

    pub fn treasury_withdraw(domain: BridgeDomain, auth: TreasuryWithdrawAuth) -> EvmAuthRequest {
        EvmAuthRequest {
            domain,
            payload: EvmAuthPayload::TreasuryWithdraw(auth),
        }
    }

    /// The action discriminator the contract keys its replay guard on.
    pub fn action(&self) -> u8 {
        match &self.payload {
            EvmAuthPayload::Payout(_) => ACTION_PAYOUT,
            EvmAuthPayload::Refund(_) => ACTION_REFUND,
            EvmAuthPayload::Settlement(_) => ACTION_SETTLE,
            EvmAuthPayload::TreasuryWithdraw(_) => ACTION_TREASURY_WITHDRAW,
        }
    }

    /// A stable, lowercase wire name for the payload family. Used by the
    /// remote-signer protocol and by operator output; never parsed back
    /// into an action byte without also checking the byte itself.
    pub fn kind_str(&self) -> &'static str {
        match &self.payload {
            EvmAuthPayload::Payout(_) => "payout",
            EvmAuthPayload::Refund(_) => "refund",
            EvmAuthPayload::Settlement(_) => "settlement",
            EvmAuthPayload::TreasuryWithdraw(_) => "treasury_withdraw",
        }
    }

    /// The route the payload is bound to. `None` for a treasury
    /// withdrawal, which is not a movement between two networks and binds
    /// no route — see [`TREASURY_WITHDRAW_TYPE`].
    pub fn route(&self) -> Option<Route> {
        match &self.payload {
            EvmAuthPayload::Payout(a) => Some(a.route),
            EvmAuthPayload::Refund(a) => Some(a.route),
            EvmAuthPayload::Settlement(a) => Some(a.route),
            EvmAuthPayload::TreasuryWithdraw(_) => None,
        }
    }

    /// The protocol chain pair the route resolves to; `None` exactly when
    /// [`Self::route`] is.
    pub fn chains(&self) -> Option<ProtocolChainPair> {
        match &self.payload {
            EvmAuthPayload::Payout(a) => Some(a.chains),
            EvmAuthPayload::Refund(a) => Some(a.chains),
            EvmAuthPayload::Settlement(a) => Some(a.chains),
            EvmAuthPayload::TreasuryWithdraw(_) => None,
        }
    }

    /// The contract's `TREASURY`, for the one payload that binds it.
    pub fn treasury(&self) -> Option<EvmAddress> {
        match &self.payload {
            EvmAuthPayload::TreasuryWithdraw(a) => Some(a.treasury),
            _ => None,
        }
    }

    /// The custodied token, for the two payloads that bind one.
    /// `None` for a settlement, which moves no tokens and deliberately
    /// does not bind a token address — see [`SETTLEMENT_TYPE`].
    pub fn token(&self) -> Option<EvmAddress> {
        match &self.payload {
            EvmAuthPayload::Payout(a) => Some(a.token),
            EvmAuthPayload::Refund(a) => Some(a.token),
            EvmAuthPayload::Settlement(_) => None,
            EvmAuthPayload::TreasuryWithdraw(a) => Some(a.token),
        }
    }

    /// The contract-side `bytes32 requestId`.
    pub fn contract_request_id(&self) -> [u8; 32] {
        match &self.payload {
            EvmAuthPayload::Payout(a) => a.request_id,
            EvmAuthPayload::Refund(a) => a.request_id,
            EvmAuthPayload::Settlement(a) => a.request_id,
            EvmAuthPayload::TreasuryWithdraw(a) => a.request_id,
        }
    }

    /// `None` for a payout, which settles a deposit that happened on
    /// another chain entirely and therefore has no local obligation.
    pub fn obligation_index(&self) -> Option<u64> {
        match &self.payload {
            EvmAuthPayload::Payout(_) => None,
            EvmAuthPayload::Refund(a) => Some(a.obligation_index),
            EvmAuthPayload::Settlement(a) => Some(a.obligation_index),
            EvmAuthPayload::TreasuryWithdraw(_) => None,
        }
    }

    /// The address value moves to. `None` for a settlement, which moves
    /// none. For a treasury withdrawal this is the treasury itself.
    pub fn recipient(&self) -> Option<EvmAddress> {
        match &self.payload {
            EvmAuthPayload::Payout(a) => Some(a.recipient),
            EvmAuthPayload::Refund(a) => Some(a.recipient),
            EvmAuthPayload::Settlement(_) => None,
            EvmAuthPayload::TreasuryWithdraw(a) => Some(a.treasury),
        }
    }

    /// The amount, in Robinhood 18-decimal atomic units. `None` for a
    /// settlement.
    pub fn amount(&self) -> Option<RobinhoodAtomic> {
        match &self.payload {
            EvmAuthPayload::Payout(a) => Some(a.amount),
            EvmAuthPayload::Refund(a) => Some(a.amount),
            EvmAuthPayload::Settlement(_) => None,
            EvmAuthPayload::TreasuryWithdraw(a) => Some(a.amount),
        }
    }

    pub fn signer_epoch(&self) -> u64 {
        match &self.payload {
            EvmAuthPayload::Payout(a) => a.signer_epoch,
            EvmAuthPayload::Refund(a) => a.signer_epoch,
            EvmAuthPayload::Settlement(a) => a.signer_epoch,
            EvmAuthPayload::TreasuryWithdraw(a) => a.signer_epoch,
        }
    }

    pub fn expiry(&self) -> u64 {
        match &self.payload {
            EvmAuthPayload::Payout(a) => a.expiry,
            EvmAuthPayload::Refund(a) => a.expiry,
            EvmAuthPayload::Settlement(a) => a.expiry,
            EvmAuthPayload::TreasuryWithdraw(a) => a.expiry,
        }
    }

    /// `hashStruct(payload)` — the same function the contract applies.
    pub fn struct_hash(&self) -> Result<[u8; 32], AuthError> {
        match &self.payload {
            EvmAuthPayload::Payout(a) => a.struct_hash(),
            EvmAuthPayload::Refund(a) => a.struct_hash(),
            EvmAuthPayload::Settlement(a) => a.struct_hash(),
            EvmAuthPayload::TreasuryWithdraw(a) => a.struct_hash(),
        }
    }

    /// The 32 bytes a signer signs.
    ///
    /// There is exactly one definition, here, reached identically by the
    /// bridge that builds the request and by the custody domain that
    /// evaluates it — which is what makes a signer-side recomputation a
    /// real check rather than a second, drifting implementation.
    pub fn digest(&self) -> Result<[u8; 32], AuthError> {
        Ok(self.domain.digest(&self.struct_hash()?))
    }

    /// A one-line human summary for a custody domain's own audit log —
    /// the [`crate::signing::policy::ClaimRequest::summary`] of this side.
    pub fn summary(&self) -> String {
        match &self.payload {
            EvmAuthPayload::Payout(a) => format!(
                "PAYOUT {} to {} on route {} (contract {})",
                a.amount,
                a.recipient.to_checksum_string(),
                a.route.as_str(),
                self.domain.verifying_contract.to_checksum_string(),
            ),
            EvmAuthPayload::Refund(a) => format!(
                "REFUND of obligation {} — {} to its depositor {} on route {} (contract {})",
                a.obligation_index,
                a.amount,
                a.recipient.to_checksum_string(),
                a.route.as_str(),
                self.domain.verifying_contract.to_checksum_string(),
            ),
            EvmAuthPayload::Settlement(a) => format!(
                "SETTLE obligation {} on route {} — moves no tokens (contract {})",
                a.obligation_index,
                a.route.as_str(),
                self.domain.verifying_contract.to_checksum_string(),
            ),
            EvmAuthPayload::TreasuryWithdraw(a) => format!(
                "RESERVE WITHDRAWAL {} to treasury {} (contract {})",
                a.amount,
                a.treasury.to_checksum_string(),
                self.domain.verifying_contract.to_checksum_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests;
