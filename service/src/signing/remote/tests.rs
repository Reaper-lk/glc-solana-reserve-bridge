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
    match err {
        SignerError::Rejected { detail, .. } => {
            assert!(detail.contains("fails local verification"))
        }
        other => panic!("expected Rejected, got {other:?}"),
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
    match err {
        SignerError::Rejected { detail, .. } => {
            assert!(detail.contains("fails local verification"))
        }
        other => panic!("expected Rejected, got {other:?}"),
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
