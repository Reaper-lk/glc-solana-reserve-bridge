//! The `GLC_RHN_SIGNER_*` environment contract, and the
//! [`EvmSignerPolicy`] it builds.
//!
//! # Why environment variables, and why every one of them is required
//!
//! [`crate::signing::evm_policy::EvmSignerPolicy`]'s own docs state the
//! rule this module implements: every field "must be provisioned by that
//! domain's own operators, through that domain's own change process.
//! Nothing in it may be derived from, fetched from, or defaulted to
//! anything the bridge host controls". So there is no config file shared
//! with the bridge, no discovery call to the bridge, and no value read
//! out of a request. The signer host's own systemd unit (or equivalent)
//! supplies them, and this module refuses to start without them.
//!
//! In particular **the verifying contract has no default**. It is not
//! known until the `GlcRobinhoodBridge` deployment exists, and a signer
//! that guessed it — or accepted a permissive placeholder — would be a
//! signer willing to authorize transfers for a deployment nobody
//! approved.
//!
//! # What is deliberately NOT configurable
//!
//! The route/protocol-chain table. This binary serves exactly
//! [`Route::GlcToRhn`] and [`Route::RhnToGlc`], and their protocol chain
//! pairs are this bridge's own namespace constants
//! ([`PROTOCOL_CHAIN_GOLDCOIN`]/[`PROTOCOL_CHAIN_ROBINHOOD`]), not a
//! deployment parameter. `EvmSignerPolicy` holds them as data because the
//! *policy type* is general; this *binary* is not, and an operator who
//! could mistype a protocol chain id could produce signatures for a
//! transfer direction the contract reads differently than they meant.
//! An operator may still narrow what is served
//! ([`ENV_ALLOWED_ROUTES`]/[`ENV_ALLOWED_ACTIONS`]); they cannot widen or
//! redefine it.
//!
//! # Secrets
//!
//! [`ENV_BEARER_TOKEN`] is the only secret this module reads, and
//! [`BearerToken`] never stores it: only its SHA-256 digest survives the
//! constructor. No error, log line or `Debug` rendering in this module
//! can therefore contain it, because after construction the process does
//! not have it. AWS credentials are never read here at all — see
//! [`super::aws`].

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use crate::evm::{EvmAddress, EvmChainId};
use crate::robinhood::auth::{
    ProtocolChainPair, ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE, ACTION_TREASURY_WITHDRAW,
};
use crate::routes::Route;
use crate::signing::evm_policy::EvmSignerPolicy;

// ------------------------------------------------------- variable names --

/// `host:port` to listen on. Required, and validated to be a loopback or
/// private address — see [`validate_bind`].
pub const ENV_BIND: &str = "GLC_RHN_SIGNER_BIND";
/// KMS key id, ARN, or alias (e.g. `alias/glc-robinhood-signer-a`).
/// Deliberately not defaulted to any one of the three production aliases:
/// a binary that knew which alias it was would make "signer A's unit file
/// was copied to host B" invisible.
pub const ENV_KMS_KEY_ID: &str = "GLC_RHN_SIGNER_KMS_KEY_ID";
/// The bearer token the bridge must present. Never logged, never stored
/// in plaintext past [`BearerToken::new`].
pub const ENV_BEARER_TOKEN: &str = "GLC_RHN_SIGNER_BEARER_TOKEN";
/// The EVM address this signer signs as. PROVEN against the KMS key at
/// startup, not trusted — see [`super::kms::prove_key_controls_address`].
pub const ENV_EVM_ADDRESS: &str = "GLC_RHN_SIGNER_EVM_ADDRESS";
/// The EIP-155 chain id the EIP-712 domain binds (4663 in production).
pub const ENV_CHAIN_ID: &str = "GLC_RHN_SIGNER_CHAIN_ID";
/// The deployed `GlcRobinhoodBridge`. No default, ever.
pub const ENV_VERIFYING_CONTRACT: &str = "GLC_RHN_SIGNER_VERIFYING_CONTRACT";
/// The custodied ERC-20.
pub const ENV_TOKEN: &str = "GLC_RHN_SIGNER_TOKEN";
/// This domain's own ceiling on one value-moving authorization, in
/// Robinhood 18-decimal atomic units, as a decimal integer.
pub const ENV_MAX_AMOUNT_ATOMIC: &str = "GLC_RHN_SIGNER_MAX_AMOUNT_ATOMIC";
/// This domain's own ceiling on how long an authorization may live.
pub const ENV_MAX_TTL_SECS: &str = "GLC_RHN_SIGNER_MAX_TTL_SECS";

/// Optional: a comma-separated subset of
/// `payout,refund,settlement,treasury_withdraw`. Defaults to the first
/// THREE — `treasury_withdraw` is never granted by default. Deploying a
/// signer binary that understands the withdrawal protocol must not, on
/// its own, widen what the custody key will sign; a domain opts in
/// through its own change process, and must ALSO set
/// [`ENV_ALLOWED_TREASURIES`] or every withdrawal is still refused.
pub const ENV_ALLOWED_ACTIONS: &str = "GLC_RHN_SIGNER_ALLOWED_ACTIONS";
/// Optional: comma-separated `0x` treasury addresses this domain has
/// independently agreed to pay reserve withdrawals to. Should equal the
/// deployed contract's immutable `TREASURY`, read by this domain's own
/// operators from the chain, never copied from a request. UNSET MEANS
/// NONE, and none means every treasury withdrawal is refused
/// ([`crate::signing::evm_policy::EvmPolicyError::TreasuryNotAllowlisted`]).
pub const ENV_ALLOWED_TREASURIES: &str = "GLC_RHN_SIGNER_ALLOWED_TREASURIES";
/// Which GOVERNANCE actions this credential may authorize, comma
/// separated: `set_limits`, `set_pause`, `set_route_enabled`.
///
/// UNSET MEANS NONE, and none means every governance request is refused
/// ([`crate::signing::evm_governance::EvmGovernanceError::GovernanceDisabled`]).
/// That default is the point: deploying a signer binary that understands
/// the governance protocol must not, on its own, widen what the custody
/// key will sign. A domain opts in through its own change process, or it
/// does not participate in governance at all.
pub const ENV_ALLOWED_GOVERNANCE_ACTIONS: &str = "GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS";
/// Optional: a comma-separated subset of `GlcToRhn,RhnToGlc`. Defaults to
/// both. Naming any other route is an error rather than a no-op — see
/// [`SignerConfigError::RouteNotServed`].
pub const ENV_ALLOWED_ROUTES: &str = "GLC_RHN_SIGNER_ALLOWED_ROUTES";
/// Optional: the contract's `signerEpoch`, when this domain has its own
/// way to know it. ABSENT means `None`, i.e. "do not check" — which is
/// the correct posture for a domain with no independent source, because
/// checking the request's own epoch against itself is not a check. There
/// is deliberately no path by which the request can populate this.
pub const ENV_SIGNER_EPOCH: &str = "GLC_RHN_SIGNER_SIGNER_EPOCH";
/// Optional: pins the AWS region rather than taking it from the standard
/// `AWS_REGION`/profile resolution.
pub const ENV_AWS_REGION: &str = "GLC_RHN_SIGNER_AWS_REGION";

// -------------------------------------------------- protocol namespaces --

/// The contract's protocol-namespace id for the Goldcoin L1 side.
///
/// NOT an EIP-155 chain id: the contract namespaces the bridge's own
/// chains independently of any EVM network, and confusing the two is the
/// mistake [`crate::evm`]'s "Two different things called a chain id"
/// docs describe.
pub const PROTOCOL_CHAIN_GOLDCOIN: u64 = 1001;
/// The contract's protocol-namespace id for the Robinhood side.
pub const PROTOCOL_CHAIN_ROBINHOOD: u64 = 2001;

/// The two routes this binary serves, with the protocol chain pair each
/// one resolves to — held here, independently of any request.
pub fn served_route_chains() -> Vec<(Route, ProtocolChainPair)> {
    vec![
        (
            Route::GlcToRhn,
            ProtocolChainPair {
                source: PROTOCOL_CHAIN_GOLDCOIN,
                dest: PROTOCOL_CHAIN_ROBINHOOD,
            },
        ),
        (
            Route::RhnToGlc,
            ProtocolChainPair {
                source: PROTOCOL_CHAIN_ROBINHOOD,
                dest: PROTOCOL_CHAIN_GOLDCOIN,
            },
        ),
    ]
}

/// Shortest bearer token this signer will accept being provisioned with.
///
/// A signer that authorizes value movement is not a place to discover
/// that someone pasted a 6-character placeholder into a unit file. 32
/// characters is well under any sane generated token (`openssl rand -hex
/// 32` is 64) and well above anything a human would type by hand.
pub const MIN_BEARER_TOKEN_CHARS: usize = 32;

// ------------------------------------------------------------- the token --

/// The bearer credential, reduced to a SHA-256 digest at construction.
///
/// Same construction as [`crate::admin_api::auth`]: comparison is over
/// the full fixed-length digest with no early return, so it reveals
/// neither the token nor where a mismatch occurred. Not a reuse of that
/// type because [`crate::admin_api::auth::OperatorRegistry`] carries
/// admin operator identity and refund-execution capability — concepts a
/// signing domain has no business modelling — and because keeping only
/// the digest is a stronger posture than that type's plaintext `String`.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken {
    digest: [u8; 32],
}

impl BearerToken {
    /// Digests `token` and forgets it. Refuses an empty or short value —
    /// see [`MIN_BEARER_TOKEN_CHARS`].
    pub fn new(token: &str) -> Result<BearerToken, SignerConfigError> {
        if token.chars().count() < MIN_BEARER_TOKEN_CHARS {
            return Err(SignerConfigError::BearerTokenTooShort {
                var: ENV_BEARER_TOKEN,
                minimum: MIN_BEARER_TOKEN_CHARS,
            });
        }
        Ok(BearerToken {
            digest: sha256(token.as_bytes()),
        })
    }

    /// Whether a raw `Authorization` header value authenticates.
    ///
    /// The scheme prefix is matched exactly (`Bearer `, one space, as
    /// [`crate::signing::remote`] sends it). A header that does not carry
    /// that prefix still pays the full digest comparison, so a caller
    /// cannot distinguish "wrong scheme" from "wrong token" by timing.
    pub fn authenticates(&self, authorization_header: &str) -> bool {
        let presented = authorization_header.strip_prefix("Bearer ").unwrap_or("");
        let presented_digest = sha256(presented.as_bytes());
        let mut diff = 0u8;
        for (a, b) in self.digest.iter().zip(presented_digest.iter()) {
            diff |= a ^ b;
        }
        // A header with no `Bearer ` prefix digests the empty string,
        // which cannot equal a token of at least MIN_BEARER_TOKEN_CHARS.
        diff == 0 && !presented.is_empty()
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Not even the digest: it is not a secret, but printing it serves
        // no operator purpose and invites offline guessing.
        f.write_str("BearerToken(<redacted>)")
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

// ------------------------------------------------------------- the config --

/// Everything `glc-robinhood-kms-signer` needs, fully validated.
///
/// `Debug` is derivable because nothing here can print a secret:
/// [`BearerToken`] redacts itself and no AWS credential is a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignerConfig {
    /// Validated loopback/private listen address.
    pub bind: SocketAddr,
    /// KMS key id, ARN or alias. Public, and logged at startup.
    pub kms_key_id: String,
    /// Optional explicit AWS region.
    pub aws_region: Option<String>,
    pub bearer_token: BearerToken,
    /// The address this signer CLAIMS. Proven at startup.
    pub evm_address: EvmAddress,
    /// The independently-held decision policy, reused verbatim from
    /// [`crate::signing::evm_policy`].
    pub policy: EvmSignerPolicy,
    /// The independently-held GOVERNANCE policy. Its `allowed_actions`
    /// is empty unless this domain's operators set
    /// [`ENV_ALLOWED_GOVERNANCE_ACTIONS`], and empty refuses everything.
    pub governance: crate::signing::evm_governance::EvmGovernancePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignerConfigError {
    #[error(
        "{var} is not set. Every value this signer decides with is provisioned by this custody \
         domain's own operators — there are no defaults, because a default would be a value the \
         domain did not choose"
    )]
    Missing { var: &'static str },
    #[error("{var} is set but empty")]
    Empty { var: &'static str },
    #[error("{var} is malformed: {detail}")]
    Malformed { var: &'static str, detail: String },
    #[error(
        "{var} must be at least {minimum} characters — a signer that authorizes value movement \
         will not start on a placeholder credential"
    )]
    BearerTokenTooShort { var: &'static str, minimum: usize },
    #[error(
        "{var} is {addr}, which is neither a loopback nor a private address. This service speaks \
         plaintext HTTP and is designed to sit behind the deployment's own TLS terminator; \
         binding it where the Internet can reach it would expose the signing endpoint directly"
    )]
    BindNotPrivate { var: &'static str, addr: IpAddr },
    #[error(
        "{var} is {addr}, an unspecified (wildcard) address — that binds every interface on the \
         host, including public ones. Name the exact interface to listen on"
    )]
    BindUnspecified { var: &'static str, addr: IpAddr },
    #[error(
        "{var} names route {route:?}; this binary serves only GlcToRhn and RhnToGlc. A route it \
         does not hold protocol chain ids for is one it must not sign for"
    )]
    RouteNotServed { var: &'static str, route: String },
    #[error("{var} names action {action:?}; expected one of payout, refund, settlement")]
    UnknownAction { var: &'static str, action: String },
    #[error("{var} is empty after parsing — an empty allow-list would refuse every request")]
    EmptyAllowList { var: &'static str },
    #[error("{var} must be greater than zero")]
    MustBePositive { var: &'static str },
    #[error(
        "{var} is the zero address, which is never a real deployment parameter and is what an \
         unset or truncated value decodes to"
    )]
    ZeroAddress { var: &'static str },
    #[error(
        "{first} and {second} are the same address ({addr}). The signer identity, the verifying \
         contract and the custodied token are three different things; a configuration where two \
         coincide is a copy/paste error, not a deployment"
    )]
    AddressesCollide {
        first: &'static str,
        second: &'static str,
        addr: String,
    },
}

/// Reads every variable from the real process environment.
pub fn from_env() -> Result<SignerConfig, SignerConfigError> {
    from_lookup(&|var| std::env::var(var).ok())
}

/// [`from_env`] against an arbitrary lookup.
///
/// The indirection exists so the tests can exercise the whole contract —
/// including every rejection — without mutating the process environment,
/// which is global mutable state shared with every other test running in
/// the same binary.
pub fn from_lookup(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<SignerConfig, SignerConfigError> {
    let bind: SocketAddr = parse_required(get, ENV_BIND)?;
    validate_bind(ENV_BIND, bind)?;

    let kms_key_id = required(get, ENV_KMS_KEY_ID)?;
    let aws_region = optional(get, ENV_AWS_REGION)?;
    let bearer_token = BearerToken::new(&required(get, ENV_BEARER_TOKEN)?)?;

    let evm_address = parse_address(get, ENV_EVM_ADDRESS)?;
    let verifying_contract = parse_address(get, ENV_VERIFYING_CONTRACT)?;
    let token = parse_address(get, ENV_TOKEN)?;
    for (first, first_addr, second, second_addr) in [
        (
            ENV_EVM_ADDRESS,
            evm_address,
            ENV_VERIFYING_CONTRACT,
            verifying_contract,
        ),
        (ENV_EVM_ADDRESS, evm_address, ENV_TOKEN, token),
        (ENV_VERIFYING_CONTRACT, verifying_contract, ENV_TOKEN, token),
    ] {
        if first_addr == second_addr {
            return Err(SignerConfigError::AddressesCollide {
                first,
                second,
                addr: first_addr.to_checksum_string(),
            });
        }
    }

    let raw_chain_id: u64 = parse_required(get, ENV_CHAIN_ID)?;
    let chain_id = EvmChainId::new(raw_chain_id).map_err(|e| SignerConfigError::Malformed {
        var: ENV_CHAIN_ID,
        detail: e.to_string(),
    })?;

    let max_amount_robinhood_atomic: u128 = parse_required(get, ENV_MAX_AMOUNT_ATOMIC)?;
    if max_amount_robinhood_atomic == 0 {
        return Err(SignerConfigError::MustBePositive {
            var: ENV_MAX_AMOUNT_ATOMIC,
        });
    }
    let max_authorization_ttl_secs: u64 = parse_required(get, ENV_MAX_TTL_SECS)?;
    if max_authorization_ttl_secs == 0 {
        return Err(SignerConfigError::MustBePositive {
            var: ENV_MAX_TTL_SECS,
        });
    }

    let allowed_actions = parse_allowed_actions(get)?;
    let allowed_routes = parse_allowed_routes(get)?;
    // Only the served routes' chain pairs, narrowed to what this
    // credential actually authorizes — a route absent from
    // `allowed_routes` has no entry to disagree about.
    let route_chains = served_route_chains()
        .into_iter()
        .filter(|(route, _)| allowed_routes.contains(route))
        .collect();

    let expected_signer_epoch = match optional(get, ENV_SIGNER_EPOCH)? {
        Some(raw) => Some(
            raw.parse::<u64>()
                .map_err(|e| SignerConfigError::Malformed {
                    var: ENV_SIGNER_EPOCH,
                    detail: e.to_string(),
                })?,
        ),
        None => None,
    };

    let allowed_governance_actions = parse_allowed_governance_actions(get)?;
    let allowed_treasuries = parse_allowed_treasuries(get)?;

    Ok(SignerConfig {
        bind,
        kms_key_id,
        aws_region,
        bearer_token,
        evm_address,
        governance: crate::signing::evm_governance::EvmGovernancePolicy {
            chain_id,
            verifying_contract,
            allowed_actions: allowed_governance_actions,
            // The same ceilings and epoch source the value-moving policy
            // uses. A domain that bounded one authorization's lifetime
            // has bounded them all; a second knob would be a second
            // thing to get wrong.
            max_authorization_ttl_secs,
            expected_signer_epoch,
        },
        policy: EvmSignerPolicy {
            chain_id,
            verifying_contract,
            token,
            allowed_actions,
            allowed_routes,
            route_chains,
            allowed_treasuries,
            max_amount_robinhood_atomic,
            max_authorization_ttl_secs,
            expected_signer_epoch,
        },
    })
}

/// Refuses a listen address the public Internet could plausibly reach.
///
/// This process serves plaintext HTTP: the deployment architecture puts
/// HTTPS in front of it (the bridge client REQUIRES `https://` — see
/// [`crate::signing::remote::RemoteSignerConfig`]), so the only correct
/// place for this socket is a loopback interface or a private network the
/// terminator shares with it. `0.0.0.0` is refused explicitly rather than
/// falling out of the private-range test, because "I bound the wildcard"
/// is the specific mistake worth naming.
///
/// Accepted: loopback, RFC 1918 private, RFC 6598 shared address space
/// (`100.64.0.0/10`, which several managed VPC and mesh-VPN deployments
/// use), IPv4/IPv6 link-local, and IPv6 unique-local (`fc00::/7`).
pub fn validate_bind(var: &'static str, addr: SocketAddr) -> Result<(), SignerConfigError> {
    let ip = addr.ip();
    let unspecified = match ip {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unspecified(),
    };
    if unspecified {
        return Err(SignerConfigError::BindUnspecified { var, addr: ip });
    }
    let private = match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            // RFC 6598 shared address space, 100.64.0.0/10.
            let shared = octets[0] == 100 && (64..128).contains(&octets[1]);
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || shared
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            // fc00::/7 unique-local, fe80::/10 link-local.
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    };
    if !private {
        return Err(SignerConfigError::BindNotPrivate { var, addr: ip });
    }
    Ok(())
}

fn parse_allowed_actions(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<u8>, SignerConfigError> {
    let Some(raw) = optional(get, ENV_ALLOWED_ACTIONS)? else {
        return Ok(vec![ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE]);
    };
    // A BTreeSet so a repeated entry is not a repeated policy row and the
    // resulting order is deterministic regardless of how it was written.
    let mut actions = BTreeSet::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let action = match name {
            "payout" => ACTION_PAYOUT,
            "refund" => ACTION_REFUND,
            "settlement" => ACTION_SETTLE,
            "treasury_withdraw" => ACTION_TREASURY_WITHDRAW,
            other => {
                return Err(SignerConfigError::UnknownAction {
                    var: ENV_ALLOWED_ACTIONS,
                    action: other.to_string(),
                })
            }
        };
        actions.insert(action);
    }
    if actions.is_empty() {
        return Err(SignerConfigError::EmptyAllowList {
            var: ENV_ALLOWED_ACTIONS,
        });
    }
    Ok(actions.into_iter().collect())
}

/// The treasury allow-list. Absent -> EMPTY -> every withdrawal refused.
///
/// An explicitly EMPTY setting is an error for the same reason the
/// governance list's is: an operator who wrote the variable meant to
/// grant something.
fn parse_allowed_treasuries(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<EvmAddress>, SignerConfigError> {
    let Some(raw) = optional(get, ENV_ALLOWED_TREASURIES)? else {
        return Ok(Vec::new());
    };
    let mut treasuries: Vec<EvmAddress> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let address: EvmAddress = entry.parse().map_err(|e| SignerConfigError::Malformed {
            var: ENV_ALLOWED_TREASURIES,
            detail: format!("{entry:?}: {e}"),
        })?;
        if address == EvmAddress::ZERO {
            return Err(SignerConfigError::Malformed {
                var: ENV_ALLOWED_TREASURIES,
                detail: "the zero address is not a treasury".to_string(),
            });
        }
        if !treasuries.contains(&address) {
            treasuries.push(address);
        }
    }
    if treasuries.is_empty() {
        return Err(SignerConfigError::EmptyAllowList {
            var: ENV_ALLOWED_TREASURIES,
        });
    }
    Ok(treasuries)
}

/// The governance allow-list. Absent -> EMPTY -> governance disabled.
///
/// Deliberately unlike [`parse_allowed_actions`], which defaults to the
/// three value-moving actions: that default preserves behaviour that
/// already existed, while a non-empty default here would CREATE an
/// authority the domain never granted.
fn parse_allowed_governance_actions(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<u8>, SignerConfigError> {
    let Some(raw) = optional(get, ENV_ALLOWED_GOVERNANCE_ACTIONS)? else {
        return Ok(Vec::new());
    };
    let mut actions = BTreeSet::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let action =
            crate::signing::evm_governance::governance_action_from_name(name).ok_or_else(|| {
                SignerConfigError::UnknownAction {
                    var: ENV_ALLOWED_GOVERNANCE_ACTIONS,
                    action: name.to_string(),
                }
            })?;
        actions.insert(action);
    }
    // An explicitly EMPTY setting is a mistake worth naming: an operator
    // who wrote the variable meant to grant something. Leaving it unset
    // is how a domain declines governance.
    if actions.is_empty() {
        return Err(SignerConfigError::EmptyAllowList {
            var: ENV_ALLOWED_GOVERNANCE_ACTIONS,
        });
    }
    Ok(actions.into_iter().collect())
}

fn parse_allowed_routes(
    get: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Route>, SignerConfigError> {
    let Some(raw) = optional(get, ENV_ALLOWED_ROUTES)? else {
        return Ok(vec![Route::GlcToRhn, Route::RhnToGlc]);
    };
    let mut routes: Vec<Route> = Vec::new();
    for name in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        // Parsed through `Route`'s own FromStr, then narrowed: a name
        // this bridge models but this binary does not serve (GlcToSol,
        // SolToRhn, ...) is an explicit error, never silently dropped.
        let route: Route = name
            .parse()
            .map_err(|_| SignerConfigError::RouteNotServed {
                var: ENV_ALLOWED_ROUTES,
                route: name.to_string(),
            })?;
        if !matches!(route, Route::GlcToRhn | Route::RhnToGlc) {
            return Err(SignerConfigError::RouteNotServed {
                var: ENV_ALLOWED_ROUTES,
                route: name.to_string(),
            });
        }
        if !routes.contains(&route) {
            routes.push(route);
        }
    }
    if routes.is_empty() {
        return Err(SignerConfigError::EmptyAllowList {
            var: ENV_ALLOWED_ROUTES,
        });
    }
    Ok(routes)
}

fn required(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
) -> Result<String, SignerConfigError> {
    let value = get(var).ok_or(SignerConfigError::Missing { var })?;
    if value.trim().is_empty() {
        return Err(SignerConfigError::Empty { var });
    }
    Ok(value.trim().to_string())
}

fn optional(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
) -> Result<Option<String>, SignerConfigError> {
    match get(var) {
        None => Ok(None),
        // An explicitly-set-but-empty optional is a mistake, not "unset":
        // silently treating it as absent is how a deployment ends up with
        // a policy the operator did not intend.
        Some(value) if value.trim().is_empty() => Err(SignerConfigError::Empty { var }),
        Some(value) => Ok(Some(value.trim().to_string())),
    }
}

fn parse_required<T>(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
) -> Result<T, SignerConfigError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    required(get, var)?
        .parse()
        .map_err(|e: T::Err| SignerConfigError::Malformed {
            var,
            detail: e.to_string(),
        })
}

fn parse_address(
    get: &dyn Fn(&str) -> Option<String>,
    var: &'static str,
) -> Result<EvmAddress, SignerConfigError> {
    let address: EvmAddress = parse_required(get, var)?;
    if address.is_zero() {
        return Err(SignerConfigError::ZeroAddress { var });
    }
    Ok(address)
}

#[cfg(test)]
mod tests;
