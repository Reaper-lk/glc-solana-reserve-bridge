//! The two endpoints, and nothing else.
//!
//! # The surface, stated as a closed list
//!
//! | method | path | purpose |
//! |---|---|---|
//! | `GET` | `/v2/evm-identity` | `{"address":"0x…"}` — the identity PROVEN at startup |
//! | `POST` | `/v2/sign-evm-auth` | an [`EvmAuthSignRequest`] in, `{"signature_hex":"…"}` out |
//!
//! Everything else is a `404`, including `POST /v1/sign` — the older,
//! byte-oriented endpoint [`crate::signing::remote`] uses for Goldcoin
//! sighashes and Solana claims. This process must never be able to sign
//! bytes it did not derive itself, so the endpoint that takes bytes is
//! absent rather than restricted. There is no debug signing route, no
//! digest route, and no health route that would answer before
//! authentication.
//!
//! # Authentication comes before routing
//!
//! An unauthenticated request gets `401` whatever it asked for, so the
//! path set cannot be enumerated by an anonymous caller and
//! `GET /v2/evm-identity` is not a free identity oracle. Comparison is
//! constant-time over a SHA-256 digest — see
//! [`super::config::BearerToken`].
//!
//! # What the layer does and does not decide
//!
//! It decides nothing about the authorization. The whole decision is
//! [`EvmSignerPolicy::evaluate`], reused verbatim; this module's job is
//! to bound the request, authenticate it, hand the document to that
//! function, and — only on `Ok` — hand `EvmAuthDecision::digest` (never
//! `expected_digest`, never anything from the request) to the signing
//! backend.
//!
//! # Errors are small, bounded, and never echo the request
//!
//! Policy errors are formatted from the request's own fields, and some of
//! those fields are caller-controlled strings. Every `detail` is
//! therefore truncated ([`MAX_ERROR_DETAIL_CHARS`]) before it reaches a
//! response body or a log line: a caller must not be able to choose how
//! much of this process's log volume it consumes, and an error body is
//! for diagnosis, not for reflecting input.
//!
//! # TLS is somebody else's job, on purpose
//!
//! This server speaks plaintext HTTP and is bound to a loopback or
//! private address ([`super::config::validate_bind`]). The bridge client
//! REFUSES a non-`https://` endpoint
//! ([`crate::signing::remote::RemoteSignerConfig`]), so a deployment must
//! put a TLS terminator in front of it. Terminating TLS here instead
//! would mean this process also holding a server certificate's private
//! key, which is one more secret in the one process whose whole design
//! goal is to hold none.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;

use crate::evm::EvmAddress;
use crate::signing::evm_governance::{
    EvmGovernancePolicy, EvmGovernanceSignRequest, EVM_GOVERNANCE_PATH,
};
use crate::signing::evm_policy::{EvmAuthSignRequest, EvmSignerPolicy};

use super::config::BearerToken;
use super::der::kms_der_to_evm_signature;
use super::kms::KmsDigestSigner;

/// Hard cap on a request body.
///
/// An [`EvmAuthSignRequest`] is a fixed set of short scalars: three
/// `0x`-prefixed hex strings, a handful of integers and two short
/// enum spellings. Fully populated with the longest legal value of every
/// field it is comfortably under 1 KiB. 4 KiB is generous headroom while
/// still bounding, by a wide margin, how much memory one caller can make
/// this process buffer — the same reasoning, and the same number, as
/// [`crate::signing::remote`]'s response-side cap.
pub const MAX_REQUEST_BODY_BYTES: usize = 4096;

/// Cap on any `detail` string in an error body or a log line.
pub const MAX_ERROR_DETAIL_CHARS: usize = 300;

/// Unix seconds, injected rather than read from the clock.
///
/// [`EvmSignerPolicy::evaluate`] takes `now` as a parameter "so a domain
/// can use its own trusted time source, and so this is testable" — this
/// trait is where that choice is made for this binary.
pub trait UnixClock: Send + Sync {
    fn now_unix(&self) -> u64;
}

/// The host clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl UnixClock for SystemClock {
    fn now_unix(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            // A clock before the epoch is not a time this policy can
            // reason about. Zero makes every authorization look already
            // expired, which is the fail-closed direction: it refuses to
            // sign rather than signing with an unknown lifetime.
            .unwrap_or(0)
    }
}

/// Everything a request is answered from.
///
/// `address` is the identity PROVEN at startup by
/// [`super::kms::prove_key_controls_address`], not the raw configured
/// value — the two are equal by construction, and naming the proven one
/// here records why serving it is safe.
pub struct SignerService {
    address: EvmAddress,
    policy: EvmSignerPolicy,
    /// The GOVERNANCE policy. Constructed DISABLED and stays disabled
    /// unless [`SignerService::with_governance`] installs one, so a
    /// signer built the way every existing caller builds it will not
    /// sign a governance authorization.
    governance: EvmGovernancePolicy,
    bearer: BearerToken,
    kms: Arc<dyn KmsDigestSigner>,
    clock: Arc<dyn UnixClock>,
}

impl SignerService {
    pub fn new(
        address: EvmAddress,
        policy: EvmSignerPolicy,
        bearer: BearerToken,
        kms: Arc<dyn KmsDigestSigner>,
    ) -> SignerService {
        SignerService::with_clock(address, policy, bearer, kms, Arc::new(SystemClock))
    }

    pub fn with_clock(
        address: EvmAddress,
        policy: EvmSignerPolicy,
        bearer: BearerToken,
        kms: Arc<dyn KmsDigestSigner>,
        clock: Arc<dyn UnixClock>,
    ) -> SignerService {
        SignerService {
            address,
            governance: EvmGovernancePolicy {
                chain_id: policy.chain_id,
                verifying_contract: policy.verifying_contract,
                // Empty: refuse every governance request.
                allowed_actions: Vec::new(),
                max_authorization_ttl_secs: policy.max_authorization_ttl_secs,
                expected_signer_epoch: policy.expected_signer_epoch,
            },
            policy,
            bearer,
            kms,
            clock,
        }
    }

    /// Installs this domain's governance policy.
    ///
    /// Additive and explicit: a caller that never calls this serves
    /// `/v3/sign-evm-governance` with a policy that refuses everything,
    /// which is what `GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS` being
    /// unset means.
    pub fn with_governance(mut self, governance: EvmGovernancePolicy) -> SignerService {
        self.governance = governance;
        self
    }

    pub fn address(&self) -> EvmAddress {
        self.address
    }

    /// Whether this signer will consider a governance request at all.
    /// Logged at startup so an operator can see which posture is live.
    pub fn governance_enabled(&self) -> bool {
        self.governance.is_enabled()
    }
}

impl std::fmt::Debug for SignerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignerService")
            .field("address", &self.address)
            .field("policy", &self.policy)
            .field("bearer", &self.bearer)
            .field("kms_key", &self.kms.key_label())
            .finish()
    }
}

// -------------------------------------------------------------- responses --

#[derive(Debug, Serialize)]
struct IdentityBody {
    address: String,
}

#[derive(Debug, Serialize)]
struct SignatureBody {
    signature_hex: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    detail: String,
}

fn json<T: Serialize>(status: StatusCode, body: &T) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        // Nothing this endpoint returns may be cached or stored by an
        // intermediary: one is an authorization signature.
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .expect("well-formed response")
}

fn error(
    status: StatusCode,
    code: &'static str,
    detail: impl Into<String>,
) -> Response<Full<Bytes>> {
    json(
        status,
        &ErrorBody {
            error: code,
            detail: truncate_detail(detail.into()),
        },
    )
}

/// Bounds a `detail` string. See the module docs.
pub fn truncate_detail(detail: String) -> String {
    let cleaned = detail.replace(['\n', '\r'], " ");
    if cleaned.chars().count() <= MAX_ERROR_DETAIL_CHARS {
        return cleaned;
    }
    let head: String = cleaned.chars().take(MAX_ERROR_DETAIL_CHARS).collect();
    format!("{head}… (truncated)")
}

// ---------------------------------------------------------------- routing --

/// Answers one request.
///
/// Generic over the body type so the tests drive the REAL router,
/// authentication, body cap and error mapping without binding a socket —
/// a hyper `Incoming` body cannot be constructed outside a live
/// connection, and a test that had to stand up a listener to check a
/// `404` would be testing tokio.
pub(crate) async fn handle<B>(req: Request<B>, service: Arc<SignerService>) -> Response<Full<Bytes>>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // ---- authentication, before anything else is even looked at ----
    let authenticated = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| service.bearer.authenticates(v))
        .unwrap_or(false);
    if !authenticated {
        // The path is deliberately absent from this line: an anonymous
        // caller must not be able to use the log as a path oracle either.
        tracing::warn!("rejected an unauthenticated request");
        return error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid Authorization: Bearer credential is required",
        );
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    match (&method, path.as_str()) {
        (&Method::GET, "/v2/evm-identity") => json(
            StatusCode::OK,
            &IdentityBody {
                address: service.address.to_checksum_string(),
            },
        ),
        (&Method::POST, "/v2/sign-evm-auth") => sign_evm_auth(req, service).await,
        (&Method::POST, EVM_GOVERNANCE_PATH) => sign_evm_governance(req, service).await,
        // A known path with the wrong verb is worth distinguishing from
        // an unknown path: it is almost always a client bug, and saying
        // so reveals nothing an authenticated caller does not know.
        (_, "/v2/evm-identity") | (_, "/v2/sign-evm-auth") | (_, EVM_GOVERNANCE_PATH) => error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            format!("{method} is not supported on {path}"),
        ),
        _ => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "this signer serves only GET /v2/evm-identity, POST /v2/sign-evm-auth and POST \
             /v3/sign-evm-governance",
        ),
    }
}

async fn sign_evm_auth<B>(req: Request<B>, service: Arc<SignerService>) -> Response<Full<Bytes>>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // ---- bounded read ----
    let limited = Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES);
    let Ok(collected) = limited.collect().await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            format!("request body is unreadable or larger than {MAX_REQUEST_BODY_BYTES} bytes"),
        );
    };
    let body = collected.to_bytes();

    let document: EvmAuthSignRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            // The body itself is NOT echoed — only serde's positional
            // complaint about it.
            return error(
                StatusCode::BAD_REQUEST,
                "malformed_request",
                format!("body is not a v2 EVM authorization sign request: {e}"),
            );
        }
    };

    // ---- the decision: reused verbatim, never re-implemented ----
    let now = service.clock.now_unix();
    let decision = match service.policy.evaluate(&document, now) {
        Ok(decision) => decision,
        Err(e) => {
            let detail = truncate_detail(e.to_string());
            tracing::warn!(reason = %detail, "policy refused an authorization; KMS was not called");
            return error(StatusCode::FORBIDDEN, "policy_rejected", detail);
        }
    };

    sign_and_respond(service, decision.digest, &decision.summary).await
}

/// `POST /v3/sign-evm-governance`.
///
/// Structurally identical to [`sign_evm_auth`] and deliberately so: a
/// different body type and a different policy, then the SAME one digest
/// that may reach the backend. There is no path here that accepts bytes
/// or a digest to sign, and the digest below is an output of
/// [`EvmGovernancePolicy::evaluate`], recomputed from the request's
/// structured fields by `robinhood::governance` — never the
/// `expected_digest` the caller sent.
async fn sign_evm_governance<B>(
    req: Request<B>,
    service: Arc<SignerService>,
) -> Response<Full<Bytes>>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // ---- bounded read ----
    let limited = Limited::new(req.into_body(), MAX_REQUEST_BODY_BYTES);
    let Ok(collected) = limited.collect().await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            format!("request body is unreadable or larger than {MAX_REQUEST_BODY_BYTES} bytes"),
        );
    };
    let body = collected.to_bytes();

    let document: EvmGovernanceSignRequest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "malformed_request",
                format!("body is not a v3 EVM governance sign request: {e}"),
            );
        }
    };

    // ---- the decision ----
    let now = service.clock.now_unix();
    let decision = match service.governance.evaluate(&document, now) {
        Ok(decision) => decision,
        Err(e) => {
            let detail = truncate_detail(e.to_string());
            tracing::warn!(
                reason = %detail,
                "governance policy refused an authorization; KMS was not called"
            );
            return error(StatusCode::FORBIDDEN, "policy_rejected", detail);
        }
    };

    sign_and_respond(service, decision.digest, &decision.summary).await
}

/// The one place a digest reaches the signing backend, shared by both
/// authorization protocols.
///
/// Shared rather than duplicated because every rule that matters lives
/// here: the digest is signed, the answer is converted and proven to
/// recover to this signer's own identity, a high-`s` answer is normalised
/// and said out loud, and no private key, key material or token appears
/// in any log line or response body.
async fn sign_and_respond(
    service: Arc<SignerService>,
    digest: [u8; 32],
    summary: &str,
) -> Response<Full<Bytes>> {
    let der = match service.kms.sign_digest(&digest).await {
        Ok(der) => der,
        Err(e) => {
            tracing::error!(
                category = e.category(),
                key = %service.kms.key_label(),
                detail = %truncate_detail(e.to_string()),
                "the signing backend did not produce a signature"
            );
            let status = if e.is_retriable() {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::BAD_GATEWAY
            };
            return error(status, "kms_failed", e.category());
        }
    };

    let converted = match kms_der_to_evm_signature(&der, &digest, service.address) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(
                detail = %truncate_detail(e.to_string()),
                "the signing backend's answer could not be converted to a compact EVM signature"
            );
            // A 5xx, not a 4xx: the caller's request was fine and the
            // fault is entirely on this side. The bridge maps 5xx to
            // `Unavailable` and moves on to another domain rather than
            // treating the authorization itself as refused.
            return error(
                StatusCode::BAD_GATEWAY,
                "signature_unusable",
                "the signing backend's signature could not be converted or did not recover to \
                 this signer's identity",
            );
        }
    };
    if converted.normalized_high_s {
        // Worth saying out loud: it means the backend's raw answer would
        // have reverted on-chain verbatim.
        tracing::warn!(
            key = %service.kms.key_label(),
            "the signing backend returned a high-s signature; it was normalised before use"
        );
    }

    tracing::info!(
        summary = %summary,
        signer = %service.address.to_checksum_string(),
        key = %service.kms.key_label(),
        "authorization approved and signed"
    );

    json(
        StatusCode::OK,
        &SignatureBody {
            // Bare lowercase hex, no `0x` — the spelling
            // `RemoteSignerClient::sign_evm_auth` decodes with
            // `goldcoin::hex::decode_vec`.
            signature_hex: crate::goldcoin::hex::encode(&converted.signature.to_bytes()),
        },
    )
}

// ----------------------------------------------------------------- server --

/// Binds `addr` and serves until `shutdown` flips.
pub async fn serve(
    addr: SocketAddr,
    service: Arc<SignerService>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "robinhood KMS authorization signer listening");
    serve_on(listener, service, shutdown).await
}

/// [`serve`] over a listener the caller already bound — the same
/// "tell me the port first" ordering [`crate::admin_api::serve_on`]
/// exists for, and for the same reason.
pub async fn serve_on(
    listener: tokio::net::TcpListener,
    service: Arc<SignerService>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                tracing::info!("signer: shutdown signal received, exiting");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "signer: accept failed");
                        continue;
                    }
                };
                let service = Arc::clone(&service);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let handler = service_fn(move |req| {
                        let service = Arc::clone(&service);
                        async move { Ok::<_, Infallible>(handle(req, service).await) }
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, handler).await {
                        tracing::debug!(%peer, error = %e, "signer connection ended");
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests;
