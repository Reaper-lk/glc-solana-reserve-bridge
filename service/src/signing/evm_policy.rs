//! Signer-side policy for Robinhood EIP-712 authorizations: what an EVM
//! authorization signer must independently decide before it puts a
//! signature on anything.
//!
//! # Why this module exists, and why it is not [`super::policy`]
//!
//! [`super::policy`] records the incident that produced it: attestation
//! signers were **blind oracles**, whose entire authorization decision was
//! "did this request carry a valid bearer token?". The fix was to give
//! each custody domain the means to understand and refuse what it is
//! being asked to sign, by shipping the parser for the canonical claim
//! bytes alongside the policy decision over the parsed result.
//!
//! The Robinhood leg cannot reuse that module, and the reason is
//! structural rather than stylistic. A Solana/Goldcoin claim is a
//! self-describing byte string: a domain tag, an action byte and typed
//! fields, all of which [`super::policy::parse_claim`] can read straight
//! out of `payload_hex`. An EIP-712 authorization is a **32-byte hash**.
//! There is nothing in it to parse. A signer handed one can verify its
//! length and absolutely nothing else, which is precisely the blind-oracle
//! posture the incident was about.
//!
//! So the request that crosses the signer boundary for an EVM
//! authorization carries the STRUCTURED FIELDS, and the signer
//! **recomputes the digest from them** using
//! [`crate::robinhood::auth`] — the same single encoder the bridge used to
//! build it, and the same one the deployed contract is cross-checked
//! against by `contracts/test/GoldenDigests.t.sol`. The signer then signs
//! the digest IT computed. Never the one it was sent.
//!
//! That inversion is the whole design:
//!
//! - A caller cannot make a signer sign arbitrary bytes, because a caller
//!   does not supply bytes to sign. It supplies an authorization, and the
//!   signer derives the bytes.
//! - A caller cannot smuggle different content past the policy check,
//!   because the fields the policy inspects are the same fields the digest
//!   is derived from. There is no second representation to disagree with.
//! - A requester that disagrees with the signer about the encoding is
//!   caught immediately: the request carries the requester's own
//!   `expected_digest`, and a mismatch is a refusal
//!   ([`EvmPolicyError::DigestMismatch`]) rather than something either
//!   side papers over.
//!
//! # What a custody domain runs, and what it does not
//!
//! Unchanged from [`super::policy`]: this crate is a *client* and never
//! holds signing keys. The HTTP shim in front of an HSM/KMS is each
//! custody domain's own process, and this repository does not ship it.
//!
//! What it ships — and what this module is — is the part that must be
//! identical everywhere: the request document, the reconstruction of the
//! authorization from it, and the policy decision over the result. A
//! domain links this, feeds it the body of a
//! `POST /v2/sign-evm-auth` request, and signs only the digest an `Ok`
//! hands back.
//!
//! # The rules, and why each one is here
//!
//! 1. **Never sign a supplied digest.** Rule zero, stated above. The
//!    digest is an OUTPUT of [`EvmSignerPolicy::evaluate`], not an input
//!    to it.
//! 2. **Fail closed on the unknown.** An unrecognized protocol version,
//!    action byte, route or payload kind is a refusal. A future protocol
//!    version must be deployed to the signers BEFORE it is deployed to the
//!    bridge, not the other way around — the same ordering
//!    [`super::policy`] requires.
//! 3. **Hold the deployment identity independently.** The verifying
//!    contract, the EVM chain id, the custodied token and each route's
//!    protocol chain pair are provisioned by the domain's own operators.
//!    A domain that read them out of the request would be verifying the
//!    request against itself.
//! 4. **Scope credentials by action.** [`EvmSignerPolicy::allowed_actions`]
//!    is the analogue of [`super::policy::SignerPolicy::allowed_classes`].
//!    A refund and a payout are different powers and a deployment may
//!    reasonably grant one credential fewer of them.
//! 5. **Bound the amount independently too.** The contract enforces its
//!    own per-transfer and rolling limits under signer quorum; this
//!    ceiling means an attacker must subvert the quorum AND every custody
//!    domain's configuration.
//! 6. **Bound the authorization's lifetime.** An expiry far in the future
//!    is a standing permission slip. A domain refuses one longer than it
//!    agreed to, independently of what the bridge configured.
//!
//! # Abandonment remains absent
//!
//! `AbandonmentAuth` closes an obligation without paying out and without
//! refunding. [`crate::robinhood::auth`] cannot build one, this module
//! cannot represent one, and a request naming its action byte is refused
//! as unknown. That is three independent refusals for a payload that
//! retains a depositor's principal.

use serde::{Deserialize, Serialize};

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::{EvmAddress, EvmChainId, EvmU256};
use crate::robinhood::auth::{
    BridgeDomain, EvmAuthPayload, EvmAuthRequest, PayoutAuth, ProtocolChainPair, RefundAuth,
    SettlementAuth, TreasuryWithdrawAuth, ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE,
    ACTION_TREASURY_WITHDRAW,
};
use crate::routes::Route;

/// The version of the EVM authorization request document below.
///
/// It is `2` rather than `1` because it is the SECOND protocol this
/// signer deployment speaks: `POST /v1/sign` (raw payload bytes, for
/// Goldcoin sighashes and Solana claim messages) is version 1 and is
/// untouched. A signer process that implements only version 1 keeps
/// working exactly as before and simply answers `404`/`400` on the new
/// path, which this client reports as [`crate::signing::signers::
/// SignerError::Rejected`] — a fail-closed outcome, never a silent
/// fallback to the older, weaker request shape.
pub const EVM_AUTH_PROTOCOL_VERSION: u32 = 2;

/// The wire document a bridge sends and a custody domain evaluates.
///
/// # Every field is here because the digest binds it
///
/// The requirement is that a signer's decision covers everything the
/// authorization actually says. Since the digest is recomputed from
/// exactly these fields, "the signer inspected it" and "the signer signed
/// it" cannot come apart: a field the domain did not receive is a field
/// that could not have been in the digest either, and reconstruction
/// would fail rather than silently default it.
///
/// Amounts travel as DECIMAL STRINGS in Robinhood's own 18-decimal atomic
/// unit. Not a JSON number: `10^18` atomic units is one whole GLC, and an
/// IEEE-754 double cannot represent that range exactly. A JSON parser
/// that silently rounded an amount would produce a digest for a different
/// transfer than the one the operator approved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvmAuthSignRequest {
    /// Must equal [`EVM_AUTH_PROTOCOL_VERSION`]. Checked FIRST, before
    /// any other field is interpreted.
    pub protocol_version: u32,
    /// `"payout"` | `"refund"` | `"settlement"` | `"treasury_withdraw"`.
    /// Redundant with `action` on purpose: the two must agree, and a
    /// request where they do not is malformed or deliberately confusing —
    /// the same reasoning [`super::policy::parse_claim`] applies to its
    /// action/length pair.
    pub kind: String,
    /// The contract's action discriminator (`0x01`/`0x02`/`0x03`/`0x0C`).
    pub action: u8,
    /// The wire spelling of [`Route`], e.g. `"GlcToRhn"`. Absent exactly
    /// for a treasury withdrawal, which binds no route. A signer built
    /// before withdrawals existed fails to parse such a document at all —
    /// fail-closed, never a silent default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// The EIP-155 chain id the EIP-712 domain binds.
    pub chain_id: u64,
    /// The deployed `GlcRobinhoodBridge`, `0x`-prefixed hex.
    pub verifying_contract: String,
    /// The contract's own namespaced protocol chain ids for this route —
    /// NOT the EIP-155 id above. See [`ProtocolChainPair`]. Absent exactly
    /// when `route` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_source_chain_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_dest_chain_id: Option<u64>,
    /// The contract's immutable `TREASURY`, `0x`-prefixed hex. Present
    /// exactly for a treasury withdrawal. A signer compares it against the
    /// treasury list it holds INDEPENDENTLY — see
    /// [`EvmSignerPolicy::allowed_treasuries`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treasury: Option<String>,
    /// The custodied ERC-20. Absent exactly for a settlement, whose
    /// payload deliberately binds no token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The contract-side `bytes32 requestId`, `0x`-prefixed hex.
    pub request_id: String,
    /// Absent exactly for a payout, which settles a deposit made on
    /// another chain and has no local obligation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_index: Option<u64>,
    /// Where value goes. Absent exactly for a settlement, which moves
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// Robinhood 18-decimal atomic units, as a decimal string. Absent
    /// exactly for a settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount_robinhood_atomic: Option<String>,
    pub signer_epoch: u64,
    /// Unix seconds after which the contract refuses this authorization.
    pub expiry: u64,
    /// The REQUESTER's own computed digest, `0x`-prefixed hex.
    ///
    /// This is not what gets signed, and a domain that treated it as such
    /// would have rebuilt the blind oracle. It is a cross-check: the
    /// signer recomputes the digest from the fields above and refuses if
    /// the two disagree, so an encoding drift between the bridge and the
    /// custody domains is caught on the first request rather than by a
    /// contract revert after gas was spent.
    pub expected_digest: String,
}

impl EvmAuthSignRequest {
    /// Builds the wire document for an authorization this service has
    /// already constructed.
    ///
    /// Fallible only because [`EvmAuthRequest::digest`] is: a payload
    /// naming a route the contract does not model, or the wrong leg for
    /// its action, has no digest and therefore no request document.
    pub fn from_request(
        request: &EvmAuthRequest,
    ) -> Result<EvmAuthSignRequest, crate::robinhood::auth::AuthError> {
        let digest = request.digest()?;
        Ok(EvmAuthSignRequest {
            protocol_version: EVM_AUTH_PROTOCOL_VERSION,
            kind: request.kind_str().to_string(),
            action: request.action(),
            route: request.route().map(|r| r.as_str().to_string()),
            chain_id: request.domain.chain_id.get(),
            verifying_contract: request.domain.verifying_contract.to_checksum_string(),
            protocol_source_chain_id: request.chains().map(|c| c.source),
            protocol_dest_chain_id: request.chains().map(|c| c.dest),
            treasury: request.treasury().map(|t| t.to_checksum_string()),
            token: request.token().map(|t| t.to_checksum_string()),
            request_id: hex32(&request.contract_request_id()),
            obligation_index: request.obligation_index(),
            // A withdrawal's destination travels as `treasury`, not as a
            // free-form `recipient`: the two mean different things to a
            // signer (one is checked against a list, the other is not).
            recipient: match request.payload {
                EvmAuthPayload::TreasuryWithdraw(_) => None,
                _ => request.recipient().map(|r| r.to_checksum_string()),
            },
            amount_robinhood_atomic: request.amount().map(|a| a.get().to_string()),
            signer_epoch: request.signer_epoch(),
            expiry: request.expiry(),
            expected_digest: hex32(&digest),
        })
    }
}

/// Why a custody domain refused to sign.
///
/// Deliberately specific. A signer that answered every refusal with one
/// opaque "denied" would make a misconfiguration on either side
/// indistinguishable from an attack, and an operator debugging a quorum
/// that will not form needs to know which.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmPolicyError {
    #[error(
        "request declares protocol version {actual}, but this signer speaks version {expected}. \
         A new protocol version must be deployed to every custody domain BEFORE it is used"
    )]
    UnknownProtocolVersion { expected: u32, actual: u32 },
    #[error(
        "request carries action byte {action:#04x}, which this signer does not recognize. \
         Abandonment in particular is not a payload this service can produce or this signer can \
         sign"
    )]
    UnknownAction { action: u8 },
    #[error(
        "request kind {kind:?} does not match action byte {action:#04x} — a malformed or \
         deliberately confusing request"
    )]
    KindActionMismatch { kind: String, action: u8 },
    #[error("request names route {route:?}, which is not a route this bridge models")]
    UnknownRoute { route: String },
    #[error("field {field} is malformed: {detail}")]
    MalformedField { field: &'static str, detail: String },
    #[error(
        "field {field} is required for a {kind} authorization and is absent — a signer must \
         never default a field the digest binds"
    )]
    MissingField { field: &'static str, kind: String },
    #[error(
        "field {field} is present on a {kind} authorization, which does not bind it — this \
         payload family has no such field and a request supplying one is not the payload it \
         claims to be"
    )]
    UnexpectedField { field: &'static str, kind: String },
    #[error(
        "request is for verifying contract {actual}, but this signer is configured for \
         {expected}. Refusing to sign for a deployment it does not serve"
    )]
    WrongVerifyingContract { expected: String, actual: String },
    #[error(
        "request is for EVM chain id {actual}, but this signer is configured for {expected} — a \
         signature gathered for one network must never be usable on another"
    )]
    WrongChainId { expected: u64, actual: u64 },
    #[error("request concerns token {actual}, but this signer is configured for {expected}")]
    WrongToken { expected: String, actual: String },
    #[error(
        "route {route} resolves to protocol chain pair ({actual_source}, {actual_dest}) in this \
         request, but this signer independently holds ({expected_source}, {expected_dest})"
    )]
    WrongProtocolChains {
        route: &'static str,
        expected_source: u64,
        expected_dest: u64,
        actual_source: u64,
        actual_dest: u64,
    },
    #[error(
        "the presented credential authorizes actions {allowed:?} only; this request is action \
         {requested:#04x}"
    )]
    ActionNotPermitted { requested: u8, allowed: Vec<u8> },
    #[error(
        "treasury {treasury} is not in THIS signer's independently-held treasury allowlist. \
         Refusing — a treasury address this custody domain has not separately agreed to is \
         refused no matter what else is true of the request or who presented it"
    )]
    TreasuryNotAllowlisted { treasury: String },
    #[error(
        "the presented credential authorizes routes {allowed:?} only; this request is for route \
         {requested}"
    )]
    RouteNotPermitted {
        requested: &'static str,
        allowed: Vec<&'static str>,
    },
    #[error(
        "amount {amount} exceeds this signer's own ceiling of {ceiling} Robinhood atomic units"
    )]
    AmountAboveCeiling { amount: u128, ceiling: u128 },
    #[error(
        "the authorization expired at {expiry} and it is now {now} — refusing to sign an \
         authorization that is already dead"
    )]
    AlreadyExpired { expiry: u64, now: u64 },
    #[error(
        "the authorization is valid for {ttl_secs}s, above this signer's own ceiling of \
         {max_ttl_secs}s — an authorization with a long life is a standing permission slip"
    )]
    TtlAboveCeiling { ttl_secs: u64, max_ttl_secs: u64 },
    #[error(
        "request names signer epoch {actual}, but this signer independently holds {expected} — a \
         quorum signed under a stale epoch is refused on-chain"
    )]
    WrongSignerEpoch { expected: u64, actual: u64 },
    #[error(
        "the authorization could not be encoded: {0}. The request describes something this \
         bridge's own encoder refuses to build"
    )]
    NotEncodable(#[from] crate::robinhood::auth::AuthError),
    #[error(
        "the requester's expected digest is {requested} but this signer independently computes \
         {computed}. The two sides disagree about what this authorization MEANS; neither the \
         requester's digest nor a signature over it may be used"
    )]
    DigestMismatch { requested: String, computed: String },
}

/// What an approved request resolved to.
///
/// The `digest` here is the one the signer COMPUTED. It is the only value
/// a custody domain may ever hand to its HSM/KMS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmAuthDecision {
    pub request: EvmAuthRequest,
    pub digest: [u8; 32],
    /// A one-line human summary for the domain's own audit log. A custody
    /// domain that logs nothing else should log this.
    pub summary: String,
}

/// One EVM custody domain's independently-held signing policy.
///
/// Every field must be provisioned by that domain's own operators,
/// through that domain's own change process. Nothing in it may be
/// derived from, fetched from, or defaulted to anything the bridge host
/// controls — the same rule [`super::policy::SignerPolicy`] states, and
/// for the same reason: if it were, this whole module would be theatre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmSignerPolicy {
    /// The one EIP-155 network this signer serves.
    pub chain_id: EvmChainId,
    /// The one deployed `GlcRobinhoodBridge` this signer serves.
    pub verifying_contract: EvmAddress,
    /// The one ERC-20 that deployment custodies.
    pub token: EvmAddress,
    /// Which action bytes the presented credential may authorize.
    pub allowed_actions: Vec<u8>,
    /// Which routes it may authorize. Held as data rather than derived,
    /// so a domain can decline a route it has not agreed to serve even
    /// after the contract enables it.
    pub allowed_routes: Vec<Route>,
    /// Each served route's protocol chain pair, held INDEPENDENTLY of the
    /// request. Read from the deployed contract's immutables by the
    /// domain's own operators at provisioning time.
    pub route_chains: Vec<(Route, ProtocolChainPair)>,
    /// Treasury addresses this domain has independently agreed to pay
    /// reserve withdrawals to. Should equal the deployed contract's
    /// immutable `TREASURY`, but must be configured separately — the
    /// point is that an attacker has to subvert both. EMPTY means every
    /// treasury withdrawal is refused, which is the correct default for a
    /// domain that has not deliberately opted in. Mirrors
    /// [`super::policy::SignerPolicy::allowed_treasuries`].
    pub allowed_treasuries: Vec<EvmAddress>,
    /// This domain's own ceiling on a single USER-FACING value-moving
    /// authorization, in Robinhood 18-decimal atomic units. Applies to
    /// payouts and refunds. Deliberately NOT to treasury withdrawals: the
    /// withdrawal policy is that any amount of free reserve liquidity —
    /// the whole reserve, once liabilities are cleared — may move to the
    /// immutable treasury, bounded only by the accounting constraints the
    /// contract and ledger enforce. For a withdrawal the destination
    /// allowlist ([`Self::allowed_treasuries`]) is the bound, and it is
    /// the one that matters: an attacker cannot name where the reserve
    /// goes. (This departs from the Solana signer's `max_withdrawal_amount`
    /// on purpose.) A settlement moves nothing and is not bounded by it.
    pub max_amount_robinhood_atomic: u128,
    /// This domain's own ceiling on how long an authorization may live.
    pub max_authorization_ttl_secs: u64,
    /// The contract's `signerEpoch`, when the domain tracks it. `None`
    /// means "do not check", which is the correct default for a domain
    /// that has no independent way to read it — an epoch check performed
    /// against a value taken from the request would be no check at all.
    pub expected_signer_epoch: Option<u64>,
}

impl EvmSignerPolicy {
    /// The full decision.
    ///
    /// Order matters, and mirrors [`super::policy::SignerPolicy::evaluate`]:
    /// shape first, then deployment identity, then credential scope, then
    /// content, then — last, once everything else has been agreed — the
    /// digest. So the error a caller sees names the first and most
    /// fundamental reason the request was refused rather than an
    /// incidental one, and a digest is never computed for a request that
    /// was going to be refused anyway.
    ///
    /// `now` is passed in rather than read from the clock so a domain can
    /// use its own trusted time source, and so this is testable.
    pub fn evaluate(
        &self,
        document: &EvmAuthSignRequest,
        now: u64,
    ) -> Result<EvmAuthDecision, EvmPolicyError> {
        // ---- shape ----
        if document.protocol_version != EVM_AUTH_PROTOCOL_VERSION {
            return Err(EvmPolicyError::UnknownProtocolVersion {
                expected: EVM_AUTH_PROTOCOL_VERSION,
                actual: document.protocol_version,
            });
        }
        let expected_kind = match document.action {
            ACTION_PAYOUT => "payout",
            ACTION_REFUND => "refund",
            ACTION_SETTLE => "settlement",
            ACTION_TREASURY_WITHDRAW => "treasury_withdraw",
            other => return Err(EvmPolicyError::UnknownAction { action: other }),
        };
        if document.kind != expected_kind {
            return Err(EvmPolicyError::KindActionMismatch {
                kind: document.kind.clone(),
                action: document.action,
            });
        }
        // A withdrawal binds no route; everything else binds exactly one.
        // Neither shape may borrow the other's fields.
        let route: Option<Route> = if document.action == ACTION_TREASURY_WITHDRAW {
            reject_present("route", expected_kind, document.route.is_some())?;
            reject_present(
                "protocol_source_chain_id",
                expected_kind,
                document.protocol_source_chain_id.is_some(),
            )?;
            reject_present(
                "protocol_dest_chain_id",
                expected_kind,
                document.protocol_dest_chain_id.is_some(),
            )?;
            None
        } else {
            reject_present("treasury", expected_kind, document.treasury.is_some())?;
            let raw = require("route", expected_kind, document.route.as_deref())?;
            Some(raw.parse().map_err(|_| EvmPolicyError::UnknownRoute {
                route: raw.to_string(),
            })?)
        };

        // ---- deployment identity, held independently ----
        let verifying_contract = parse_address("verifying_contract", &document.verifying_contract)?;
        if verifying_contract != self.verifying_contract {
            return Err(EvmPolicyError::WrongVerifyingContract {
                expected: self.verifying_contract.to_checksum_string(),
                actual: verifying_contract.to_checksum_string(),
            });
        }
        if document.chain_id != self.chain_id.get() {
            return Err(EvmPolicyError::WrongChainId {
                expected: self.chain_id.get(),
                actual: document.chain_id,
            });
        }

        // ---- credential scope ----
        if !self.allowed_actions.contains(&document.action) {
            return Err(EvmPolicyError::ActionNotPermitted {
                requested: document.action,
                allowed: self.allowed_actions.clone(),
            });
        }
        // ---- the route and its protocol chain pair, held independently ----
        let route_and_chains: Option<(Route, ProtocolChainPair)> = match route {
            None => None,
            Some(route) => {
                if !self.allowed_routes.contains(&route) {
                    return Err(EvmPolicyError::RouteNotPermitted {
                        requested: route.as_str(),
                        allowed: self.allowed_routes.iter().map(|r| r.as_str()).collect(),
                    });
                }
                let expected_chains = self
                    .route_chains
                    .iter()
                    .find(|(r, _)| *r == route)
                    .map(|(_, pair)| *pair)
                    .ok_or(EvmPolicyError::RouteNotPermitted {
                        requested: route.as_str(),
                        allowed: self.route_chains.iter().map(|(r, _)| r.as_str()).collect(),
                    })?;
                let actual_source = require_value(
                    "protocol_source_chain_id",
                    expected_kind,
                    document.protocol_source_chain_id,
                )?;
                let actual_dest = require_value(
                    "protocol_dest_chain_id",
                    expected_kind,
                    document.protocol_dest_chain_id,
                )?;
                if expected_chains.source != actual_source || expected_chains.dest != actual_dest {
                    return Err(EvmPolicyError::WrongProtocolChains {
                        route: route.as_str(),
                        expected_source: expected_chains.source,
                        expected_dest: expected_chains.dest,
                        actual_source,
                        actual_dest,
                    });
                }
                Some((
                    route,
                    ProtocolChainPair {
                        source: actual_source,
                        dest: actual_dest,
                    },
                ))
            }
        };

        // ---- lifetime ----
        if document.expiry <= now {
            return Err(EvmPolicyError::AlreadyExpired {
                expiry: document.expiry,
                now,
            });
        }
        let ttl = document.expiry - now;
        if ttl > self.max_authorization_ttl_secs {
            return Err(EvmPolicyError::TtlAboveCeiling {
                ttl_secs: ttl,
                max_ttl_secs: self.max_authorization_ttl_secs,
            });
        }
        if let Some(expected) = self.expected_signer_epoch {
            if document.signer_epoch != expected {
                return Err(EvmPolicyError::WrongSignerEpoch {
                    expected,
                    actual: document.signer_epoch,
                });
            }
        }

        // ---- reconstruct the exact authorization ----
        let request_id = parse_bytes32("request_id", &document.request_id)?;
        let domain = BridgeDomain::new(self.chain_id, self.verifying_contract);
        // Every route-bound arm below unwraps this; the withdrawal arm
        // does not touch it. The shape check above already guaranteed the
        // pairing, so the `expect` states an invariant, not a hope.
        let bound =
            || route_and_chains.expect("a route-bound action carries a route and its chain pair");
        let payload = match document.action {
            ACTION_PAYOUT => {
                let (route, chains) = bound();
                reject_present(
                    "obligation_index",
                    expected_kind,
                    document.obligation_index.is_some(),
                )?;
                let token = self.require_token(document, expected_kind)?;
                let recipient = parse_address(
                    "recipient",
                    require("recipient", expected_kind, document.recipient.as_deref())?,
                )?;
                let amount = self.require_amount(document, expected_kind)?;
                EvmAuthPayload::Payout(PayoutAuth {
                    route,
                    chains,
                    token,
                    request_id,
                    recipient,
                    amount,
                    signer_epoch: document.signer_epoch,
                    expiry: document.expiry,
                })
            }
            ACTION_REFUND => {
                let (route, chains) = bound();
                let token = self.require_token(document, expected_kind)?;
                let obligation_index =
                    require_value("obligation_index", expected_kind, document.obligation_index)?;
                let recipient = parse_address(
                    "recipient",
                    require("recipient", expected_kind, document.recipient.as_deref())?,
                )?;
                let amount = self.require_amount(document, expected_kind)?;
                EvmAuthPayload::Refund(RefundAuth {
                    route,
                    chains,
                    token,
                    request_id,
                    obligation_index,
                    recipient,
                    amount,
                    signer_epoch: document.signer_epoch,
                    expiry: document.expiry,
                })
            }
            ACTION_SETTLE => {
                let (route, chains) = bound();
                // A settlement moves no tokens and names no recipient.
                // A request that supplies either is not the payload it
                // claims to be, and quietly ignoring the extra field
                // would let a caller believe it had bound something the
                // digest does not cover.
                reject_present("token", expected_kind, document.token.is_some())?;
                reject_present("recipient", expected_kind, document.recipient.is_some())?;
                reject_present(
                    "amount_robinhood_atomic",
                    expected_kind,
                    document.amount_robinhood_atomic.is_some(),
                )?;
                let obligation_index =
                    require_value("obligation_index", expected_kind, document.obligation_index)?;
                EvmAuthPayload::Settlement(SettlementAuth {
                    route,
                    chains,
                    request_id,
                    obligation_index,
                    signer_epoch: document.signer_epoch,
                    expiry: document.expiry,
                })
            }
            ACTION_TREASURY_WITHDRAW => {
                // No obligation, no free-form recipient: the destination
                // IS the treasury field, and it is checked against this
                // domain's own list before anything else about it.
                reject_present(
                    "obligation_index",
                    expected_kind,
                    document.obligation_index.is_some(),
                )?;
                reject_present("recipient", expected_kind, document.recipient.is_some())?;
                let token = self.require_token(document, expected_kind)?;
                let treasury = parse_address(
                    "treasury",
                    require("treasury", expected_kind, document.treasury.as_deref())?,
                )?;
                // THE check. An address this domain did not separately
                // agree to is refused no matter what else is true of the
                // request or who presented it — the same rule the Solana
                // signer policy states for its own treasury allowlist.
                if !self.allowed_treasuries.contains(&treasury) {
                    return Err(EvmPolicyError::TreasuryNotAllowlisted {
                        treasury: treasury.to_checksum_string(),
                    });
                }
                // NO ceiling. The treasury-withdrawal policy is that an
                // operator may move any amount of FREE reserve liquidity —
                // up to and including the entire reserve once liabilities
                // are cleared — to the immutable treasury, and that the
                // only bounds are the accounting ones the contract and the
                // ledger enforce (protected floor, unsettled principal,
                // reserved liquidity, pending obligations). A per-domain
                // amount ceiling would be exactly the artificial cap that
                // policy forbids, and would make a deliberate drain
                // impossible without first weakening every domain's payout
                // ceiling. The destination allowlist above is the bound.
                let amount = parse_amount_unbounded(document, expected_kind)?;
                EvmAuthPayload::TreasuryWithdraw(TreasuryWithdrawAuth {
                    token,
                    request_id,
                    treasury,
                    amount,
                    signer_epoch: document.signer_epoch,
                    expiry: document.expiry,
                })
            }
            other => return Err(EvmPolicyError::UnknownAction { action: other }),
        };

        let request = EvmAuthRequest { domain, payload };
        // The encoder's own route/leg checks run here: a payout on a
        // deposit route, or a refund on a payout route, has no digest at
        // all.
        let digest = request.digest()?;

        // ---- and only now, the cross-check ----
        let requested_digest = parse_bytes32("expected_digest", &document.expected_digest)?;
        if requested_digest != digest {
            return Err(EvmPolicyError::DigestMismatch {
                requested: hex32(&requested_digest),
                computed: hex32(&digest),
            });
        }

        let summary = request.summary();
        Ok(EvmAuthDecision {
            request,
            digest,
            summary,
        })
    }

    fn require_token(
        &self,
        document: &EvmAuthSignRequest,
        kind: &str,
    ) -> Result<EvmAddress, EvmPolicyError> {
        let token = parse_address("token", require("token", kind, document.token.as_deref())?)?;
        if token != self.token {
            return Err(EvmPolicyError::WrongToken {
                expected: self.token.to_checksum_string(),
                actual: token.to_checksum_string(),
            });
        }
        Ok(token)
    }

    fn require_amount(
        &self,
        document: &EvmAuthSignRequest,
        kind: &str,
    ) -> Result<RobinhoodAtomic, EvmPolicyError> {
        let raw = require(
            "amount_robinhood_atomic",
            kind,
            document.amount_robinhood_atomic.as_deref(),
        )?;
        let value: u128 = raw.parse().map_err(|_| EvmPolicyError::MalformedField {
            field: "amount_robinhood_atomic",
            detail: format!(
                "{raw:?} is not a decimal integer in Robinhood 18-decimal atomic units"
            ),
        })?;
        if value > self.max_amount_robinhood_atomic {
            return Err(EvmPolicyError::AmountAboveCeiling {
                amount: value,
                ceiling: self.max_amount_robinhood_atomic,
            });
        }
        Ok(RobinhoodAtomic::new(value))
    }
}

/// The amount field, parsed and NOT compared against any ceiling — for
/// the one payload family whose policy is "no artificial amount limit".
fn parse_amount_unbounded(
    document: &EvmAuthSignRequest,
    kind: &str,
) -> Result<RobinhoodAtomic, EvmPolicyError> {
    let raw = require(
        "amount_robinhood_atomic",
        kind,
        document.amount_robinhood_atomic.as_deref(),
    )?;
    let value: u128 = raw.parse().map_err(|_| EvmPolicyError::MalformedField {
        field: "amount_robinhood_atomic",
        detail: format!("{raw:?} is not a decimal integer in Robinhood 18-decimal atomic units"),
    })?;
    if value == 0 {
        return Err(EvmPolicyError::MalformedField {
            field: "amount_robinhood_atomic",
            detail: "a withdrawal of zero is not a withdrawal".to_string(),
        });
    }
    Ok(RobinhoodAtomic::new(value))
}

fn require<'a>(
    field: &'static str,
    kind: &str,
    value: Option<&'a str>,
) -> Result<&'a str, EvmPolicyError> {
    value.ok_or(EvmPolicyError::MissingField {
        field,
        kind: kind.to_string(),
    })
}

fn require_value<T>(
    field: &'static str,
    kind: &str,
    value: Option<T>,
) -> Result<T, EvmPolicyError> {
    value.ok_or(EvmPolicyError::MissingField {
        field,
        kind: kind.to_string(),
    })
}

fn reject_present(field: &'static str, kind: &str, present: bool) -> Result<(), EvmPolicyError> {
    if present {
        return Err(EvmPolicyError::UnexpectedField {
            field,
            kind: kind.to_string(),
        });
    }
    Ok(())
}

fn parse_address(field: &'static str, value: &str) -> Result<EvmAddress, EvmPolicyError> {
    value.parse().map_err(|e| EvmPolicyError::MalformedField {
        field,
        detail: format!("{e}"),
    })
}

fn parse_bytes32(field: &'static str, value: &str) -> Result<[u8; 32], EvmPolicyError> {
    crate::evm::hex::decode_fixed::<32>(value).map_err(|e| EvmPolicyError::MalformedField {
        field,
        detail: format!("{e}"),
    })
}

/// Lowercase `0x`-prefixed hex for a 32-byte value. The one spelling this
/// protocol uses for `request_id` and `expected_digest`, so a comparison
/// between two of them can never fail on case alone.
///
/// [`crate::evm::hex::encode_lower`] already emits the `0x`; this is a
/// thin alias so every call site in the protocol spells it one way.
pub fn hex32(bytes: &[u8; 32]) -> String {
    crate::evm::hex::encode_lower(bytes)
}

/// Amount ceiling meaning "no ceiling this signer can express" — the
/// widest a Robinhood atomic amount can be. Offered so a deployment that
/// deliberately relies on the contract's own limits alone says so
/// explicitly rather than by writing a large number.
pub const NO_AMOUNT_CEILING: u128 = u128::MAX;

/// Sanity helper for a domain provisioning [`EvmSignerPolicy::
/// max_amount_robinhood_atomic`] from a whole-GLC figure.
pub fn glc_to_robinhood_atomic(whole_glc: u64) -> Option<u128> {
    u128::from(whole_glc).checked_mul(10u128.pow(18))
}

/// The ABI word a policy ceiling corresponds to, for operator display.
pub fn ceiling_as_u256(ceiling: u128) -> EvmU256 {
    EvmU256::from_u128(ceiling)
}

#[cfg(test)]
mod tests;
