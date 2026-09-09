//! Signer-side policy for Robinhood GOVERNANCE authorizations: the
//! `/v3/sign-evm-governance` protocol, and the independent decision a
//! custody domain makes before it signs a `setLimits`, `setPaused` or
//! `setRouteEnabled`.
//!
//! # Why this is a new protocol version and not a widened v2
//!
//! [`super::evm_policy`]'s `EvmAuthSignRequest` describes a VALUE
//! MOVEMENT: it requires a route, a request id and a protocol chain pair,
//! and it has no nonce. A governance authorization has none of those and
//! needs one — the fields do not overlap, and bolting optional governance
//! fields onto the v2 document would mean every existing payout request
//! newly carries fields whose absence has to be re-validated. The v2
//! document, its policy, its endpoint and its tests are therefore
//! **untouched by this module**; a signer that serves only v2 keeps
//! behaving exactly as it did, and answers `404` here.
//!
//! That is the deployment ordering [`super::evm_policy`] already states,
//! made structural: governance cannot be requested from a signer that has
//! not been upgraded, and an upgraded signer still refuses governance
//! until its operators opt in.
//!
//! # Rule zero still holds: the digest is an OUTPUT
//!
//! There is no endpoint here that accepts bytes or a digest to sign, and
//! adding one would rebuild the blind oracle
//! [`super::policy`]'s docs exist to describe. The request carries the
//! STRUCTURED FIELDS of the proposed change — the seven limit values, or
//! the two pause booleans, or the route and its flag — and the signer
//! reconstructs the payload hash, the struct hash and the EIP-712 digest
//! itself, using [`crate::robinhood::governance`], the same single
//! encoder the proposing tool used. It signs the digest IT derived.
//! `expected_digest` is carried only so a disagreement is a refusal
//! rather than something either side papers over.
//!
//! # Governance is OFF unless a domain turns it on
//!
//! [`EvmGovernancePolicy::allowed_actions`] defaults to EMPTY, and an
//! empty allow-list refuses everything
//! ([`EvmGovernanceError::GovernanceDisabled`]). Deploying this code to a
//! signer changes nothing about what that signer will sign; a domain's
//! operators must additionally set
//! `GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS`. Widening what a custody
//! key may authorize is a decision each domain makes for itself, through
//! its own change process — never a consequence of a deploy.
//!
//! # What a domain can still never sign here
//!
//! `rotateSigners`, `rotateGuardians`, `commitMigration`,
//! `finalizeMigration` and `ACTION_ABANDON` have no representation in
//! [`crate::robinhood::governance`] and none here. A request naming one
//! is refused as an unknown action, which is the same fail-closed posture
//! abandonment already gets on the v2 path.

use serde::{Deserialize, Serialize};

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::{EvmAddress, EvmChainId, EvmU256};
use crate::robinhood::auth::BridgeDomain;
use crate::robinhood::calls::BridgeLimits;
use crate::robinhood::governance::{
    validate_limits, GovernanceAuth, GovernanceError, GovernancePayload, ACTION_SET_LIMITS,
    ACTION_SET_PAUSE, ACTION_SET_ROUTE_ENABLED,
};
use crate::routes::Route;

/// The version of the governance request document below.
///
/// `3` because it is the THIRD protocol this signer deployment speaks:
/// `/v1/sign` (raw payload bytes) and `/v2/sign-evm-auth` (value-moving
/// EIP-712 authorizations) are unchanged and independent.
pub const EVM_GOVERNANCE_PROTOCOL_VERSION: u32 = 3;

/// The HTTP path this protocol is served on.
pub const EVM_GOVERNANCE_PATH: &str = "/v3/sign-evm-governance";

/// The wire spelling of every action a domain may be configured to allow.
pub const GOVERNANCE_ACTION_NAMES: &[(&str, u8)] = &[
    ("set_limits", ACTION_SET_LIMITS),
    ("set_pause", ACTION_SET_PAUSE),
    ("set_route_enabled", ACTION_SET_ROUTE_ENABLED),
];

/// The seven `Limits` members, as decimal strings in Robinhood 18-decimal
/// atomic units.
///
/// Strings rather than integers because a `uint256` does not fit any JSON
/// number a parser will preserve exactly, and a limit silently rounded by
/// a JSON float would be a limit nobody stated. Decimal rather than hex
/// so a human reviewing a signer's audit log reads the same figures the
/// proposal printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GovernanceLimitsFields {
    pub inbound_min: String,
    pub inbound_max: String,
    pub inbound_rolling_limit: String,
    pub outbound_min: String,
    pub outbound_max: String,
    pub outbound_rolling_limit: String,
    pub protected_min_reserve: String,
}

/// The body of a `POST /v3/sign-evm-governance` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvmGovernanceSignRequest {
    /// Must equal [`EVM_GOVERNANCE_PROTOCOL_VERSION`]. Checked FIRST.
    pub protocol_version: u32,
    /// `"set_limits"` | `"set_pause"` | `"set_route_enabled"`. Redundant
    /// with `action` on purpose: the two must agree.
    pub kind: String,
    /// The contract's action discriminator (`0x07`/`0x04`/`0x0B`).
    pub action: u8,
    /// The EIP-155 chain id the EIP-712 domain binds.
    pub chain_id: u64,
    /// The deployed `GlcRobinhoodBridge`, `0x`-prefixed hex.
    pub verifying_contract: String,
    /// The contract's `signerEpoch` at proposal time.
    pub signer_epoch: u64,
    /// The contract's `governanceNonce`, as a decimal string. Strict
    /// equality on chain, so a stale value authorizes nothing.
    pub nonce: String,
    /// Unix seconds after which the contract refuses this authorization.
    pub expiry: u64,
    /// Present exactly for `set_limits`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<GovernanceLimitsFields>,
    /// Present exactly for `set_pause`. Both, always — the contract takes
    /// both, so naming one would hide the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deposits_paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payouts_paused: Option<bool>,
    /// Present exactly for `set_route_enabled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_enabled: Option<bool>,
    /// The PROPOSER's own computed digest, `0x`-prefixed hex. Never what
    /// gets signed — see the module docs.
    pub expected_digest: String,
}

impl EvmGovernanceSignRequest {
    /// Builds the wire document for an authorization already constructed
    /// by [`crate::robinhood::governance`].
    pub fn from_auth(
        auth: &GovernanceAuth,
        domain: BridgeDomain,
    ) -> Result<EvmGovernanceSignRequest, GovernanceError> {
        let digest = auth.digest(domain)?;
        let mut document = EvmGovernanceSignRequest {
            protocol_version: EVM_GOVERNANCE_PROTOCOL_VERSION,
            kind: auth.payload.kind_str().to_string(),
            action: auth.payload.action(),
            chain_id: domain.chain_id.get(),
            verifying_contract: domain.verifying_contract.to_checksum_string(),
            signer_epoch: auth.signer_epoch,
            nonce: u256_to_decimal(auth.nonce),
            expiry: auth.expiry,
            limits: None,
            deposits_paused: None,
            payouts_paused: None,
            route: None,
            route_enabled: None,
            expected_digest: crate::evm::hex::encode_lower(&digest),
        };
        match &auth.payload {
            GovernancePayload::SetLimits(limits) => {
                document.limits = Some(GovernanceLimitsFields {
                    inbound_min: u256_to_decimal(limits.inbound_min),
                    inbound_max: u256_to_decimal(limits.inbound_max),
                    inbound_rolling_limit: u256_to_decimal(limits.inbound_rolling_limit),
                    outbound_min: u256_to_decimal(limits.outbound_min),
                    outbound_max: u256_to_decimal(limits.outbound_max),
                    outbound_rolling_limit: u256_to_decimal(limits.outbound_rolling_limit),
                    protected_min_reserve: u256_to_decimal(limits.protected_min_reserve),
                });
            }
            GovernancePayload::SetPaused {
                deposits_paused,
                payouts_paused,
            } => {
                document.deposits_paused = Some(*deposits_paused);
                document.payouts_paused = Some(*payouts_paused);
            }
            GovernancePayload::SetRouteEnabled { route, enabled } => {
                document.route = Some(route.as_str().to_string());
                document.route_enabled = Some(*enabled);
            }
        }
        Ok(document)
    }
}

/// Decimal rendering of a `uint256` that this protocol can carry.
///
/// Values above `u128::MAX` are refused everywhere they are parsed, so
/// rendering one is unreachable; it produces the hex word rather than a
/// wrong decimal, which fails the round trip loudly instead of quietly.
fn u256_to_decimal(value: EvmU256) -> String {
    match value.try_to_u128() {
        Ok(v) => v.to_string(),
        Err(_) => value.to_word_hex(),
    }
}

/// Why a governance signing request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvmGovernanceError {
    #[error(
        "request declares governance protocol version {actual}, but this signer speaks version \
         {expected}. A new protocol version must be deployed to every custody domain BEFORE it \
         is used"
    )]
    UnknownProtocolVersion { expected: u32, actual: u32 },
    #[error(
        "this signer is not configured to authorize ANY governance action. Governance is off \
         until a domain's own operators set GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS — \
         deploying the code does not turn it on"
    )]
    GovernanceDisabled,
    #[error(
        "request carries action byte {action:#04x}, which is not a governance action this signer \
         recognizes. Signer rotation, guardian rotation, migration and abandonment are \
         deliberately unrepresentable here"
    )]
    UnknownAction { action: u8 },
    #[error(
        "request kind {kind:?} does not match action byte {action:#04x} — a malformed or \
         deliberately confusing request"
    )]
    KindActionMismatch { kind: String, action: u8 },
    #[error(
        "the presented credential authorizes governance actions {allowed:?} only; this request \
         is action {requested:#04x}"
    )]
    ActionNotPermitted { requested: u8, allowed: Vec<u8> },
    #[error(
        "request is for verifying contract {actual}, but this signer is configured for \
         {expected}. Refusing to sign for a deployment it does not serve"
    )]
    WrongVerifyingContract { expected: String, actual: String },
    #[error(
        "request is for EVM chain id {actual}, but this signer is configured for {expected} — a \
         governance signature gathered for one network must never be usable on another"
    )]
    WrongChainId { expected: u64, actual: u64 },
    #[error(
        "request names signer epoch {actual}, but this signer independently holds {expected} — a \
         quorum signed under a stale epoch is refused on-chain"
    )]
    WrongSignerEpoch { expected: u64, actual: u64 },
    #[error("field {field} is malformed: {detail}")]
    MalformedField { field: &'static str, detail: String },
    #[error(
        "field {field} is required for a {kind} governance authorization and is absent — a \
         signer must never default a field the digest binds"
    )]
    MissingField { field: &'static str, kind: String },
    #[error(
        "field {field} is present on a {kind} governance authorization, which does not bind it — \
         a request supplying it is not the payload it claims to be"
    )]
    UnexpectedField { field: &'static str, kind: String },
    #[error(
        "the authorization expired at {expiry} and it is now {now} — refusing to sign an \
         authorization that is already dead"
    )]
    AlreadyExpired { expiry: u64, now: u64 },
    #[error(
        "the authorization is valid for {ttl_secs}s, above this signer's own ceiling of \
         {max_ttl_secs}s — a governance authorization with a long life is a standing permission \
         slip over the contract's own policy"
    )]
    TtlAboveCeiling { ttl_secs: u64, max_ttl_secs: u64 },
    #[error("the governance authorization could not be encoded: {0}")]
    Encoding(#[from] GovernanceError),
    #[error(
        "the requester's expected_digest {requested} is not the digest this signer derived from \
         the request's own fields ({derived}). The two sides disagree about what this \
         authorization MEANS; neither may be signed"
    )]
    DigestMismatch { requested: String, derived: String },
}

/// A governance request this signer has agreed to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmGovernanceDecision {
    pub auth: GovernanceAuth,
    /// The ONLY 32 bytes that may reach a signing key for this request.
    pub digest: [u8; 32],
    /// A one-line human summary for the domain's own audit log.
    pub summary: String,
}

/// One EVM custody domain's independently-held GOVERNANCE policy.
///
/// Separate from [`super::evm_policy::EvmSignerPolicy`] rather than a
/// field on it, so that adding governance support cannot alter the
/// evaluation of a single value-moving authorization. Every field is
/// provisioned by the domain's own operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmGovernancePolicy {
    /// The one EIP-155 network this signer serves.
    pub chain_id: EvmChainId,
    /// The one deployed `GlcRobinhoodBridge` this signer serves.
    pub verifying_contract: EvmAddress,
    /// Which governance action bytes this credential may authorize.
    ///
    /// EMPTY BY DEFAULT, and empty means refuse everything. Governance is
    /// opt-in per domain.
    pub allowed_actions: Vec<u8>,
    /// This domain's own ceiling on how long a governance authorization
    /// may live.
    pub max_authorization_ttl_secs: u64,
    /// The contract's `signerEpoch`, when the domain tracks it. `None`
    /// means "do not check" — an epoch check against a value taken from
    /// the request would be no check at all.
    pub expected_signer_epoch: Option<u64>,
}

impl EvmGovernancePolicy {
    /// Whether this domain will consider any governance request at all.
    pub fn is_enabled(&self) -> bool {
        !self.allowed_actions.is_empty()
    }

    /// The full decision.
    ///
    /// Order mirrors [`super::evm_policy::EvmSignerPolicy::evaluate`]:
    /// shape, then deployment identity, then credential scope, then
    /// content, then — last — the digest. So the error names the first
    /// and most fundamental reason, and no digest is derived for a
    /// request that was going to be refused anyway.
    pub fn evaluate(
        &self,
        document: &EvmGovernanceSignRequest,
        now: u64,
    ) -> Result<EvmGovernanceDecision, EvmGovernanceError> {
        // ---- shape ----
        if document.protocol_version != EVM_GOVERNANCE_PROTOCOL_VERSION {
            return Err(EvmGovernanceError::UnknownProtocolVersion {
                expected: EVM_GOVERNANCE_PROTOCOL_VERSION,
                actual: document.protocol_version,
            });
        }
        // Before anything else about the request is interpreted: a domain
        // that has not opted in says so, rather than reporting whichever
        // field happened to be wrong.
        if !self.is_enabled() {
            return Err(EvmGovernanceError::GovernanceDisabled);
        }
        let expected_kind = match document.action {
            ACTION_SET_LIMITS => "set_limits",
            ACTION_SET_PAUSE => "set_pause",
            ACTION_SET_ROUTE_ENABLED => "set_route_enabled",
            other => return Err(EvmGovernanceError::UnknownAction { action: other }),
        };
        if document.kind != expected_kind {
            return Err(EvmGovernanceError::KindActionMismatch {
                kind: document.kind.clone(),
                action: document.action,
            });
        }

        // ---- deployment identity, held independently ----
        let verifying_contract = parse_address("verifying_contract", &document.verifying_contract)?;
        if verifying_contract != self.verifying_contract {
            return Err(EvmGovernanceError::WrongVerifyingContract {
                expected: self.verifying_contract.to_checksum_string(),
                actual: verifying_contract.to_checksum_string(),
            });
        }
        if document.chain_id != self.chain_id.get() {
            return Err(EvmGovernanceError::WrongChainId {
                expected: self.chain_id.get(),
                actual: document.chain_id,
            });
        }
        if let Some(expected) = self.expected_signer_epoch {
            if document.signer_epoch != expected {
                return Err(EvmGovernanceError::WrongSignerEpoch {
                    expected,
                    actual: document.signer_epoch,
                });
            }
        }

        // ---- credential scope ----
        if !self.allowed_actions.contains(&document.action) {
            return Err(EvmGovernanceError::ActionNotPermitted {
                requested: document.action,
                allowed: self.allowed_actions.clone(),
            });
        }

        // ---- lifetime ----
        if document.expiry <= now {
            return Err(EvmGovernanceError::AlreadyExpired {
                expiry: document.expiry,
                now,
            });
        }
        let ttl_secs = document.expiry - now;
        if ttl_secs > self.max_authorization_ttl_secs {
            return Err(EvmGovernanceError::TtlAboveCeiling {
                ttl_secs,
                max_ttl_secs: self.max_authorization_ttl_secs,
            });
        }

        // ---- content ----
        let payload = self.reconstruct_payload(document, expected_kind)?;
        let nonce = parse_u256("nonce", &document.nonce)?;
        let auth = GovernanceAuth {
            payload,
            signer_epoch: document.signer_epoch,
            nonce,
            expiry: document.expiry,
        };

        // ---- and only now, the digest ----
        let domain = BridgeDomain::new(self.chain_id, self.verifying_contract);
        let digest = auth.digest(domain)?;
        let requested = parse_bytes32("expected_digest", &document.expected_digest)?;
        if requested != digest {
            return Err(EvmGovernanceError::DigestMismatch {
                requested: crate::evm::hex::encode_lower(&requested),
                derived: crate::evm::hex::encode_lower(&digest),
            });
        }

        let summary = summarize(&auth);
        Ok(EvmGovernanceDecision {
            auth,
            digest,
            summary,
        })
    }

    /// Rebuilds the proposed change from the request's structured fields,
    /// refusing every field that does not belong to the named kind.
    ///
    /// Quietly ignoring a foreign field would let a caller believe it had
    /// bound something the digest does not cover — the same reasoning the
    /// v2 path applies to a settlement that supplies an amount.
    fn reconstruct_payload(
        &self,
        document: &EvmGovernanceSignRequest,
        kind: &str,
    ) -> Result<GovernancePayload, EvmGovernanceError> {
        match document.action {
            ACTION_SET_LIMITS => {
                reject_present("deposits_paused", kind, document.deposits_paused.is_some())?;
                reject_present("payouts_paused", kind, document.payouts_paused.is_some())?;
                reject_present("route", kind, document.route.is_some())?;
                reject_present("route_enabled", kind, document.route_enabled.is_some())?;
                let fields =
                    document
                        .limits
                        .as_ref()
                        .ok_or_else(|| EvmGovernanceError::MissingField {
                            field: "limits",
                            kind: kind.to_string(),
                        })?;
                let limits = BridgeLimits {
                    inbound_min: parse_u256("limits.inbound_min", &fields.inbound_min)?,
                    inbound_max: parse_u256("limits.inbound_max", &fields.inbound_max)?,
                    inbound_rolling_limit: parse_u256(
                        "limits.inbound_rolling_limit",
                        &fields.inbound_rolling_limit,
                    )?,
                    outbound_min: parse_u256("limits.outbound_min", &fields.outbound_min)?,
                    outbound_max: parse_u256("limits.outbound_max", &fields.outbound_max)?,
                    outbound_rolling_limit: parse_u256(
                        "limits.outbound_rolling_limit",
                        &fields.outbound_rolling_limit,
                    )?,
                    protected_min_reserve: parse_u256(
                        "limits.protected_min_reserve",
                        &fields.protected_min_reserve,
                    )?,
                };
                // The signer applies the contract's own limit rules
                // itself. A domain that signed a limit set the contract
                // would revert on has spent a nonce and a quorum's
                // attention on nothing.
                validate_limits(&limits)?;
                Ok(GovernancePayload::SetLimits(limits))
            }
            ACTION_SET_PAUSE => {
                reject_present("limits", kind, document.limits.is_some())?;
                reject_present("route", kind, document.route.is_some())?;
                reject_present("route_enabled", kind, document.route_enabled.is_some())?;
                Ok(GovernancePayload::SetPaused {
                    deposits_paused: require_bool(
                        "deposits_paused",
                        kind,
                        document.deposits_paused,
                    )?,
                    payouts_paused: require_bool("payouts_paused", kind, document.payouts_paused)?,
                })
            }
            ACTION_SET_ROUTE_ENABLED => {
                reject_present("limits", kind, document.limits.is_some())?;
                reject_present("deposits_paused", kind, document.deposits_paused.is_some())?;
                reject_present("payouts_paused", kind, document.payouts_paused.is_some())?;
                let raw =
                    document
                        .route
                        .as_deref()
                        .ok_or_else(|| EvmGovernanceError::MissingField {
                            field: "route",
                            kind: kind.to_string(),
                        })?;
                let route: Route = raw
                    .parse()
                    .map_err(|_| EvmGovernanceError::MalformedField {
                        field: "route",
                        detail: format!("{raw:?} is not a route this bridge models"),
                    })?;
                // `GovernancePayload::payload_hash` refuses SolToRhn and
                // RhnToSol, so the refusal happens whether or not this
                // signer thought to check. Building the payload here is
                // enough; the encoder is the authority.
                Ok(GovernancePayload::SetRouteEnabled {
                    route,
                    enabled: require_bool("route_enabled", kind, document.route_enabled)?,
                })
            }
            other => Err(EvmGovernanceError::UnknownAction { action: other }),
        }
    }
}

/// One line naming exactly what was agreed, for a domain's audit log.
fn summarize(auth: &GovernanceAuth) -> String {
    let what = match &auth.payload {
        GovernancePayload::SetLimits(l) => format!(
            "setLimits(inMin={}, inMax={}, inRoll={}, outMin={}, outMax={}, outRoll={}, \
             protectedMin={})",
            u256_to_decimal(l.inbound_min),
            u256_to_decimal(l.inbound_max),
            u256_to_decimal(l.inbound_rolling_limit),
            u256_to_decimal(l.outbound_min),
            u256_to_decimal(l.outbound_max),
            u256_to_decimal(l.outbound_rolling_limit),
            u256_to_decimal(l.protected_min_reserve),
        ),
        GovernancePayload::SetPaused {
            deposits_paused,
            payouts_paused,
        } => format!("setPaused(deposits={deposits_paused}, payouts={payouts_paused})"),
        GovernancePayload::SetRouteEnabled { route, enabled } => {
            format!("setRouteEnabled({}, {enabled})", route.as_str())
        }
    };
    format!(
        "governance {what} nonce={} epoch={} expiry={}",
        u256_to_decimal(auth.nonce),
        auth.signer_epoch,
        auth.expiry
    )
}

fn require_bool(
    field: &'static str,
    kind: &str,
    value: Option<bool>,
) -> Result<bool, EvmGovernanceError> {
    value.ok_or(EvmGovernanceError::MissingField {
        field,
        kind: kind.to_string(),
    })
}

fn reject_present(
    field: &'static str,
    kind: &str,
    present: bool,
) -> Result<(), EvmGovernanceError> {
    if present {
        return Err(EvmGovernanceError::UnexpectedField {
            field,
            kind: kind.to_string(),
        });
    }
    Ok(())
}

fn parse_address(field: &'static str, value: &str) -> Result<EvmAddress, EvmGovernanceError> {
    value
        .parse()
        .map_err(|e| EvmGovernanceError::MalformedField {
            field,
            detail: format!("{e}"),
        })
}

fn parse_bytes32(field: &'static str, value: &str) -> Result<[u8; 32], EvmGovernanceError> {
    crate::evm::hex::decode_fixed::<32>(value).map_err(|e| EvmGovernanceError::MalformedField {
        field,
        detail: format!("{e}"),
    })
}

/// Parses a decimal `uint256` this protocol can carry.
///
/// Bounded at `u128::MAX`, which is not a limitation in practice:
/// [`crate::robinhood::governance::validate_limits`] already refuses a
/// limit above it, and `governanceNonce` increments by one per governance
/// action. A value above the bound is refused rather than truncated.
fn parse_u256(field: &'static str, value: &str) -> Result<EvmU256, EvmGovernanceError> {
    let parsed: u128 = value
        .parse()
        .map_err(|e| EvmGovernanceError::MalformedField {
            field,
            detail: format!("{value:?} is not a decimal integer this protocol can carry: {e}"),
        })?;
    Ok(EvmU256::from_u128(parsed))
}

/// The action byte for a wire spelling, for configuration parsing.
pub fn governance_action_from_name(name: &str) -> Option<u8> {
    GOVERNANCE_ACTION_NAMES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, a)| *a)
}

/// Robinhood atomic units from a decimal string, for CLI override
/// parsing. Here so the one parser this protocol uses is the one the
/// proposing tool uses.
pub fn robinhood_atomic_from_decimal(value: &str) -> Result<RobinhoodAtomic, String> {
    value
        .parse::<u128>()
        .map(RobinhoodAtomic::new)
        .map_err(|e| format!("{value:?} is not a decimal integer: {e}"))
}

#[cfg(test)]
mod tests;
