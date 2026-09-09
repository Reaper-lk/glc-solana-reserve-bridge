//! The endpoints, end to end through the real router.
//!
//! These drive [`handle`] directly rather than binding a socket: the
//! router, the bearer check, the body cap, the policy call and the error
//! mapping are all in that function, and a test that had to stand up a
//! listener to assert a `404` would be testing tokio instead. The signing
//! backend is [`super::super::kms::fake::FakeKms`], which signs with a
//! real secp256k1 key — so a `200` here really does carry a signature
//! that recovers, and "KMS was called" is a fact about a counter rather
//! than about a mock's expectations.

use std::sync::Arc;

use http_body_util::BodyExt;
use hyper::Request;

use super::super::config::{served_route_chains, BearerToken};
use super::super::kms::fake::{FakeBehaviour, FakeKms};
use super::super::kms::KmsError;
use super::*;
use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::secp;
use crate::evm::{EvmChainId, EvmSignature};
use crate::robinhood::auth::{
    BridgeDomain, EvmAuthRequest, PayoutAuth, ProtocolChainPair, RefundAuth, SettlementAuth,
    ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE,
};
use crate::routes::Route;
use crate::signing::evm_policy::EvmAuthSignRequest;

const CHAIN_ID: u64 = 4663;
const NOW: u64 = 1_800_000_000;
const TTL: u64 = 900;
const MAX_TTL: u64 = 3_600;
const BEARER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";
const ONE_GLC: u128 = 1_000_000_000_000_000_000;

fn bridge() -> EvmAddress {
    EvmAddress::from_bytes([0xb1; 20])
}

fn token() -> EvmAddress {
    EvmAddress::from_bytes([0x70; 20])
}

fn domain() -> BridgeDomain {
    BridgeDomain::new(EvmChainId::new(CHAIN_ID).unwrap(), bridge())
}

fn glc_to_rhn() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 1001,
        dest: 2001,
    }
}

fn rhn_to_glc() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 2001,
        dest: 1001,
    }
}

fn policy() -> EvmSignerPolicy {
    EvmSignerPolicy {
        chain_id: EvmChainId::new(CHAIN_ID).unwrap(),
        verifying_contract: bridge(),
        token: token(),
        allowed_actions: vec![ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE],
        allowed_routes: vec![Route::GlcToRhn, Route::RhnToGlc],
        route_chains: served_route_chains(),
        max_amount_robinhood_atomic: 10_000 * ONE_GLC,
        max_authorization_ttl_secs: MAX_TTL,
        expected_signer_epoch: None,
    }
}

/// A clock the tests own, so `evaluate`'s lifetime checks are exercised
/// deterministically rather than against the wall clock.
struct FixedClock(u64);

impl UnixClock for FixedClock {
    fn now_unix(&self) -> u64 {
        self.0
    }
}

struct Harness {
    service: Arc<SignerService>,
    kms: Arc<FakeKms>,
}

fn harness_with(policy: EvmSignerPolicy) -> Harness {
    let kms = Arc::new(FakeKms::new(0x11));
    let service = Arc::new(SignerService::with_clock(
        kms.address(),
        policy,
        BearerToken::new(BEARER).unwrap(),
        Arc::clone(&kms) as Arc<dyn super::super::kms::KmsDigestSigner>,
        Arc::new(FixedClock(NOW)),
    ));
    Harness { service, kms }
}

fn harness() -> Harness {
    harness_with(policy())
}

// ------------------------------------------------------------- requests --

fn get(path: &str, bearer: Option<&str>) -> Request<Full<Bytes>> {
    build(Method::GET, path, bearer, Bytes::new())
}

fn post_json(
    path: &str,
    bearer: Option<&str>,
    body: &impl serde::Serialize,
) -> Request<Full<Bytes>> {
    build(
        Method::POST,
        path,
        bearer,
        Bytes::from(serde_json::to_vec(body).unwrap()),
    )
}

fn build(method: Method, path: &str, bearer: Option<&str>, body: Bytes) -> Request<Full<Bytes>> {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    builder.body(Full::new(body)).unwrap()
}

async fn call(harness: &Harness, request: Request<Full<Bytes>>) -> (StatusCode, serde_json::Value) {
    let response = handle(request, Arc::clone(&harness.service)).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| panic!("every response is JSON; got {bytes:?}: {e}"));
    (status, value)
}

// --------------------------------------------------------- authorizations --

fn payout() -> EvmAuthRequest {
    EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(5 * ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    )
}

fn refund() -> EvmAuthRequest {
    EvmAuthRequest::refund(
        domain(),
        RefundAuth {
            route: Route::RhnToGlc,
            chains: rhn_to_glc(),
            token: token(),
            request_id: [0x22; 32],
            obligation_index: 42,
            recipient: EvmAddress::from_bytes([0xd0; 20]),
            amount: RobinhoodAtomic::new(3 * ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    )
}

fn settlement() -> EvmAuthRequest {
    EvmAuthRequest::settlement(
        domain(),
        SettlementAuth {
            route: Route::RhnToGlc,
            chains: rhn_to_glc(),
            request_id: [0x33; 32],
            obligation_index: 42,
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    )
}

fn document(request: &EvmAuthRequest) -> EvmAuthSignRequest {
    EvmAuthSignRequest::from_request(request).expect("a well-formed authorization encodes")
}

fn signature_of(value: &serde_json::Value) -> EvmSignature {
    let hex = value["signature_hex"]
        .as_str()
        .unwrap_or_else(|| panic!("expected signature_hex in {value}"));
    // Exactly 130 lowercase hex digits, no `0x` — the spelling
    // `RemoteSignerClient::sign_evm_auth` decodes.
    assert_eq!(hex.len(), 130, "signature_hex must be 130 hex chars");
    assert!(
        hex.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "signature_hex must be lowercase hex with no 0x prefix: {hex}"
    );
    EvmSignature::try_from_slice(&crate::goldcoin::hex::decode_vec(hex).unwrap()).unwrap()
}

// ============================================================== identity ==

#[tokio::test]
async fn the_identity_endpoint_returns_the_proven_checksummed_address() {
    let harness = harness();
    let (status, body) = call(&harness, get("/v2/evm-identity", Some(BEARER))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["address"].as_str().unwrap(),
        harness.kms.address().to_checksum_string()
    );
    // The bridge client parses this with `EvmAddress::from_str`, which
    // validates EIP-55 whenever the string carries case information.
    let parsed: EvmAddress = body["address"].as_str().unwrap().parse().unwrap();
    assert_eq!(parsed, harness.kms.address());
    // Serving an identity must not consume the signing key.
    assert_eq!(harness.kms.calls(), 0);
}

// ======================================================== authentication ==

#[tokio::test]
async fn an_unauthenticated_request_is_refused_on_every_endpoint() {
    let harness = harness();
    let doc = document(&payout());
    for request in [
        get("/v2/evm-identity", None),
        post_json("/v2/sign-evm-auth", None, &doc),
    ] {
        let (status, body) = call(&harness, request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "unauthorized");
    }
    assert_eq!(harness.kms.calls(), 0);
}

#[tokio::test]
async fn a_wrong_bearer_token_is_refused() {
    let harness = harness();
    let doc = document(&payout());
    for token in [
        "f".repeat(48),
        BEARER[..20].to_string(),
        format!("{BEARER}x"),
        String::new(),
    ] {
        let (status, body) =
            call(&harness, post_json("/v2/sign-evm-auth", Some(&token), &doc)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "token {token:?}");
        assert_eq!(body["error"], "unauthorized");
    }
    assert_eq!(harness.kms.calls(), 0, "a bad credential never reaches KMS");
}

/// Authentication happens BEFORE routing, so an anonymous caller cannot
/// use the response code to learn which paths exist.
#[tokio::test]
async fn an_unauthenticated_caller_cannot_enumerate_paths() {
    let harness = harness();
    for path in ["/v2/evm-identity", "/v1/sign", "/nope", "/"] {
        let (status, _) = call(&harness, get(path, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path}");
    }
}

// ============================================================== signing ==

#[tokio::test]
async fn every_payload_family_is_signed_and_the_signature_recovers() {
    for request in [payout(), refund(), settlement()] {
        let harness = harness();
        let (status, body) = call(
            &harness,
            post_json("/v2/sign-evm-auth", Some(BEARER), &document(&request)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", request.kind_str());

        let signature = signature_of(&body);
        let digest = request.digest().unwrap();
        assert_eq!(
            secp::recover_address(&digest, &signature).unwrap(),
            harness.kms.address(),
            "{}",
            request.kind_str()
        );
    }
}

/// The one digest that may reach the backend is the one the POLICY
/// computed — and it reaches it exactly once per request.
#[tokio::test]
async fn an_approved_request_reaches_the_backend_exactly_once_with_the_policys_digest() {
    let harness = harness();
    let request = payout();
    let (status, _) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&request)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(harness.kms.calls(), 1);
    assert_eq!(
        harness.kms.signed_digests(),
        vec![request.digest().unwrap()],
        "the backend must see the recomputed digest and nothing else"
    );
}

/// A backend that does not normalise `s` is not a backend that produces
/// unusable signatures: the conversion repairs it, and what leaves is
/// low-`s` and recovers.
#[tokio::test]
async fn a_high_s_backend_answer_is_normalised_before_it_is_served() {
    let harness = harness();
    harness.kms.set_behaviour(FakeBehaviour::SignHighS);
    let request = payout();
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&request)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let signature = signature_of(&body);
    assert!(!secp::is_high_s(signature.s()));
    assert_eq!(
        secp::recover_address(&request.digest().unwrap(), &signature).unwrap(),
        harness.kms.address()
    );
}

// ================================================== policy rejections ====

/// Every one of these is a genuinely-encodable authorization whose
/// digest agrees with its own fields — so the refusal is the POLICY
/// refusing, not a malformed document being caught.
#[tokio::test]
async fn a_request_for_another_chain_is_refused_without_calling_the_backend() {
    let other_domain = BridgeDomain::new(EvmChainId::new(1).unwrap(), bridge());
    let request = EvmAuthRequest::payout(
        other_domain,
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    assert_rejected(&document(&request), "chain id").await;
}

#[tokio::test]
async fn a_request_for_another_verifying_contract_is_refused() {
    let other_domain = BridgeDomain::new(
        EvmChainId::new(CHAIN_ID).unwrap(),
        EvmAddress::from_bytes([0xbe; 20]),
    );
    let request = EvmAuthRequest::payout(
        other_domain,
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    assert_rejected(&document(&request), "verifying contract").await;
}

#[tokio::test]
async fn a_request_for_another_token_is_refused() {
    let request = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: EvmAddress::from_bytes([0x71; 20]),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    assert_rejected(&document(&request), "token").await;
}

/// A credential narrowed to one route refuses the other, even though the
/// authorization is otherwise perfectly valid for this deployment.
#[tokio::test]
async fn a_route_this_credential_does_not_authorize_is_refused() {
    let mut narrowed = policy();
    narrowed.allowed_routes = vec![Route::GlcToRhn];
    narrowed.route_chains.retain(|(r, _)| *r == Route::GlcToRhn);
    let harness = harness_with(narrowed);

    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&refund())),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "policy_rejected");
    assert_eq!(harness.kms.calls(), 0);
}

#[tokio::test]
async fn an_action_this_credential_does_not_authorize_is_refused() {
    let mut narrowed = policy();
    narrowed.allowed_actions = vec![ACTION_SETTLE];
    let harness = harness_with(narrowed);
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&payout())),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "policy_rejected");
    assert_eq!(harness.kms.calls(), 0);
}

/// The bridge host's own protocol chain ids are not authoritative: a
/// request that swaps the pair for a route describes a different transfer
/// direction than this domain provisioned.
#[tokio::test]
async fn a_request_whose_protocol_chain_pair_disagrees_is_refused() {
    let request = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            // The RhnToGlc pair on a GlcToRhn route.
            chains: rhn_to_glc(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    assert_rejected(&document(&request), "protocol chain pair").await;
}

#[tokio::test]
async fn an_amount_above_this_domains_own_ceiling_is_refused() {
    let request = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            // The policy ceiling is 10_000 GLC.
            amount: RobinhoodAtomic::new(10_001 * ONE_GLC),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    assert_rejected(&document(&request), "amount ceiling").await;
}

#[tokio::test]
async fn a_lifetime_above_this_domains_own_ceiling_is_refused() {
    let request = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(ONE_GLC),
            signer_epoch: 7,
            // The policy ceiling is MAX_TTL.
            expiry: NOW + MAX_TTL + 1,
        },
    );
    assert_rejected(&document(&request), "ttl ceiling").await;
}

#[tokio::test]
async fn an_already_expired_authorization_is_refused() {
    let request = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn(),
            token: token(),
            request_id: [0x11; 32],
            recipient: EvmAddress::from_bytes([0xc0; 20]),
            amount: RobinhoodAtomic::new(ONE_GLC),
            signer_epoch: 7,
            expiry: NOW - 1,
        },
    );
    assert_rejected(&document(&request), "already expired").await;
}

#[tokio::test]
async fn a_stale_signer_epoch_is_refused_when_this_domain_tracks_one() {
    let mut tracked = policy();
    tracked.expected_signer_epoch = Some(9);
    let harness = harness_with(tracked);
    // payout() carries signer_epoch 7.
    let (status, _) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&payout())),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(harness.kms.calls(), 0);
}

/// Rule zero. The requester's own digest is a cross-check, never the
/// value signed — so a document whose fields and digest disagree is
/// refused outright, and NOTHING is signed. If `expected_digest` were
/// ever the thing signed, this test would pass a `200` back with a
/// signature over a caller-chosen 32 bytes.
#[tokio::test]
async fn a_document_whose_expected_digest_disagrees_is_refused() {
    let harness = harness();
    let mut doc = document(&payout());
    doc.expected_digest =
        "0x00000000000000000000000000000000000000000000000000000000deadbeef".to_string();
    let (status, body) = call(&harness, post_json("/v2/sign-evm-auth", Some(BEARER), &doc)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "policy_rejected");
    assert_eq!(
        harness.kms.calls(),
        0,
        "a disputed digest must never reach the signing backend"
    );
}

#[tokio::test]
async fn an_unknown_protocol_version_is_refused() {
    let mut doc = document(&payout());
    doc.protocol_version = 99;
    assert_rejected(&doc, "protocol version").await;
}

/// Abandonment is not a payload this signer can represent. Its action
/// byte is refused as unknown, before any reconstruction is attempted.
#[tokio::test]
async fn an_unknown_action_byte_is_refused() {
    let mut doc = document(&payout());
    doc.action = 0x04;
    doc.kind = "abandonment".to_string();
    assert_rejected(&doc, "unknown action").await;
}

async fn assert_rejected(document: &EvmAuthSignRequest, label: &str) {
    let harness = harness();
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), document),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {body}");
    assert_eq!(body["error"], "policy_rejected", "{label}");
    assert!(
        body["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "{label}: a refusal must say why"
    );
    assert_eq!(
        harness.kms.calls(),
        0,
        "{label}: a refused request must never reach the signing backend"
    );
}

// ================================================= the shape of the surface ==

/// The endpoint that takes bytes does not exist here. Neither does any
/// digest, debug, or health route.
#[tokio::test]
async fn there_is_no_endpoint_that_signs_arbitrary_bytes_or_a_bare_digest() {
    let harness = harness();
    let raw = serde_json::json!({
        "payload_hex": "aa".repeat(32),
        "digest": "0x".to_string() + &"11".repeat(32),
        "message": "0x".to_string() + &"22".repeat(32),
    });
    for path in [
        "/v1/sign",
        "/v1/identity",
        "/v2/sign",
        "/v2/sign-digest",
        "/v2/sign-raw",
        "/v2/sign-evm-auth/raw",
        "/sign",
        "/debug/sign",
        "/health",
        "/metrics",
        "/",
    ] {
        let (status, body) = call(&harness, post_json(path, Some(BEARER), &raw)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} answered {body}");
        assert_eq!(body["error"], "not_found", "{path}");
    }
    assert_eq!(
        harness.kms.calls(),
        0,
        "no path outside the two endpoints may reach the signing backend"
    );
}

#[tokio::test]
async fn a_known_path_with_the_wrong_method_is_refused() {
    let harness = harness();
    let doc = document(&payout());
    let cases = [
        (Method::POST, "/v2/evm-identity"),
        (Method::DELETE, "/v2/evm-identity"),
        (Method::GET, "/v2/sign-evm-auth"),
        (Method::PUT, "/v2/sign-evm-auth"),
    ];
    for (method, path) in cases {
        let request = build(
            method.clone(),
            path,
            Some(BEARER),
            Bytes::from(serde_json::to_vec(&doc).unwrap()),
        );
        let (status, body) = call(&harness, request).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{method} {path}");
        assert_eq!(body["error"], "method_not_allowed");
    }
    assert_eq!(harness.kms.calls(), 0);
}

// ================================================================ bodies ==

#[tokio::test]
async fn an_oversized_body_is_refused_before_it_is_parsed() {
    let harness = harness();
    let request = build(
        Method::POST,
        "/v2/sign-evm-auth",
        Some(BEARER),
        Bytes::from(vec![b'x'; MAX_REQUEST_BODY_BYTES + 1]),
    );
    let (status, body) = call(&harness, request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"], "request_too_large");
    assert_eq!(harness.kms.calls(), 0);
}

/// A padded-but-legal document still works: the cap bounds memory, it is
/// not a second parser.
#[tokio::test]
async fn a_body_at_the_cap_is_still_served() {
    let harness = harness();
    let mut bytes = serde_json::to_vec(&document(&payout())).unwrap();
    assert!(bytes.len() < MAX_REQUEST_BODY_BYTES, "a document is small");
    // Pad with insignificant whitespace up to exactly the cap.
    bytes.resize(MAX_REQUEST_BODY_BYTES, b' ');
    let request = build(
        Method::POST,
        "/v2/sign-evm-auth",
        Some(BEARER),
        Bytes::from(bytes),
    );
    let (status, _) = call(&harness, request).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_malformed_body_is_a_client_error_and_never_reaches_the_backend() {
    let harness = harness();
    for body in [
        Bytes::from_static(b""),
        Bytes::from_static(b"not json"),
        Bytes::from_static(b"{}"),
        Bytes::from_static(br#"{"protocol_version":2}"#),
        // The v1 request shape, offered to the v2 endpoint.
        Bytes::from_static(br#"{"payload_hex":"aabb"}"#),
    ] {
        let request = build(
            Method::POST,
            "/v2/sign-evm-auth",
            Some(BEARER),
            body.clone(),
        );
        let (status, value) = call(&harness, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
        assert_eq!(value["error"], "malformed_request");
    }
    assert_eq!(harness.kms.calls(), 0);
}

/// An error body is small and bounded even when the caller supplies a
/// long field the policy error would otherwise interpolate.
#[tokio::test]
async fn error_details_are_bounded_regardless_of_what_the_caller_sent() {
    let harness = harness();
    let mut doc = document(&payout());
    doc.amount_robinhood_atomic = Some("9".repeat(3_000));
    let (status, body) = call(&harness, post_json("/v2/sign-evm-auth", Some(BEARER), &doc)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.chars().count() <= MAX_ERROR_DETAIL_CHARS + 16,
        "detail was {} chars",
        detail.chars().count()
    );
}

#[test]
fn truncate_detail_bounds_and_flattens() {
    assert_eq!(truncate_detail("short".to_string()), "short");
    assert_eq!(truncate_detail("a\nb\r\nc".to_string()), "a b  c");
    let long = truncate_detail("x".repeat(5_000));
    assert!(long.chars().count() <= MAX_ERROR_DETAIL_CHARS + 16);
    assert!(long.ends_with("(truncated)"));
}

// ================================================== backend failure modes ==

#[tokio::test]
async fn an_unavailable_backend_is_a_retriable_5xx_carrying_only_a_category() {
    let harness = harness();
    harness
        .kms
        .set_behaviour(FakeBehaviour::Fail(KmsError::Unavailable(
            "connection reset".to_string(),
        )));
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&payout())),
    )
    .await;
    // The bridge client maps 5xx to `SignerError::Unavailable` and comes
    // back next tick, which is the right outcome for a transport fault.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "kms_failed");
    assert_eq!(body["detail"], "unavailable");
    assert!(body.get("signature_hex").is_none());
}

#[tokio::test]
async fn a_refusing_backend_is_a_non_retriable_5xx() {
    let harness = harness();
    harness
        .kms
        .set_behaviour(FakeBehaviour::Fail(KmsError::Refused(
            "AccessDeniedException".to_string(),
        )));
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&payout())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["detail"], "refused");
}

/// A signature that does not recover to this signer's own identity is
/// never served, even though the request itself was approved.
#[tokio::test]
async fn a_signature_from_the_wrong_key_is_never_served() {
    let harness = harness();
    harness.kms.set_behaviour(FakeBehaviour::SignWithWrongKey);
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&payout())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "signature_unusable");
    assert!(body.get("signature_hex").is_none());
}

#[tokio::test]
async fn a_malformed_backend_answer_is_never_served() {
    let harness = harness();
    harness.kms.set_behaviour(FakeBehaviour::ReturnGarbage);
    let (status, body) = call(
        &harness,
        post_json("/v2/sign-evm-auth", Some(BEARER), &document(&payout())),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["error"], "signature_unusable");
}

// ==================================================================== misc ==

#[tokio::test]
async fn no_response_ever_contains_the_bearer_token() {
    let harness = harness();
    let doc = document(&payout());
    let requests = vec![
        get("/v2/evm-identity", Some(BEARER)),
        post_json("/v2/sign-evm-auth", Some(BEARER), &doc),
        post_json("/v2/sign-evm-auth", Some("wrong-token-entirely"), &doc),
        get("/nope", Some(BEARER)),
    ];
    for request in requests {
        let (_, body) = call(&harness, request).await;
        assert!(
            !body.to_string().contains(BEARER),
            "a response body echoed the credential: {body}"
        );
    }
}

#[test]
fn the_service_debug_rendering_carries_no_secret() {
    let harness = harness();
    let rendered = format!("{:?}", harness.service);
    assert!(!rendered.contains(BEARER));
    assert!(rendered.contains("BearerToken(<redacted>)"));
}

// =====================================================================
// `POST /v3/sign-evm-governance`
// =====================================================================
//
// The endpoint exists on every upgraded signer and refuses everything
// until that signer's own operators opt in. These drive the same router
// the value-moving tests drive, against the same real-key fake KMS, so a
// `200` here carries a signature that genuinely recovers to the signer's
// address over the digest the POLICY derived.

use crate::evm::EvmU256;
use crate::robinhood::calls::BridgeLimits;
use crate::robinhood::governance::{
    GovernanceAuth, GovernancePayload, ACTION_SET_LIMITS, ACTION_SET_PAUSE,
    ACTION_SET_ROUTE_ENABLED, CANONICAL_SCALE,
};
use crate::signing::evm_governance::{
    EvmGovernancePolicy, EvmGovernanceSignRequest, EVM_GOVERNANCE_PATH,
};

fn governance_policy(allowed: Vec<u8>) -> EvmGovernancePolicy {
    EvmGovernancePolicy {
        chain_id: EvmChainId::new(CHAIN_ID).unwrap(),
        verifying_contract: bridge(),
        allowed_actions: allowed,
        max_authorization_ttl_secs: MAX_TTL,
        expected_signer_epoch: None,
    }
}

/// A harness whose signer has opted in to `allowed`.
fn governance_harness(allowed: Vec<u8>) -> Harness {
    let kms = Arc::new(FakeKms::new(0x11));
    let service = Arc::new(
        SignerService::with_clock(
            kms.address(),
            policy(),
            BearerToken::new(BEARER).unwrap(),
            Arc::clone(&kms) as Arc<dyn super::super::kms::KmsDigestSigner>,
            Arc::new(FixedClock(NOW)),
        )
        .with_governance(governance_policy(allowed)),
    );
    Harness { service, kms }
}

fn governance_limits() -> BridgeLimits {
    BridgeLimits {
        inbound_min: EvmU256::from_u128(CANONICAL_SCALE),
        inbound_max: EvmU256::from_u128(20 * CANONICAL_SCALE),
        inbound_rolling_limit: EvmU256::from_u128(40 * CANONICAL_SCALE),
        outbound_min: EvmU256::from_u128(CANONICAL_SCALE),
        outbound_max: EvmU256::from_u128(20 * CANONICAL_SCALE),
        outbound_rolling_limit: EvmU256::from_u128(40 * CANONICAL_SCALE),
        protected_min_reserve: EvmU256::from_u128(5 * CANONICAL_SCALE),
    }
}

fn governance_document(payload: GovernancePayload) -> EvmGovernanceSignRequest {
    EvmGovernanceSignRequest::from_auth(
        &GovernanceAuth {
            payload,
            signer_epoch: 7,
            nonce: EvmU256::from_u64(3),
            expiry: NOW + TTL,
        },
        domain(),
    )
    .expect("a buildable governance document")
}

/// The default posture. A signer built the way every existing caller
/// builds one signs no governance action, and says why.
#[tokio::test]
async fn governance_is_disabled_on_a_signer_that_did_not_opt_in() {
    let harness = harness();
    assert!(!harness.service.governance_enabled());

    let (status, body) = call(
        &harness,
        post_json(
            EVM_GOVERNANCE_PATH,
            Some(BEARER),
            &governance_document(GovernancePayload::SetPaused {
                deposits_paused: true,
                payouts_paused: true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "policy_rejected");
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS"),
        "the refusal must name the variable that grants it: {body}"
    );
    assert_eq!(harness.kms.calls(), 0, "KMS must not be called");
}

/// Opted in, every action signs — and the signature recovers to this
/// signer's address over the digest the policy derived, not one the
/// caller supplied.
#[tokio::test]
async fn an_opted_in_signer_signs_every_permitted_governance_action() {
    let harness = governance_harness(vec![
        ACTION_SET_LIMITS,
        ACTION_SET_PAUSE,
        ACTION_SET_ROUTE_ENABLED,
    ]);
    assert!(harness.service.governance_enabled());

    for payload in [
        GovernancePayload::SetLimits(governance_limits()),
        GovernancePayload::SetPaused {
            deposits_paused: false,
            payouts_paused: false,
        },
        GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        },
    ] {
        let auth = GovernanceAuth {
            payload: payload.clone(),
            signer_epoch: 7,
            nonce: EvmU256::from_u64(3),
            expiry: NOW + TTL,
        };
        let document = governance_document(payload);
        let (status, body) = call(
            &harness,
            post_json(EVM_GOVERNANCE_PATH, Some(BEARER), &document),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let raw =
            crate::goldcoin::hex::decode_vec(body["signature_hex"].as_str().unwrap()).expect("hex");
        let signature = EvmSignature::try_from_slice(&raw).expect("a compact signature");
        let digest = auth.digest(domain()).unwrap();
        let recovered = secp::recover_address(&digest, &signature).expect("recovers");
        assert_eq!(
            recovered,
            harness.service.address(),
            "the signature must be over the digest the POLICY derived"
        );
    }
    assert_eq!(harness.kms.calls(), 3);
}

/// Scope is per action: a domain may grant the pause and withhold the
/// limits.
#[tokio::test]
async fn an_action_outside_the_signers_scope_is_refused_without_calling_kms() {
    let harness = governance_harness(vec![ACTION_SET_PAUSE]);

    let (status, _) = call(
        &harness,
        post_json(
            EVM_GOVERNANCE_PATH,
            Some(BEARER),
            &governance_document(GovernancePayload::SetPaused {
                deposits_paused: true,
                payouts_paused: true,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = call(
        &harness,
        post_json(
            EVM_GOVERNANCE_PATH,
            Some(BEARER),
            &governance_document(GovernancePayload::SetLimits(governance_limits())),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        harness.kms.calls(),
        1,
        "only the permitted action reached KMS"
    );
}

/// The digest is derived, never taken: a tampered field is refused before
/// the backend is touched.
#[tokio::test]
async fn a_tampered_governance_request_never_reaches_the_signing_backend() {
    let harness = governance_harness(vec![ACTION_SET_LIMITS]);
    let mut document = governance_document(GovernancePayload::SetLimits(governance_limits()));
    document.limits.as_mut().unwrap().outbound_max = (30 * CANONICAL_SCALE).to_string();

    let (status, body) = call(
        &harness,
        post_json(EVM_GOVERNANCE_PATH, Some(BEARER), &document),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(harness.kms.calls(), 0);
}

/// The two structurally non-executable routes are refused by the signer,
/// whatever it has been configured to allow.
#[tokio::test]
async fn a_solana_facing_route_is_refused_by_the_endpoint() {
    let harness = governance_harness(vec![ACTION_SET_ROUTE_ENABLED]);
    for route in ["SolToRhn", "RhnToSol"] {
        let mut document = governance_document(GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        });
        document.route = Some(route.to_string());
        let (status, body) = call(
            &harness,
            post_json(EVM_GOVERNANCE_PATH, Some(BEARER), &document),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{route}: {body}");
    }
    assert_eq!(harness.kms.calls(), 0);
}

/// The endpoint is authenticated exactly like the others, and an
/// unauthenticated caller learns nothing about the governance posture.
#[tokio::test]
async fn the_governance_endpoint_requires_the_bearer_token() {
    let harness = governance_harness(vec![ACTION_SET_PAUSE]);
    let document = governance_document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });

    for bearer in [None, Some("wrong-token-wrong-token-wrong-token-wrong-tok")] {
        let (status, body) =
            call(&harness, post_json(EVM_GOVERNANCE_PATH, bearer, &document)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        let rendered = body.to_string();
        assert!(!rendered.contains(BEARER), "the token must never be echoed");
        assert!(
            !rendered.contains("set_pause"),
            "an unauthenticated caller learns nothing: {rendered}"
        );
    }
    assert_eq!(harness.kms.calls(), 0);
}

/// No response body, in any posture, carries a token, a key id or key
/// material.
#[tokio::test]
async fn no_governance_response_leaks_a_credential() {
    let harness = governance_harness(vec![ACTION_SET_PAUSE]);
    let good = governance_document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    let mut bad = good.clone();
    bad.chain_id = 1;

    for document in [good, bad] {
        let (_, body) = call(
            &harness,
            post_json(EVM_GOVERNANCE_PATH, Some(BEARER), &document),
        )
        .await;
        let rendered = body.to_string();
        assert!(!rendered.contains(BEARER), "{rendered}");
        assert!(!rendered.to_lowercase().contains("private"), "{rendered}");
        assert!(!rendered.contains("kms_key"), "{rendered}");
    }
}

/// A wrong verb on the governance path is a 405, not a 404 — the same
/// distinction the other two paths make.
#[tokio::test]
async fn the_governance_path_answers_405_for_the_wrong_verb() {
    let harness = governance_harness(vec![ACTION_SET_PAUSE]);
    let (status, _) = call(&harness, get(EVM_GOVERNANCE_PATH, Some(BEARER))).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

/// v2 is untouched: the value-moving endpoint behaves identically on a
/// signer that has opted in to governance.
#[tokio::test]
async fn enabling_governance_does_not_change_the_value_moving_endpoint() {
    let with_governance = governance_harness(vec![
        ACTION_SET_LIMITS,
        ACTION_SET_PAUSE,
        ACTION_SET_ROUTE_ENABLED,
    ]);
    let without = harness();

    for request in [payout(), refund()] {
        let document = EvmAuthSignRequest::from_request(&request).unwrap();
        let (a_status, a_body) = call(
            &with_governance,
            post_json("/v2/sign-evm-auth", Some(BEARER), &document),
        )
        .await;
        let (b_status, b_body) = call(
            &without,
            post_json("/v2/sign-evm-auth", Some(BEARER), &document),
        )
        .await;
        assert_eq!(a_status, StatusCode::OK);
        assert_eq!(a_status, b_status);
        assert_eq!(
            a_body, b_body,
            "the governance opt-in must not change a value-moving answer"
        );
    }
}
