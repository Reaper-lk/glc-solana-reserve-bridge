//! Exercises the real wire protocol end-to-end against a real local HTTP
//! server (`hyper`, the same server-construction pattern already used by
//! `ops::health::serve` — no mock-HTTP crate) — never a mocked
//! `VaultSigner`/`AttestationSigner`. TLS itself is NOT exercised here:
//! the test server speaks plain HTTP, reached only via
//! `RemoteVaultSigner::connect_for_tests`/
//! `RemoteAttestationSigner::connect_for_tests` (test-only, `https://`
//! enforcement skipped). That enforcement has its own direct,
//! no-server-needed test (`https_scheme_is_required`) below. This split
//! is deliberate: standing up a real TLS certificate fixture would add
//! real complexity for no additional coverage of THIS module's own
//! logic, since certificate validation is reqwest/rustls's own, already
//! broadly-used code path, not something this module reimplements.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer as _};
use tokio::net::TcpListener;

use super::*;

const AUTH_TOKEN_ENV: &str = "GLC_TEST_REMOTE_SIGNER_TOKEN";
const AUTH_TOKEN_VALUE: &str = "s3cr3t-test-token-never-logged";

/// Every test in this module that needs `AUTH_TOKEN_ENV` sets it to this
/// exact same value — safe to call with no cross-test synchronization
/// even though `cargo test` runs tests in parallel threads within one
/// process (`std::env::set_var` is process-global): the set is
/// idempotent (every caller writes the identical value), and nothing in
/// this module ever removes or changes `AUTH_TOKEN_ENV` to a different
/// value mid-run — only the two dedicated env-var tests below
/// (`auth_token_missing_env_var_fails_closed`/
/// `auth_token_empty_env_var_fails_closed`) mutate a variable at all,
/// and each uses its own uniquely-named variable no other test touches.
fn set_auth_env() {
    // SAFETY: idempotent (every caller writes the same value) and this
    // specific variable is never removed/changed by any other test.
    unsafe {
        std::env::set_var(AUTH_TOKEN_ENV, AUTH_TOKEN_VALUE);
    }
}

#[derive(Clone)]
enum ServerBehavior {
    /// Real identity + real, correctly-signed responses.
    Normal,
    /// Identity endpoint reports a DIFFERENT (but real, validly-encoded)
    /// public key than the one actually used to sign — simulates a
    /// misconfigured/wrong endpoint.
    WrongIdentity,
    /// Sign endpoint returns well-formed hex that does not decode to a
    /// valid/matching signature — simulates a compromised or buggy
    /// signer.
    InvalidSignature,
    /// Sign endpoint returns a response body that isn't valid JSON.
    MalformedResponse,
    /// Sign endpoint returns `403` with a structured rejection body.
    Rejects,
    /// Sign endpoint never responds — the client's own timeout must
    /// fire.
    Hangs,
    /// Identity endpoint returns a `302` pointing at a canary server —
    /// proves the client never follows it (and never contacts the
    /// canary at all).
    RedirectsIdentity,
    /// Sign endpoint returns a `302` pointing at a canary server — same
    /// proof, for the other endpoint.
    RedirectsSign,
    /// Identity endpoint returns a body well over
    /// `MAX_RESPONSE_BODY_BYTES`, with a correct (honest)
    /// `Content-Length` — must be rejected before/without fully
    /// buffering it.
    OversizedIdentity,
    /// Sign endpoint returns an oversized body with a correct
    /// `Content-Length` — same proof, for the other endpoint.
    OversizedSign,
}

struct TestSigner {
    vault_secret: libsecp256k1::SecretKey,
    vault_pubkey: [u8; 33],
    attestation_keypair: Keypair,
    behavior: ServerBehavior,
    /// Only used by `RedirectsIdentity`/`RedirectsSign` — where the `302`
    /// `Location` header points.
    redirect_target: Option<SocketAddr>,
}

impl TestSigner {
    fn new(behavior: ServerBehavior) -> Self {
        Self::with_redirect_target(behavior, None)
    }

    fn with_redirect_target(behavior: ServerBehavior, redirect_target: Option<SocketAddr>) -> Self {
        let mut rng = rand::rngs::OsRng;
        let vault_secret = libsecp256k1::SecretKey::random(&mut rng);
        let vault_pubkey =
            libsecp256k1::PublicKey::from_secret_key(&vault_secret).serialize_compressed();
        TestSigner {
            vault_secret,
            vault_pubkey,
            attestation_keypair: Keypair::new(),
            behavior,
            redirect_target,
        }
    }

    fn vault_pubkey(&self) -> [u8; 33] {
        self.vault_pubkey
    }

    fn attestation_pubkey(&self) -> Pubkey {
        self.attestation_keypair.pubkey()
    }
}

/// Starts a real local server that records whether it was EVER contacted
/// (`Arc<AtomicBool>`, checked by the caller after the real request under
/// test completes) — used as a `302 Location` target to prove the client
/// under test genuinely never follows a redirect, not merely that it
/// handles the immediate `302` response correctly.
async fn spawn_canary_server() -> (SocketAddr, Arc<std::sync::atomic::AtomicBool>) {
    let contacted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let flag = Arc::clone(&contacted);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            let io = TokioIo::new(stream);
            let service = service_fn(|_req: Request<Incoming>| async {
                Ok::<_, Infallible>(json_response(StatusCode::OK, r#"{"public_key_hex":""}"#))
            });
            tokio::spawn(async move {
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    (addr, contacted)
}

/// Starts a real local server that speaks raw HTTP/1.1 chunked transfer
/// encoding (no `Content-Length` header at all) for exactly one request,
/// returning a body well over `MAX_RESPONSE_BODY_BYTES` — hyper's own
/// server (used by `spawn_test_server`) always sets a correct
/// `Content-Length` for a `Full<Bytes>` body, so this specific "declared
/// length absent/untrustworthy" case is written directly at the socket
/// level to actually exercise it.
async fn spawn_chunked_oversized_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        // Drain (and discard) whatever request was sent before replying.
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let chunk = "b".repeat(8192);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Transfer-Encoding: chunked\r\n\
             \r\n\
             {:x}\r\n\
             {chunk}\r\n\
             0\r\n\
             \r\n",
            chunk.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    });
    addr
}

/// Starts a real local plain-HTTP server implementing `/v1/identity`/
/// `/v1/sign` per `signing::remote` module docs, for exactly one of
/// `is_vault`/`!is_vault` (a real deployment runs one signer per
/// endpoint; these tests do the same). Returns the bound address; the
/// server runs until the process/test ends (no graceful shutdown needed
/// — each test binds a fresh OS-assigned port via `:0`).
async fn spawn_test_server(signer: Arc<TestSigner>, is_vault: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let signer = Arc::clone(&signer);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| handle(req, Arc::clone(&signer), is_vault));
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    addr
}

async fn handle(
    req: Request<Incoming>,
    signer: Arc<TestSigner>,
    is_vault: bool,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let auth_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {AUTH_TOKEN_VALUE}"));
    if !auth_ok {
        return Ok(json_response(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"unauthorized"}"#,
        ));
    }

    let path = req.uri().path().to_string();
    match (req.method().as_str(), path.as_str()) {
        ("GET", "/v1/identity") => Ok(handle_identity(&signer, is_vault)),
        ("POST", "/v1/sign") => {
            if matches!(signer.behavior, ServerBehavior::Hangs) {
                // Never respond — the caller's own client-side timeout
                // must fire. Sleeping far longer than any test's
                // configured timeout is sufficient; the connection is
                // dropped when the test ends.
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            let body = req.into_body().collect().await.unwrap().to_bytes();
            Ok(handle_sign(&signer, is_vault, &body))
        }
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":"not_found"}"#,
        )),
    }
}

fn redirect_response(target: SocketAddr) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::FOUND)
        .header("location", format!("http://{target}/v1/identity"))
        .body(Full::new(Bytes::new()))
        .unwrap()
}

fn oversized_response() -> Response<Full<Bytes>> {
    // Comfortably over MAX_RESPONSE_BODY_BYTES (4096), wrapped in an
    // otherwise well-formed JSON shape so a client that (incorrectly)
    // had no size limit at all would still be able to parse it — this
    // is testing the size bound specifically, not malformed JSON.
    let filler = "a".repeat(8192);
    let body = format!(r#"{{"public_key_hex": "{filler}", "signature_hex": "{filler}"}}"#);
    json_response(StatusCode::OK, &body)
}

fn handle_identity(signer: &TestSigner, is_vault: bool) -> Response<Full<Bytes>> {
    if matches!(signer.behavior, ServerBehavior::RedirectsIdentity) {
        return redirect_response(signer.redirect_target.expect("redirect_target must be set"));
    }
    if matches!(signer.behavior, ServerBehavior::OversizedIdentity) {
        return oversized_response();
    }
    let reported: Vec<u8> = match signer.behavior {
        ServerBehavior::WrongIdentity => {
            // A different, real key — never the one actually used to
            // sign.
            if is_vault {
                let mut rng = rand::rngs::OsRng;
                let sk = libsecp256k1::SecretKey::random(&mut rng);
                libsecp256k1::PublicKey::from_secret_key(&sk)
                    .serialize_compressed()
                    .to_vec()
            } else {
                Keypair::new().pubkey().to_bytes().to_vec()
            }
        }
        _ if is_vault => signer.vault_pubkey().to_vec(),
        _ => signer.attestation_pubkey().to_bytes().to_vec(),
    };
    let body = serde_json::json!({ "public_key_hex": crate::goldcoin::hex::encode(&reported) });
    json_response(StatusCode::OK, &body.to_string())
}

fn handle_sign(signer: &TestSigner, is_vault: bool, body: &[u8]) -> Response<Full<Bytes>> {
    if matches!(signer.behavior, ServerBehavior::RedirectsSign) {
        return redirect_response(signer.redirect_target.expect("redirect_target must be set"));
    }
    if matches!(signer.behavior, ServerBehavior::OversizedSign) {
        return oversized_response();
    }
    if matches!(signer.behavior, ServerBehavior::Rejects) {
        return json_response(
            StatusCode::FORBIDDEN,
            r#"{"error":"rejected","detail":"policy denied this request"}"#,
        );
    }
    if matches!(signer.behavior, ServerBehavior::MalformedResponse) {
        return json_response(StatusCode::OK, "{not valid json");
    }

    let req: serde_json::Value = serde_json::from_slice(body).unwrap();
    let payload_hex = req["payload_hex"].as_str().unwrap();
    let payload = crate::goldcoin::hex::decode_vec(payload_hex).unwrap();

    let signature_hex = if matches!(signer.behavior, ServerBehavior::InvalidSignature) {
        // Well-formed hex, well-formed length, but not a signature that
        // verifies — a compromised/buggy signer's response.
        if is_vault {
            // A validly DER-encoded signature over the WRONG message.
            let wrong = [0xAB; 32];
            let msg = libsecp256k1::Message::parse(&wrong);
            let (sig, _) = libsecp256k1::sign(&msg, &signer.vault_secret);
            crate::goldcoin::hex::encode(sig.serialize_der().as_ref())
        } else {
            let wrong_sig = signer.attestation_keypair.sign_message(b"wrong message");
            crate::goldcoin::hex::encode(wrong_sig.as_ref())
        }
    } else if is_vault {
        let sighash: [u8; 32] = payload.as_slice().try_into().unwrap();
        let msg = libsecp256k1::Message::parse(&sighash);
        let (sig, _) = libsecp256k1::sign(&msg, &signer.vault_secret);
        crate::goldcoin::hex::encode(sig.serialize_der().as_ref())
    } else {
        let sig = signer.attestation_keypair.sign_message(&payload);
        crate::goldcoin::hex::encode(sig.as_ref())
    };

    let body = serde_json::json!({ "signature_hex": signature_hex });
    json_response(StatusCode::OK, &body.to_string())
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn test_config(addr: SocketAddr, timeout: Duration) -> RemoteSignerConfig {
    RemoteSignerConfig {
        endpoint_url: format!("http://{addr}"),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout,
    }
}

// ------------------------------------------------------------- vault signer --

#[tokio::test]
async fn vault_successful_remote_signature() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let remote = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .expect("connect must succeed against a well-behaved signer");
    assert_eq!(remote.public_key(), signer.vault_pubkey());

    let sighash = [0x11; 32];
    let der = remote
        .sign_sighash(&sighash)
        .await
        .expect("sign must succeed");
    assert!(
        crate::goldcoin::multisig::verify_partial(&signer.vault_pubkey(), &sighash, &der),
        "returned signature must actually verify"
    );
}

#[tokio::test]
async fn vault_public_key_mismatch_refuses_to_connect() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::WrongIdentity));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let err = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .expect_err("mismatched identity must fail closed at connect time");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn vault_sign_timeout() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Hangs));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    // Identity must still answer normally for connect() to succeed —
    // only /v1/sign hangs (see handle()) — so use a real timeout for
    // connect, then a short one for the sign call itself.
    let connect_cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteVaultSigner::connect_for_tests(&connect_cfg, signer.vault_pubkey())
        .await
        .unwrap();

    // Rebuild with a short timeout for the actual timeout assertion:
    // RemoteSignerConfig::timeout governs the reqwest client's own
    // request timeout, set once at construction.
    let short_cfg = test_config(addr, Duration::from_millis(200));
    let remote_short = RemoteVaultSigner::connect_for_tests(&short_cfg, signer.vault_pubkey())
        .await
        .unwrap();
    let _ = remote; // constructed only to prove a normal connect still works against this server

    let err = remote_short
        .sign_sighash(&[0x22; 32])
        .await
        .expect_err("a hanging signer must time out, not hang forever");
    assert!(matches!(err, SignerError::Timeout { .. }), "{err:?}");
}

#[tokio::test]
async fn vault_connection_failure() {
    set_auth_env();
    // A closed port on localhost: real connection-level failure, no
    // server listening at all (matches ops::alerting's own test
    // convention for "deliberately unreachable").
    let cfg = RemoteSignerConfig {
        endpoint_url: "http://127.0.0.1:1".to_string(),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout: Duration::from_secs(2),
    };
    let err = RemoteVaultSigner::connect_for_tests(&cfg, [0u8; 33])
        .await
        .expect_err("an unreachable endpoint must fail closed at connect time");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn vault_signer_rejection() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Rejects));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .unwrap();

    let err = remote
        .sign_sighash(&[0x33; 32])
        .await
        .expect_err("an explicit rejection must surface as an error");
    match err {
        SignerError::Rejected { detail, .. } => assert!(detail.contains("policy denied")),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn vault_malformed_response() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::MalformedResponse));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .unwrap();

    let err = remote
        .sign_sighash(&[0x44; 32])
        .await
        .expect_err("a malformed response body must never be silently accepted");
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
}

#[tokio::test]
async fn vault_invalid_signature_is_rejected_by_local_verification() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::InvalidSignature));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .unwrap();

    let err = remote
        .sign_sighash(&[0x55; 32])
        .await
        .expect_err("a well-formed but non-verifying signature must be rejected locally");
    // Untrustworthy, NOT Rejected: the endpoint did not decline, it
    // answered wrong. A caller must be able to tell those apart.
    assert!(!err.is_tolerable(), "{err:?}");
    match &err {
        SignerError::Untrustworthy { detail, .. } => {
            assert!(detail.contains("fails local verification"))
        }
        other => panic!("expected Untrustworthy, got {other:?}"),
    }
}

// -------------------------------------------------------- attestation signer --

#[tokio::test]
async fn attestation_successful_remote_signature() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let remote = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .expect("connect must succeed against a well-behaved signer");
    assert_eq!(remote.pubkey(), signer.attestation_pubkey());

    let message = b"canonical claim message bytes";
    let signature = remote
        .sign_message(message)
        .await
        .expect("sign must succeed");
    assert!(signature.verify(signer.attestation_pubkey().as_ref(), message));
}

#[tokio::test]
async fn attestation_public_key_mismatch_refuses_to_connect() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::WrongIdentity));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let err = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .expect_err("mismatched identity must fail closed at connect time");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn attestation_sign_timeout() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Hangs));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let short_cfg = test_config(addr, Duration::from_millis(200));
    let remote =
        RemoteAttestationSigner::connect_for_tests(&short_cfg, signer.attestation_pubkey())
            .await
            .unwrap();

    let err = remote
        .sign_message(b"message")
        .await
        .expect_err("a hanging signer must time out, not hang forever");
    assert!(matches!(err, SignerError::Timeout { .. }), "{err:?}");
}

#[tokio::test]
async fn attestation_connection_failure() {
    set_auth_env();
    let cfg = RemoteSignerConfig {
        endpoint_url: "http://127.0.0.1:1".to_string(),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout: Duration::from_secs(2),
    };
    let err = RemoteAttestationSigner::connect_for_tests(&cfg, Pubkey::new_unique())
        .await
        .expect_err("an unreachable endpoint must fail closed at connect time");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn attestation_signer_rejection() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Rejects));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .unwrap();

    let err = remote
        .sign_message(b"message")
        .await
        .expect_err("an explicit rejection must surface as an error");
    match err {
        SignerError::Rejected { detail, .. } => assert!(detail.contains("policy denied")),
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn attestation_malformed_response() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::MalformedResponse));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .unwrap();

    let err = remote
        .sign_message(b"message")
        .await
        .expect_err("a malformed response body must never be silently accepted");
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
}

#[tokio::test]
async fn attestation_invalid_signature_is_rejected_by_local_verification() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::InvalidSignature));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .unwrap();

    let err = remote
        .sign_message(b"message")
        .await
        .expect_err("a well-formed but non-verifying signature must be rejected locally");
    // Untrustworthy, NOT Rejected: the endpoint did not decline, it
    // answered wrong. A caller must be able to tell those apart.
    assert!(!err.is_tolerable(), "{err:?}");
    match &err {
        SignerError::Untrustworthy { detail, .. } => {
            assert!(detail.contains("fails local verification"))
        }
        other => panic!("expected Untrustworthy, got {other:?}"),
    }
}

// ----------------------------------------------------------------- scheme --

#[test]
fn https_scheme_is_required() {
    let insecure = RemoteSignerConfig {
        endpoint_url: "http://example.com".to_string(),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout: Duration::from_secs(5),
    };
    assert!(matches!(
        insecure.validate_scheme(),
        Err(RemoteSignerConfigError::InsecureEndpoint { .. })
    ));

    let secure = RemoteSignerConfig {
        endpoint_url: "https://example.com".to_string(),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout: Duration::from_secs(5),
    };
    assert!(secure.validate_scheme().is_ok());
}

// ------------------------------------------------------------------ secrets --

#[test]
fn auth_token_is_never_exposed_via_debug_formatting() {
    let token = AuthToken("this-is-the-real-secret-value".to_string());
    let formatted = format!("{token:?}");
    assert!(!formatted.contains("this-is-the-real-secret-value"));
    assert!(formatted.contains("redacted"));
}

#[test]
fn auth_token_missing_env_var_fails_closed() {
    // Uniquely-named variable, touched by no other test — safe without
    // cross-test synchronization (see `set_auth_env`'s docs).
    let var = "GLC_TEST_REMOTE_SIGNER_TOKEN_DEFINITELY_UNSET";
    // SAFETY: this variable is never set anywhere, by this or any other
    // test, so removing it (in case a prior run of this same test left
    // it set — it doesn't, but this keeps the test self-contained) races
    // with nothing.
    unsafe {
        std::env::remove_var(var);
    }
    let err = AuthToken::from_env(var).unwrap_err();
    assert!(matches!(
        err,
        RemoteSignerConfigError::AuthTokenMissing { .. }
    ));
}

#[test]
fn auth_token_empty_env_var_fails_closed() {
    // Uniquely-named variable, touched by no other test — safe without
    // cross-test synchronization (see `set_auth_env`'s docs).
    let var = "GLC_TEST_REMOTE_SIGNER_TOKEN_EMPTY";
    // SAFETY: uniquely-named, touched by no other test.
    unsafe {
        std::env::set_var(var, "");
    }
    let err = AuthToken::from_env(var).unwrap_err();
    assert!(matches!(
        err,
        RemoteSignerConfigError::AuthTokenEmpty { .. }
    ));
    // SAFETY: uniquely-named, touched by no other test.
    unsafe {
        std::env::remove_var(var);
    }
}

// ---------------------------------------------------------------- redirects --

#[tokio::test]
async fn vault_identity_redirect_is_rejected_and_target_never_contacted() {
    set_auth_env();
    let (canary_addr, contacted) = spawn_canary_server().await;
    let signer = Arc::new(TestSigner::with_redirect_target(
        ServerBehavior::RedirectsIdentity,
        Some(canary_addr),
    ));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let err = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .expect_err("a 302 must never be followed — connect must fail closed");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
    assert!(
        !contacted.load(std::sync::atomic::Ordering::SeqCst),
        "the redirect target must never actually be contacted"
    );
}

#[tokio::test]
async fn vault_sign_redirect_is_rejected_and_target_never_contacted() {
    set_auth_env();
    let (canary_addr, contacted) = spawn_canary_server().await;
    // Identity must answer normally so connect() succeeds; only /v1/sign
    // redirects — mirrors the timeout test's own pattern for isolating
    // which endpoint is under test.
    let normal_signer = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&normal_signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteVaultSigner::connect_for_tests(&cfg, normal_signer.vault_pubkey())
        .await
        .unwrap();
    let _ = &remote; // constructed against the Normal server only to prove connect works

    let redirect_signer = Arc::new(TestSigner::with_redirect_target(
        ServerBehavior::RedirectsSign,
        Some(canary_addr),
    ));
    let redirect_addr = spawn_test_server(Arc::clone(&redirect_signer), true).await;
    let redirect_cfg = test_config(redirect_addr, Duration::from_secs(5));
    let remote_redirect =
        RemoteVaultSigner::connect_for_tests(&redirect_cfg, redirect_signer.vault_pubkey())
            .await
            .unwrap();

    let err = remote_redirect
        .sign_sighash(&[0x66; 32])
        .await
        .expect_err("a 302 from /v1/sign must never be followed");
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
    assert!(
        !contacted.load(std::sync::atomic::Ordering::SeqCst),
        "the redirect target must never actually be contacted"
    );
}

#[tokio::test]
async fn attestation_identity_redirect_is_rejected_and_target_never_contacted() {
    set_auth_env();
    let (canary_addr, contacted) = spawn_canary_server().await;
    let signer = Arc::new(TestSigner::with_redirect_target(
        ServerBehavior::RedirectsIdentity,
        Some(canary_addr),
    ));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let err = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .expect_err("a 302 must never be followed — connect must fail closed");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
    assert!(!contacted.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn attestation_sign_redirect_is_rejected_and_target_never_contacted() {
    set_auth_env();
    let (canary_addr, contacted) = spawn_canary_server().await;
    let redirect_signer = Arc::new(TestSigner::with_redirect_target(
        ServerBehavior::RedirectsSign,
        Some(canary_addr),
    ));
    let redirect_addr = spawn_test_server(Arc::clone(&redirect_signer), false).await;
    let redirect_cfg = test_config(redirect_addr, Duration::from_secs(5));
    let remote_redirect = RemoteAttestationSigner::connect_for_tests(
        &redirect_cfg,
        redirect_signer.attestation_pubkey(),
    )
    .await
    .unwrap();

    let err = remote_redirect
        .sign_message(b"message")
        .await
        .expect_err("a 302 from /v1/sign must never be followed");
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
    assert!(!contacted.load(std::sync::atomic::Ordering::SeqCst));
}

// ------------------------------------------------------------- oversized body --

#[tokio::test]
async fn vault_identity_oversized_response_is_rejected() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::OversizedIdentity));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let err = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .expect_err("an oversized identity response must be rejected, not buffered");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn vault_sign_oversized_response_is_rejected() {
    set_auth_env();
    let normal_signer = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&normal_signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));
    let remote = RemoteVaultSigner::connect_for_tests(&cfg, normal_signer.vault_pubkey())
        .await
        .unwrap();
    let _ = &remote;

    let oversized_signer = Arc::new(TestSigner::new(ServerBehavior::OversizedSign));
    let oversized_addr = spawn_test_server(Arc::clone(&oversized_signer), true).await;
    let oversized_cfg = test_config(oversized_addr, Duration::from_secs(5));
    let remote_oversized =
        RemoteVaultSigner::connect_for_tests(&oversized_cfg, oversized_signer.vault_pubkey())
            .await
            .unwrap();

    let err = remote_oversized
        .sign_sighash(&[0x77; 32])
        .await
        .expect_err("an oversized sign response must be rejected, not buffered");
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
}

#[tokio::test]
async fn attestation_identity_oversized_response_is_rejected() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::OversizedIdentity));
    let addr = spawn_test_server(Arc::clone(&signer), false).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    let err = RemoteAttestationSigner::connect_for_tests(&cfg, signer.attestation_pubkey())
        .await
        .expect_err("an oversized identity response must be rejected, not buffered");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn attestation_sign_oversized_response_is_rejected() {
    set_auth_env();
    let oversized_signer = Arc::new(TestSigner::new(ServerBehavior::OversizedSign));
    let oversized_addr = spawn_test_server(Arc::clone(&oversized_signer), false).await;
    let oversized_cfg = test_config(oversized_addr, Duration::from_secs(5));
    let remote_oversized = RemoteAttestationSigner::connect_for_tests(
        &oversized_cfg,
        oversized_signer.attestation_pubkey(),
    )
    .await
    .unwrap();

    let err = remote_oversized
        .sign_message(b"message")
        .await
        .expect_err("an oversized sign response must be rejected, not buffered");
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
}

#[tokio::test]
async fn chunked_response_with_no_content_length_is_still_bounded() {
    // Content-Length is absent entirely (genuine HTTP/1.1 chunked
    // transfer encoding, written at the raw socket level — see
    // spawn_chunked_oversized_server's docs) — proves the running-total
    // check during streaming catches an oversized body even when there
    // was never a declared length to reject upfront.
    set_auth_env();
    let addr = spawn_chunked_oversized_server().await;
    let cfg = RemoteSignerConfig {
        endpoint_url: format!("http://{addr}"),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout: Duration::from_secs(5),
    };
    let err = RemoteVaultSigner::connect_for_tests(&cfg, [0u8; 33])
        .await
        .expect_err("a chunked, oversized, Content-Length-less body must still be rejected");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

// --------------------------------------------------------- explicit auth failure --

#[tokio::test]
async fn wrong_auth_token_is_rejected_and_never_appears_in_the_error() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    // A config pointing at a DIFFERENT env var holding a WRONG token —
    // the server's own auth check (see handle()) compares against
    // AUTH_TOKEN_VALUE and returns 401 for anything else.
    let wrong_var = "GLC_TEST_CFG_WRONG_TOKEN";
    // SAFETY: uniquely-named, touched by no other test.
    unsafe {
        std::env::set_var(wrong_var, "totally-wrong-token-value");
    }
    let cfg = RemoteSignerConfig {
        endpoint_url: format!("http://{addr}"),
        auth_token_env: wrong_var.to_string(),
        timeout: Duration::from_secs(5),
    };

    let err = RemoteVaultSigner::connect_for_tests(&cfg, signer.vault_pubkey())
        .await
        .expect_err("a 401 must fail closed at connect time (identity is fetched first)");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
    let message = format!("{err}");
    assert!(
        !message.contains("totally-wrong-token-value"),
        "the wrong token value must never appear in the error text: {message}"
    );
    assert!(
        !format!("{err:?}").contains("totally-wrong-token-value"),
        "the wrong token value must never appear in Debug output either"
    );
}

// -------------------------------------------------------- exact identity match --

#[tokio::test]
async fn identity_comparison_is_byte_exact_not_a_near_match() {
    set_auth_env();
    let signer = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&signer), true).await;
    let cfg = test_config(addr, Duration::from_secs(5));

    // Flip exactly one bit relative to the server's real public key —
    // everything else about the request (auth, endpoint, format) is
    // otherwise identical and would succeed.
    let mut near_match = signer.vault_pubkey();
    near_match[32] ^= 0x01;
    assert_ne!(near_match, signer.vault_pubkey());

    let err = RemoteVaultSigner::connect_for_tests(&cfg, near_match)
        .await
        .expect_err("a one-bit-different \"expected\" key must never be treated as a match");
    assert!(
        matches!(err, RemoteSignerConfigError::ClientBuild { .. }),
        "{err:?}"
    );
}

// =====================================================================
// Protocol v2 — Robinhood EIP-712 authorization signing
// =====================================================================
//
// Same discipline as the v1 tests above: a REAL local HTTP server
// speaking the real wire protocol, never a mocked `EvmAuthSigner`. What
// these prove that the v1 tests cannot is the inversion v2 exists for —
// the custody domain derives the digest from the structured request and
// signs the one IT computed, so a client and a server that disagree about
// what an authorization means produce a refusal rather than a signature
// over the wrong thing.

use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::secp::EvmSecretKey;
use crate::evm::{EvmAddress, EvmChainId};
use crate::robinhood::auth::{
    BridgeDomain, EvmAuthRequest, PayoutAuth, ProtocolChainPair, SettlementAuth,
};
use crate::robinhood::signer::EvmAuthSigner;
use crate::routes::Route;
use crate::signing::evm_policy::{EvmAuthSignRequest, EvmSignerPolicy};

/// How the v2 test server misbehaves, if at all.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EvmBehavior {
    /// Runs the real policy and signs the digest it derived.
    Normal,
    /// Reports an address it does not control.
    WrongIdentity,
    /// Signs a DIFFERENT authorization than the one it was sent — the
    /// case the whole design exists to catch. A blind-oracle signer
    /// could not tell the difference; this client can.
    SignsSomethingElse,
    /// Answers `404` on the v2 paths, as a signer process that only
    /// implements v1 would.
    V1Only,
    /// Runs the policy and REFUSES, as a domain declining a request.
    PolicyRefuses,
}

struct EvmTestSigner {
    key: EvmSecretKey,
    behavior: EvmBehavior,
    /// The policy this "custody domain" independently holds.
    policy: EvmSignerPolicy,
    /// Bumped on every signature actually issued.
    signed: Arc<std::sync::atomic::AtomicUsize>,
}

const EVM_CHAIN_ID: u64 = 4663;
const EVM_NOW: u64 = 1_800_000_000;

fn evm_addr(byte: u8) -> EvmAddress {
    EvmAddress::from_bytes([byte; 20])
}

fn evm_bridge() -> EvmAddress {
    evm_addr(0xb1)
}

fn evm_token() -> EvmAddress {
    evm_addr(0x70)
}

fn evm_domain() -> BridgeDomain {
    BridgeDomain::new(EvmChainId::new(EVM_CHAIN_ID).unwrap(), evm_bridge())
}

fn evm_chains() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 1001,
        dest: 2001,
    }
}

fn evm_policy() -> EvmSignerPolicy {
    EvmSignerPolicy {
        chain_id: EvmChainId::new(EVM_CHAIN_ID).unwrap(),
        verifying_contract: evm_bridge(),
        token: evm_token(),
        allowed_actions: vec![0x01, 0x02, 0x03],
        allowed_routes: vec![Route::GlcToRhn, Route::RhnToGlc],
        route_chains: vec![
            (Route::GlcToRhn, evm_chains()),
            (
                Route::RhnToGlc,
                ProtocolChainPair {
                    source: 2001,
                    dest: 1001,
                },
            ),
        ],
        max_amount_robinhood_atomic: 10_000 * 1_000_000_000_000_000_000,
        max_authorization_ttl_secs: 3_600,
        expected_signer_epoch: None,
    }
}

fn evm_payout(amount_glc: u64) -> EvmAuthRequest {
    EvmAuthRequest::payout(
        evm_domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: evm_chains(),
            token: evm_token(),
            request_id: [0x11; 32],
            recipient: evm_addr(0xc0),
            amount: RobinhoodAtomic::new(u128::from(amount_glc) * 1_000_000_000_000_000_000),
            signer_epoch: 7,
            expiry: EVM_NOW + 900,
        },
    )
}

/// The authorization a misbehaving server signs INSTEAD of the one it was
/// sent: a different recipient and a much larger amount.
fn evm_attackers_payout() -> EvmAuthRequest {
    EvmAuthRequest::payout(
        evm_domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: evm_chains(),
            token: evm_token(),
            request_id: [0x11; 32],
            recipient: evm_addr(0xee),
            amount: RobinhoodAtomic::new(9_000 * 1_000_000_000_000_000_000),
            signer_epoch: 7,
            expiry: EVM_NOW + 900,
        },
    )
}

async fn spawn_evm_server(signer: Arc<EvmTestSigner>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let signer = Arc::clone(&signer);
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req| handle_evm(req, Arc::clone(&signer)));
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    addr
}

async fn handle_evm(
    req: Request<Incoming>,
    signer: Arc<EvmTestSigner>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let auth_ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {AUTH_TOKEN_VALUE}"));
    if !auth_ok {
        return Ok(json_response(
            StatusCode::UNAUTHORIZED,
            r#"{"error":"unauthorized"}"#,
        ));
    }
    if signer.behavior == EvmBehavior::V1Only {
        // Exactly what a signer process predating v2 does: it has no
        // handler for these paths.
        return Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":"not_found","detail":"unknown path"}"#,
        ));
    }

    let path = req.uri().path().to_string();
    match (req.method().as_str(), path.as_str()) {
        ("GET", "/v2/evm-identity") => {
            let address = if signer.behavior == EvmBehavior::WrongIdentity {
                evm_addr(0xfe)
            } else {
                signer.key.address()
            };
            Ok(json_response(
                StatusCode::OK,
                &format!(r#"{{"address":"{}"}}"#, address.to_checksum_string()),
            ))
        }
        ("POST", "/v2/sign-evm-auth") => {
            let body = req.into_body().collect().await.unwrap().to_bytes();
            let document: EvmAuthSignRequest = match serde_json::from_slice(&body) {
                Ok(d) => d,
                Err(e) => {
                    return Ok(json_response(
                        StatusCode::BAD_REQUEST,
                        &format!(r#"{{"error":"malformed","detail":"{e}"}}"#),
                    ))
                }
            };

            // THE custody-domain step: derive the digest from the
            // structured fields, never take the caller's word for it.
            let decision = match signer.policy.evaluate(&document, EVM_NOW) {
                Ok(d) => d,
                Err(e) => {
                    return Ok(json_response(
                        StatusCode::FORBIDDEN,
                        &format!(r#"{{"error":"policy","detail":"{e}"}}"#),
                    ))
                }
            };
            if signer.behavior == EvmBehavior::PolicyRefuses {
                return Ok(json_response(
                    StatusCode::FORBIDDEN,
                    r#"{"error":"policy","detail":"this domain declines"}"#,
                ));
            }

            let digest = if signer.behavior == EvmBehavior::SignsSomethingElse {
                evm_attackers_payout().digest().unwrap()
            } else {
                decision.digest
            };
            let signature = crate::evm::secp::sign_digest(&signer.key, &digest);
            signer
                .signed
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json_response(
                StatusCode::OK,
                &format!(
                    r#"{{"signature_hex":"{}"}}"#,
                    crate::goldcoin::hex::encode(&signature.to_bytes())
                ),
            ))
        }
        _ => Ok(json_response(
            StatusCode::NOT_FOUND,
            r#"{"error":"not_found"}"#,
        )),
    }
}

fn evm_test_signer(behavior: EvmBehavior) -> Arc<EvmTestSigner> {
    let mut bytes = [0u8; 32];
    bytes[31] = 0x41;
    Arc::new(EvmTestSigner {
        key: EvmSecretKey::from_bytes(&bytes).unwrap(),
        behavior,
        policy: evm_policy(),
        signed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

// --------------------------------------------------------------- tests --

/// The happy path, end to end over a real socket: the client sends the
/// structured authorization, the domain derives the digest itself, and
/// the returned signature verifies against the digest the CLIENT derived
/// independently.
#[tokio::test]
async fn a_production_signer_signs_an_authorization_and_the_signature_verifies() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::Normal);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .expect("the endpoint's identity matches");

    let request = evm_payout(5);
    let signature = signer
        .sign_authorization(&request)
        .await
        .expect("a well-formed authorization is signed");

    let recovered =
        crate::evm::secp::recover_address(&request.digest().unwrap(), &signature).unwrap();
    assert_eq!(recovered, server.key.address());
    assert_eq!(server.signed.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// The property the whole v2 design exists for. A domain that signs a
/// DIFFERENT authorization than the one it was asked about produces a
/// signature that does not recover to its own address over the digest
/// this side derived — so it is refused, not stored.
#[tokio::test]
async fn a_signer_that_signs_a_different_authorization_is_refused() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::SignsSomethingElse);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .unwrap();

    let err = signer
        .sign_authorization(&evm_payout(5))
        .await
        .expect_err("a signature over a different payload must be refused");
    assert!(
        matches!(err, SignerError::Untrustworthy { .. }),
        "answering wrong is not the same as declining: {err:?}"
    );
    assert!(!err.is_tolerable(), "a quorum must never route around this");
    // The server DID issue a signature — it is this client that refused
    // to accept it, which is the point.
    assert_eq!(server.signed.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// A signer process that predates v2 answers `404` on the new paths. That
/// must fail closed at CONNECT time, so a deployment cannot start
/// believing it has a quorum it does not have — and must never silently
/// fall back to the weaker v1 "sign these bytes" request.
#[tokio::test]
async fn a_v1_only_signer_process_fails_closed_at_connect() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::V1Only);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let err = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .expect_err("a signer that does not speak v2 must not be constructed");
    let rendered = err.to_string();
    assert!(rendered.contains("evm-identity"), "{rendered}");
    assert_eq!(server.signed.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_endpoint_reporting_a_different_address_is_never_constructed() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::WrongIdentity);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let err = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .expect_err("an identity mismatch must fail closed");
    assert!(err.to_string().contains("does not match"), "{err}");
}

/// A domain declining is a liveness-shaped answer for the quorum: one
/// refusal is what a 2-of-3 tolerates, and it must be a typed `Rejected`
/// rather than a panic or a silent empty signature.
#[tokio::test]
async fn a_domain_that_declines_reports_a_typed_refusal() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::PolicyRefuses);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .unwrap();
    let err = signer.sign_authorization(&evm_payout(5)).await.unwrap_err();
    assert!(matches!(err, SignerError::Rejected { .. }), "{err:?}");
    assert!(err.to_string().contains("this domain declines"), "{err}");
}

/// The domain's OWN policy runs on the real wire document — a request
/// above its ceiling is refused server-side, not merely client-side.
#[tokio::test]
async fn the_domains_own_policy_refuses_an_amount_above_its_ceiling() {
    set_auth_env();
    let mut server = evm_test_signer(EvmBehavior::Normal);
    Arc::get_mut(&mut server)
        .unwrap()
        .policy
        .max_amount_robinhood_atomic = 1_000_000_000_000_000_000; // 1 GLC
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .unwrap();

    let err = signer
        .sign_authorization(&evm_payout(5))
        .await
        .expect_err("5 GLC is above this domain's 1 GLC ceiling");
    assert!(err.to_string().contains("ceiling"), "{err}");
    assert_eq!(
        server.signed.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no signature may be issued for a refused request"
    );
}

/// A settlement is a different payload family with different bound
/// fields; it must cross the same wire correctly.
#[tokio::test]
async fn a_settlement_authorization_round_trips_over_the_wire() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::Normal);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        server.key.address(),
    )
    .await
    .unwrap();

    let request = EvmAuthRequest::settlement(
        evm_domain(),
        SettlementAuth {
            route: Route::RhnToGlc,
            chains: ProtocolChainPair {
                source: 2001,
                dest: 1001,
            },
            request_id: [0x33; 32],
            obligation_index: 42,
            signer_epoch: 7,
            expiry: EVM_NOW + 900,
        },
    );
    let signature = signer.sign_authorization(&request).await.expect("signed");
    assert_eq!(
        crate::evm::secp::recover_address(&request.digest().unwrap(), &signature).unwrap(),
        server.key.address()
    );
}

/// v2 is additive: the v1 endpoints keep working exactly as before, and
/// nothing in the v2 client touches them.
#[tokio::test]
async fn the_v1_protocol_is_unaffected_by_the_v2_extension() {
    set_auth_env();
    let v1 = Arc::new(TestSigner::new(ServerBehavior::Normal));
    let addr = spawn_test_server(Arc::clone(&v1), true).await;
    let vault = RemoteVaultSigner::connect_for_tests(
        &test_config(addr, Duration::from_secs(5)),
        v1.vault_pubkey(),
    )
    .await
    .expect("a v1 vault signer still connects against a v1 server");
    let sighash = [0x5au8; 32];
    let der = vault.sign_sighash(&sighash).await.expect("v1 still signs");
    assert!(crate::goldcoin::multisig::verify_partial(
        &v1.vault_pubkey(),
        &sighash,
        &der
    ));
}

/// The bearer token is scoped per endpoint and never logged. A v2 signer
/// presenting no credential is refused like any other.
#[tokio::test]
async fn the_v2_endpoints_require_the_bearer_token() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::Normal);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let cfg = RemoteSignerConfig {
        endpoint_url: format!("http://{addr}"),
        auth_token_env: "GLC_TEST_EVM_MISSING_TOKEN_VAR".to_string(),
        timeout: Duration::from_secs(5),
    };
    let err = RemoteEvmAuthSigner::connect_for_tests(&cfg, server.key.address())
        .await
        .expect_err("a missing token must fail closed");
    assert!(
        matches!(err, RemoteSignerConfigError::AuthTokenMissing { .. }),
        "{err:?}"
    );
}

/// `https://` is enforced for the v2 client exactly as for v1 — a
/// plaintext signer endpoint defeats the premise.
#[tokio::test]
async fn the_v2_client_requires_https() {
    set_auth_env();
    let insecure = RemoteSignerConfig {
        endpoint_url: "http://signer.invalid".to_string(),
        auth_token_env: AUTH_TOKEN_ENV.to_string(),
        timeout: Duration::from_millis(50),
    };
    let err = RemoteEvmAuthSigner::connect(&insecure, evm_addr(0x01))
        .await
        .expect_err("plaintext must be refused before any request is sent");
    assert!(
        matches!(err, RemoteSignerConfigError::InsecureEndpoint { .. }),
        "{err:?}"
    );
}

// ---------------------------------------- the production quorum, for real --
//
// `signer::tests` covers the quorum RULES against in-process dev signers.
// These cover the thing that was actually missing in Phase F: that a
// quorum forms at all when the signers are separate PROCESSES reached
// over the wire, each running its own policy.

fn evm_test_signer_with_key(behavior: EvmBehavior, key_byte: u8) -> Arc<EvmTestSigner> {
    let mut bytes = [0u8; 32];
    bytes[31] = key_byte;
    Arc::new(EvmTestSigner {
        key: EvmSecretKey::from_bytes(&bytes).unwrap(),
        behavior,
        policy: evm_policy(),
        signed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

/// Stands up `count` independent custody domains and returns the
/// connected pool plus the authorized set the contract would hold.
async fn connect_pool(
    behaviors: &[EvmBehavior],
) -> (
    Vec<Box<dyn EvmAuthSigner>>,
    Vec<EvmAddress>,
    Vec<Arc<EvmTestSigner>>,
) {
    let mut pool: Vec<Box<dyn EvmAuthSigner>> = Vec::new();
    let mut authorized = Vec::new();
    let mut servers = Vec::new();
    for (i, behavior) in behaviors.iter().enumerate() {
        let server = evm_test_signer_with_key(*behavior, 0x41 + i as u8);
        let addr = spawn_evm_server(Arc::clone(&server)).await;
        authorized.push(server.key.address());
        // A signer that refuses to connect is simply absent from the
        // pool, which is exactly how a deployment with an unreachable
        // custody domain starts.
        if let Ok(signer) = RemoteEvmAuthSigner::connect_for_tests(
            &test_config(addr, Duration::from_secs(5)),
            server.key.address(),
        )
        .await
        {
            pool.push(Box::new(signer));
        }
        servers.push(server);
    }
    (pool, authorized, servers)
}

/// Phase F's launch blocker A, closed: a production-shaped pool of remote
/// EIP-712 signers assembles a real 2-of-3 quorum.
#[tokio::test]
async fn a_production_remote_pool_forms_a_quorum() {
    set_auth_env();
    let (pool, authorized, servers) = connect_pool(&[
        EvmBehavior::Normal,
        EvmBehavior::Normal,
        EvmBehavior::Normal,
    ])
    .await;
    assert_eq!(pool.len(), 3);

    let request = evm_payout(5);
    let quorum = crate::robinhood::signer::collect_quorum(
        &pool,
        &authorized,
        &request,
        Duration::from_secs(5),
    )
    .await
    .expect("three healthy custody domains form a quorum");

    assert_eq!(quorum.signatures.len(), 2);
    assert_ne!(
        quorum.signatures[0].0, quorum.signatures[1].0,
        "a quorum must be two DISTINCT custody domains"
    );
    assert_eq!(quorum.digest, request.digest().unwrap());
    for (address, signature) in quorum.signatures {
        assert!(authorized.contains(&address));
        assert_eq!(
            crate::evm::secp::recover_address(&quorum.digest, &signature).unwrap(),
            address
        );
    }
    // Exactly the threshold was asked; the third domain is spare
    // capacity, not a third signature.
    let total: usize = servers
        .iter()
        .map(|s| s.signed.load(std::sync::atomic::Ordering::SeqCst))
        .sum();
    assert_eq!(total, 2);
}

/// One domain down is what a 2-of-3 exists to tolerate.
#[tokio::test]
async fn a_quorum_still_forms_with_one_custody_domain_declining() {
    set_auth_env();
    let (pool, authorized, _) = connect_pool(&[
        EvmBehavior::PolicyRefuses,
        EvmBehavior::Normal,
        EvmBehavior::Normal,
    ])
    .await;
    crate::robinhood::signer::collect_quorum(
        &pool,
        &authorized,
        &evm_payout(5),
        Duration::from_secs(5),
    )
    .await
    .expect("one refusal out of three is tolerated");
}

/// Two down is not. A quorum that cannot form is a refusal, and nothing
/// downstream — no nonce, no gas, no broadcast — happens.
#[tokio::test]
async fn two_declining_domains_leave_the_quorum_unformable() {
    set_auth_env();
    let (pool, authorized, _) = connect_pool(&[
        EvmBehavior::PolicyRefuses,
        EvmBehavior::PolicyRefuses,
        EvmBehavior::Normal,
    ])
    .await;
    let err = crate::robinhood::signer::collect_quorum(
        &pool,
        &authorized,
        &evm_payout(5),
        Duration::from_secs(5),
    )
    .await
    .expect_err("one signature is not a quorum");
    assert!(
        matches!(
            err,
            crate::robinhood::signer::QuorumError::SignerFailed { .. }
                | crate::robinhood::signer::QuorumError::NotEnoughSigners { .. }
        ),
        "{err:?}"
    );
}

/// A domain signing a different authorization is NOT a liveness failure
/// and is not tolerated: the collection aborts rather than reaching past
/// it to the next signer.
#[tokio::test]
async fn a_domain_signing_something_else_aborts_the_whole_quorum() {
    set_auth_env();
    let (pool, authorized, _) = connect_pool(&[
        EvmBehavior::SignsSomethingElse,
        EvmBehavior::Normal,
        EvmBehavior::Normal,
    ])
    .await;
    let err = crate::robinhood::signer::collect_quorum(
        &pool,
        &authorized,
        &evm_payout(5),
        Duration::from_secs(5),
    )
    .await
    .expect_err("a wrong signature is an abort, never a fallback to the next domain");
    // Abandoned, not routed around: two honest domains were available
    // and a quorum COULD have formed from them. It must not.
    assert!(
        matches!(
            err,
            crate::robinhood::signer::QuorumError::Untrustworthy { .. }
        ),
        "{err:?}"
    );
}

/// A pool listing one custody domain twice is a quorum of one wearing a
/// quorum's clothes. Two connections to the SAME endpoint recover to the
/// same address and cannot both be counted.
#[tokio::test]
async fn one_domain_listed_twice_cannot_make_a_quorum() {
    set_auth_env();
    let server = evm_test_signer(EvmBehavior::Normal);
    let addr = spawn_evm_server(Arc::clone(&server)).await;
    let mut pool: Vec<Box<dyn EvmAuthSigner>> = Vec::new();
    for _ in 0..2 {
        pool.push(Box::new(
            RemoteEvmAuthSigner::connect_for_tests(
                &test_config(addr, Duration::from_secs(5)),
                server.key.address(),
            )
            .await
            .unwrap(),
        ));
    }
    let err = crate::robinhood::signer::collect_quorum(
        &pool,
        &[server.key.address()],
        &evm_payout(5),
        Duration::from_secs(5),
    )
    .await
    .expect_err("the same domain twice is not two domains");
    assert!(
        matches!(
            err,
            crate::robinhood::signer::QuorumError::NotEnoughSigners { .. }
        ),
        "{err:?}"
    );
}

/// A signer whose address is not in the contract's set would be refused
/// on-chain as `UnauthorizedSigner`. Catching it here means no gas is
/// spent finding out.
#[tokio::test]
async fn a_domain_outside_the_contracts_signer_set_is_refused_locally() {
    set_auth_env();
    let (pool, _, _) = connect_pool(&[EvmBehavior::Normal, EvmBehavior::Normal]).await;
    // An authorized set that names neither connected domain.
    let foreign = [evm_addr(0xa1), evm_addr(0xa2), evm_addr(0xa3)];
    let err = crate::robinhood::signer::collect_quorum(
        &pool,
        &foreign,
        &evm_payout(5),
        Duration::from_secs(5),
    )
    .await
    .expect_err("a signer outside the contract's set must be refused before broadcast");
    assert!(
        matches!(
            err,
            crate::robinhood::signer::QuorumError::NotAuthorized { .. }
        ),
        "{err:?}"
    );
}

// ---------------------------------- interop with the SHIPPED signer server --
//
// Every EVM test above answers with a hand-rolled server written in this
// file. That proves the CLIENT is correct against the protocol as this
// file understands it — which is exactly the thing that can silently
// drift once the repository also ships the server half
// (`signing::evm_kms`, the `glc-robinhood-kms-signer` binary).
//
// These two tests close that gap: the real `RemoteEvmAuthSigner` talks
// to the real `evm_kms::server`, over a real socket, with no shared code
// between them except the wire types both sides already depend on. If
// either half ever changes a path, a JSON field name, a hex spelling or
// a status-code mapping, one of these fails.

use crate::signing::evm_kms;

const KMS_INTEROP_TOKEN_ENV: &str = "GLC_TEST_KMS_SIGNER_INTEROP_TOKEN";
/// At least `evm_kms::config::MIN_BEARER_TOKEN_CHARS` long — the shipped
/// server refuses to be provisioned with anything shorter, which the
/// shorter `AUTH_TOKEN_VALUE` above is.
const KMS_INTEROP_TOKEN_VALUE: &str = "interop-token-0123456789abcdef0123456789abcdef";

/// Same idempotent-write discipline as [`set_auth_env`], on a variable no
/// other test in this crate touches.
fn set_kms_interop_auth_env() {
    // SAFETY: idempotent, and this variable is written by this function
    // only and never removed.
    unsafe {
        std::env::set_var(KMS_INTEROP_TOKEN_ENV, KMS_INTEROP_TOKEN_VALUE);
    }
}

struct InteropClock;

impl evm_kms::server::UnixClock for InteropClock {
    fn now_unix(&self) -> u64 {
        EVM_NOW
    }
}

/// Stands up the SHIPPED server on an ephemeral port and returns its
/// address alongside the fake KMS backing it.
async fn spawn_shipped_kms_signer() -> (SocketAddr, Arc<evm_kms::kms::fake::FakeKms>) {
    let kms = Arc::new(evm_kms::kms::fake::FakeKms::new(0x5a));
    let service = Arc::new(evm_kms::server::SignerService::with_clock(
        kms.address(),
        evm_policy(),
        evm_kms::config::BearerToken::new(KMS_INTEROP_TOKEN_VALUE).unwrap(),
        Arc::clone(&kms) as Arc<dyn evm_kms::kms::KmsDigestSigner>,
        Arc::new(InteropClock),
    ));

    // Bound here, handed over live — never bound, dropped and re-bound,
    // which would race anything else on the host for the port (see
    // `admin_api::serve_on`'s docs).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        // Held so the sender outlives the loop: dropping it would fire
        // `changed()` and shut the server down immediately.
        let _shutdown_tx = shutdown_tx;
        let _ = evm_kms::server::serve_on(listener, service, shutdown_rx).await;
    });
    (addr, kms)
}

/// The whole round trip: identity fetch, policy evaluation, KMS signing,
/// DER -> `r || s || v` conversion, and the client's own local recovery
/// check — across a socket, with the shipped code on both ends.
#[tokio::test]
async fn the_shipped_kms_signer_is_wire_compatible_with_this_client() {
    set_kms_interop_auth_env();
    let (addr, kms) = spawn_shipped_kms_signer().await;

    // `connect` itself is a compatibility assertion: it fetches
    // `GET /v2/evm-identity` and fails closed unless the address parses
    // and matches.
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &RemoteSignerConfig {
            endpoint_url: format!("http://{addr}"),
            auth_token_env: KMS_INTEROP_TOKEN_ENV.to_string(),
            timeout: Duration::from_secs(5),
        },
        kms.address(),
    )
    .await
    .expect("the shipped server must satisfy this client's identity handshake");

    let request = evm_payout(5);
    let signature = crate::robinhood::signer::EvmAuthSigner::sign_authorization(&signer, &request)
        .await
        .expect("the shipped server must satisfy this client's signing path");

    // The client already verified this locally; asserting it here names
    // what "compatible" means rather than trusting the client's `Ok`.
    assert_eq!(
        crate::evm::secp::recover_address(&request.digest().unwrap(), &signature).unwrap(),
        kms.address()
    );
    assert!(!crate::evm::secp::is_high_s(signature.s()));
    assert!(matches!(signature.v(), 27 | 28));

    // The server signed the digest IT derived, once.
    assert_eq!(kms.calls(), 1);
    assert_eq!(kms.signed_digests(), vec![request.digest().unwrap()]);
}

/// A refusal must cross the wire as a refusal. The shipped server answers
/// `403` when its own policy declines, and this client must report that
/// as `Rejected` — not as a transport problem, and never as a signature.
#[tokio::test]
async fn the_shipped_kms_signers_policy_refusal_reaches_this_client_as_a_rejection() {
    set_kms_interop_auth_env();
    let (addr, kms) = spawn_shipped_kms_signer().await;
    let signer = RemoteEvmAuthSigner::connect_for_tests(
        &RemoteSignerConfig {
            endpoint_url: format!("http://{addr}"),
            auth_token_env: KMS_INTEROP_TOKEN_ENV.to_string(),
            timeout: Duration::from_secs(5),
        },
        kms.address(),
    )
    .await
    .unwrap();

    // Above the domain's own 10_000 GLC ceiling.
    let err =
        crate::robinhood::signer::EvmAuthSigner::sign_authorization(&signer, &evm_payout(20_000))
            .await
            .expect_err("an authorization above the domain's ceiling must be refused");
    assert!(
        matches!(err, SignerError::Rejected { .. }),
        "a policy refusal must be Rejected, not Unavailable or Untrustworthy: {err:?}"
    );
    assert_eq!(kms.calls(), 0, "a refused request never reaches the key");
}
