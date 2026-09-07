//! Production-capable remote signer client (docs/22-production-readiness-
//! review.md P0-1: "at least one real backend implementation of
//! `VaultSigner`/`AttestationSigner`"), implementing both traits from
//! `signing::signers` over a small, provider-neutral HTTPS protocol
//! rather than a specific cloud KMS SDK — so any of the three genuinely
//! separate custody domains the approved trust model requires
//! (docs/02-trust-model.md, docs/12-management-decisions.md item 1) can
//! sit behind any vendor/HSM/KMS/homemade signer process that speaks this
//! one small protocol, without this crate depending on a specific vendor
//! SDK. See docs/26-production-signer-deployment.md for the operator-
//! facing runbook this module implements.
//!
//! # What this module is NOT
//!
//! This is a *client*. It never holds, generates, imports, or exports
//! private key material — that lives entirely in whatever process answers
//! HTTP requests at the configured endpoint (the actual custody domain:
//! an HSM, a cloud KMS proxy, a hardware signer's own small HTTP shim).
//! Per docs/22 item 10/the calling task's own requirement: HSM/KMS/
//! private-key functionality does not belong inside this daemon, and
//! nothing here adds any.
//!
//! # Wire protocol
//!
//! Two endpoints, relative to a configured `endpoint_url` base (which
//! MUST be `https://` — see [`RemoteSignerConfig::validate_scheme`]):
//!
//! - `GET {base}/v1/identity` → `200 {"public_key_hex": "<hex>"}`. Called
//!   once per signer, at construction, to cross-check the endpoint's
//!   actual public identity against the operator-configured
//!   `expected_public_key` — a mismatch fails closed and this signer is
//!   never constructed (see [`RemoteVaultSigner::connect`]/
//!   [`RemoteAttestationSigner::connect`]).
//! - `POST {base}/v1/sign`, body `{"payload_hex": "<hex of the exact
//!   bytes to sign>"}` → `200 {"signature_hex": "<hex>"}` on success, or
//!   a non-2xx status with body `{"error": "<code>", "detail": "<text>"}`
//!   on failure. The payload is always exactly what the caller (this
//!   crate's own `signing::attestation`/`signing::goldcoin_vault`
//!   independent-re-derivation logic) already computed — this client
//!   never adds, removes, or reinterprets a single byte of it.
//!
//! # Protocol version 2: Robinhood EIP-712 authorizations
//!
//! Two ADDITIONAL endpoints, on their own `/v2/` paths, serving
//! [`RemoteEvmAuthSigner`]:
//!
//! - `GET {base}/v2/evm-identity` → `200 {"address": "0x<20 bytes>"}`.
//! - `POST {base}/v2/sign-evm-auth`, body a
//!   [`crate::signing::evm_policy::EvmAuthSignRequest`] →
//!   `200 {"signature_hex": "<130 hex chars>"}`, the 65-byte compact
//!   `r || s || v` form.
//!
//! **The v1 endpoints are untouched.** A deployed signer process that
//! implements only v1 keeps serving Goldcoin and Solana signatures
//! exactly as before and simply does not answer the `/v2/` paths, which
//! this client reports as [`SignerError::Rejected`] — a fail-closed
//! outcome, never a silent downgrade to the weaker v1 request shape. That
//! is the whole reason the extension is a NEW PATH rather than a new
//! field on `/v1/sign`: a version negotiated by adding an optional field
//! is a version an old signer can ignore.
//!
//! ## Why v2 does not carry "the bytes to sign"
//!
//! Because a 32-byte EIP-712 digest cannot be understood by the domain
//! being asked to sign it. `POST /v2/sign-evm-auth` carries the
//! authorization's STRUCTURED fields; the custody domain recomputes the
//! digest from them and signs the one IT computed. The request's
//! `expected_digest` is a cross-check the domain must agree with, never
//! the value it signs. See [`crate::signing::evm_policy`], which is the
//! module a custody domain links to make that decision, and
//! [`crate::robinhood::signer`] for the trait shape on this side.
//!
//! This client's own defence in depth is unchanged in spirit: the
//! returned signature is recovered, in-process, against the digest THIS
//! side derived from the same request, and the recovered address must be
//! the identity the endpoint was configured as.
//!
//! Every request carries `Authorization: Bearer <token>`, where `<token>`
//! is read once, at process startup, from the environment variable NAMED
//! in config (`auth_token_env`) — never itself a config value, never
//! logged, never included in any `Debug`/error output (see
//! [`AuthToken`]'s own `Debug` impl).
//!
//! # Defense in depth: every returned signature is verified locally
//!
//! A remote signer's `200` response is not trusted blindly: the returned
//! signature is verified, in-process, against the exact payload that was
//! sent and the already-identity-checked expected public key, using the
//! same verification this crate already uses elsewhere
//! (`goldcoin::multisig::verify_partial` /
//! `solana_sdk::signature::Signature::verify`) — before ever being handed
//! back to a caller as `Ok`. A remote signer returning a malformed or
//! simply-wrong signature is indistinguishable, from this client's
//! perspective, from one that is compromised or buggy; both fail closed
//! as [`SignerError::Untrustworthy`], never silently accepted and never
//! routed around as if the domain had merely declined.
//!
//! # Error mapping
//!
//! Every failure mode maps into the existing three [`SignerError`]
//! variants (no new variants added — see that type's own docs, which are
//! already vendor-neutral by design):
//!
//! - Connection failure (endpoint unreachable, TLS handshake failure,
//!   DNS failure) or a `5xx` response → [`SignerError::Unavailable`]
//!   (a liveness problem, retriable next tick).
//! - A `4xx` response or a malformed/unparseable response body →
//!   [`SignerError::Rejected`] (the endpoint was reached and explicitly
//!   refused, or answered unintelligibly).
//! - A returned signature that fails LOCAL VERIFICATION →
//!   [`SignerError::Untrustworthy`], which is deliberately NOT
//!   `Rejected`: declining is something a healthy custody domain does,
//!   and answering wrong is not. See that variant's docs.
//! - The HTTP call exceeding the per-signer configured timeout →
//!   [`SignerError::Timeout`]. This is in addition to, not instead of,
//!   the generic `tokio::time::timeout` wrapper every call site already
//!   applies as defense in depth (`signing::signers` module docs) — that
//!   existing wrapper is untouched by this module.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use crate::signing::signers::{AttestationSigner, BoxFut, SignerError, VaultSigner};

/// Operator-configured shape of one remote signer endpoint — provider-
/// neutral: nothing here is specific to any cloud vendor or HSM product.
/// See `config.rs`'s `RawRemoteSigner`/`resolve()` for how this is loaded
/// from TOML plus one environment variable (never the secret itself in
/// config — see module docs).
#[derive(Debug, Clone)]
pub struct RemoteSignerConfig {
    /// MUST be `https://` — see [`RemoteSignerConfig::validate_scheme`].
    /// This is the one piece of transport security this client itself
    /// enforces; TLS certificate validation itself is reqwest's own
    /// rustls-backed default (this crate's existing dependency — see
    /// `service/Cargo.toml`'s `reqwest` feature list), not reimplemented
    /// here.
    pub endpoint_url: String,
    /// Name of the environment variable holding the bearer-token secret
    /// — never the secret itself, and never committed to git (this is a
    /// field name, read once at process startup by
    /// [`AuthToken::from_env`]).
    pub auth_token_env: String,
    pub timeout: Duration,
}

impl RemoteSignerConfig {
    /// Fails closed on any non-`https://` endpoint — a plaintext signer
    /// endpoint would defeat the entire "authenticate + encrypt every
    /// request to a genuinely separate custody domain" premise this
    /// module exists for.
    fn validate_scheme(&self) -> Result<(), RemoteSignerConfigError> {
        if !self.endpoint_url.starts_with("https://") {
            return Err(RemoteSignerConfigError::InsecureEndpoint {
                endpoint_url: self.endpoint_url.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteSignerConfigError {
    #[error(
        "remote signer endpoint {endpoint_url} is not https:// — refusing to send \
         authentication/signing traffic over an unencrypted connection"
    )]
    InsecureEndpoint { endpoint_url: String },
    #[error("environment variable {var} (named by auth_token_env) is not set")]
    AuthTokenMissing { var: String },
    #[error("environment variable {var} (named by auth_token_env) is set but empty")]
    AuthTokenEmpty { var: String },
    #[error("could not build HTTP client for {endpoint_url}: {detail}")]
    ClientBuild {
        endpoint_url: String,
        detail: String,
    },
}

/// A bearer-token secret, read once from the environment. The only
/// `Debug`/`Display` this type ever offers redacts the value — this is a
/// deliberate, structural guard against the secret ever ending up in a
/// log line, panic message, or `{:?}`-formatted error, not just a
/// convention callers are expected to follow.
#[derive(Clone)]
struct AuthToken(String);

impl AuthToken {
    fn from_env(var_name: &str) -> Result<Self, RemoteSignerConfigError> {
        let value =
            std::env::var(var_name).map_err(|_| RemoteSignerConfigError::AuthTokenMissing {
                var: var_name.to_string(),
            })?;
        if value.is_empty() {
            return Err(RemoteSignerConfigError::AuthTokenEmpty {
                var: var_name.to_string(),
            });
        }
        Ok(AuthToken(value))
    }

    fn header_value(&self) -> String {
        format!("Bearer {}", self.0)
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

#[derive(Debug, Serialize)]
struct SignRequest<'a> {
    payload_hex: &'a str,
}

#[derive(Debug, Deserialize)]
struct SignResponse {
    signature_hex: String,
}

#[derive(Debug, Deserialize)]
struct IdentityResponse {
    public_key_hex: String,
}

/// `GET /v2/evm-identity`'s body. A separate shape from
/// [`IdentityResponse`] on purpose: an EVM authorization signer is
/// identified by a 20-byte ADDRESS, and reusing `public_key_hex` would
/// invite a domain to answer with a 33-byte compressed key that this
/// client would then have to hash — silently accepting an answer to a
/// different question.
#[derive(Debug, Deserialize)]
struct EvmIdentityResponse {
    address: String,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    detail: String,
}

/// Strict upper bound on any single response body this client will ever
/// read from a signer endpoint, enforced by
/// `RemoteSignerClient::read_bounded_body` — checked against
/// `Content-Length` where declared, and against the running total of
/// chunks actually read either way (a declared `Content-Length` is never
/// assumed trustworthy on its own). Every response this protocol
/// legitimately produces is tiny: `{"public_key_hex": "<64 or 66 hex
/// chars>"}` is well under 100 bytes; `{"signature_hex": "<hex>"}` is at
/// most a few hundred bytes even for the largest DER-encoded secp256k1
/// signature; an error body (`{"error": ..., "detail": ...}`) is
/// operator-written prose, not expected to be large either. 4 KiB is
/// generous headroom over any of those while still bounding, by a wide
/// margin, how much memory a compromised or misbehaving endpoint can
/// force this client to buffer.
const MAX_RESPONSE_BODY_BYTES: usize = 4096;

/// Shared HTTP mechanics both `RemoteVaultSigner` and
/// `RemoteAttestationSigner` are thin wrappers around — identical
/// protocol, different payload/signature encodings and different local
/// verification (secp256k1 DER vs. ed25519).
#[derive(Debug)]
struct RemoteSignerClient {
    http: reqwest::Client,
    base_url: String,
    auth: AuthToken,
    timeout: Duration,
    /// Used only in error/log messages — a public identifier, never
    /// secret (matches `SignerError`'s own `identity` field convention).
    identity_label: String,
}

impl RemoteSignerClient {
    async fn connect(
        config: &RemoteSignerConfig,
        identity_label: String,
    ) -> Result<(Self, Vec<u8>), RemoteSignerConfigError> {
        config.validate_scheme()?;
        Self::connect_unchecked(config, identity_label).await
    }

    /// Everything `connect` does except the `https://`-only scheme
    /// enforcement. Only ever called by `connect` itself (which checks
    /// the scheme first) and, in `#[cfg(test)]` builds, directly by the
    /// test suite against a real local plain-HTTP test server — see
    /// `remote::tests` module docs for why testing the wire protocol and
    /// error mapping this way (real transport, not a mocked one) doesn't
    /// require standing up a TLS certificate fixture, and why that is a
    /// deliberately separate concern from the scheme enforcement itself
    /// (which has its own direct, no-server-needed test).
    async fn connect_unchecked(
        config: &RemoteSignerConfig,
        identity_label: String,
    ) -> Result<(Self, Vec<u8>), RemoteSignerConfigError> {
        let auth = AuthToken::from_env(&config.auth_token_env)?;
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            // This protocol has no legitimate reason to ever redirect —
            // disabling redirects entirely means a 3xx response always
            // surfaces as a hard, visible error (`!status().is_success()`
            // below) instead of being silently followed. Without this,
            // reqwest's default policy follows up to 10 redirects; it
            // does strip Authorization on a cross-host-or-port redirect,
            // but forwards it on a same-origin one, and either way the
            // daemon would be sending a real signing payload to a
            // destination the operator never configured — a classic SSRF
            // pattern for a "genuinely separate custody domain" threat
            // model that already assumes one domain could be
            // compromised. Verified against reqwest 0.12's own source
            // (`redirect.rs`) during the security review that flagged
            // this.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| RemoteSignerConfigError::ClientBuild {
                endpoint_url: config.endpoint_url.clone(),
                detail: e.to_string(),
            })?;
        let client = RemoteSignerClient {
            http,
            base_url: config.endpoint_url.trim_end_matches('/').to_string(),
            auth,
            timeout: config.timeout,
            identity_label,
        };
        let identity_bytes =
            client
                .fetch_identity()
                .await
                .map_err(|e| RemoteSignerConfigError::ClientBuild {
                    endpoint_url: client.base_url.clone(),
                    detail: format!("identity fetch failed: {e}"),
                })?;
        Ok((client, identity_bytes))
    }

    /// `GET {base}/v1/identity` — called once, at construction, never
    /// again (the signer's identity is fixed for the process lifetime;
    /// a rotation is a new `custody_transitions` cycle and a new
    /// deployment, per docs/09-runbook.md, not something this client
    /// re-checks live).
    async fn fetch_identity(&self) -> Result<Vec<u8>, SignerError> {
        let url = format!("{}/v1/identity", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("authorization", self.auth.header_value())
            .send()
            .await
            .map_err(|e| self.map_reqwest_error(e))?;
        if !resp.status().is_success() {
            return Err(self.map_error_status(resp).await);
        }
        let bytes = self.read_bounded_body(resp).await?;
        let body: IdentityResponse =
            serde_json::from_slice(&bytes).map_err(|e| SignerError::Rejected {
                identity: self.identity_label.clone(),
                detail: format!("malformed identity response: {e}"),
            })?;
        crate::goldcoin::hex::decode_vec(&body.public_key_hex).map_err(|e| SignerError::Rejected {
            identity: self.identity_label.clone(),
            detail: format!("identity response public_key_hex is not valid hex: {e}"),
        })
    }

    /// `GET {base}/v2/evm-identity` — the EVM authorization signer's
    /// 20-byte address. Called once, at construction, for the same reason
    /// and with the same finality as [`RemoteSignerClient::fetch_identity`].
    async fn fetch_evm_identity(&self) -> Result<crate::evm::EvmAddress, SignerError> {
        let url = format!("{}/v2/evm-identity", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("authorization", self.auth.header_value())
            .send()
            .await
            .map_err(|e| self.map_reqwest_error(e))?;
        if !resp.status().is_success() {
            return Err(self.map_error_status(resp).await);
        }
        let bytes = self.read_bounded_body(resp).await?;
        let body: EvmIdentityResponse =
            serde_json::from_slice(&bytes).map_err(|e| SignerError::Rejected {
                identity: self.identity_label.clone(),
                detail: format!("malformed evm-identity response: {e}"),
            })?;
        body.address.parse().map_err(|e| SignerError::Rejected {
            identity: self.identity_label.clone(),
            detail: format!("evm-identity response address is not an EVM address: {e}"),
        })
    }

    /// `POST {base}/v2/sign-evm-auth`.
    ///
    /// Sends the AUTHORIZATION, not bytes. Returns the raw signature the
    /// endpoint replied with; the caller decodes and verifies it against
    /// its own independently derived digest before trusting it — see
    /// [`RemoteEvmAuthSigner::sign_authorization`].
    async fn sign_evm_auth(
        &self,
        document: &crate::signing::evm_policy::EvmAuthSignRequest,
    ) -> Result<Vec<u8>, SignerError> {
        let url = format!("{}/v2/sign-evm-auth", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("authorization", self.auth.header_value())
            .json(document)
            .send()
            .await
            .map_err(|e| self.map_reqwest_error(e))?;
        if !resp.status().is_success() {
            return Err(self.map_error_status(resp).await);
        }
        let bytes = self.read_bounded_body(resp).await?;
        let body: SignResponse =
            serde_json::from_slice(&bytes).map_err(|e| SignerError::Rejected {
                identity: self.identity_label.clone(),
                detail: format!("malformed sign-evm-auth response: {e}"),
            })?;
        crate::goldcoin::hex::decode_vec(&body.signature_hex).map_err(|e| SignerError::Rejected {
            identity: self.identity_label.clone(),
            detail: format!("sign-evm-auth response signature_hex is not valid hex: {e}"),
        })
    }

    /// `POST {base}/v1/sign`. Returns the raw signature bytes exactly as
    /// the remote signer returned them — callers are responsible for
    /// decoding into their own signature type and verifying locally
    /// before trusting it (see module docs).
    async fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, SignerError> {
        let url = format!("{}/v1/sign", self.base_url);
        let payload_hex = crate::goldcoin::hex::encode(payload);
        let resp = self
            .http
            .post(&url)
            .header("authorization", self.auth.header_value())
            .json(&SignRequest {
                payload_hex: &payload_hex,
            })
            .send()
            .await
            .map_err(|e| self.map_reqwest_error(e))?;
        if !resp.status().is_success() {
            return Err(self.map_error_status(resp).await);
        }
        let bytes = self.read_bounded_body(resp).await?;
        let body: SignResponse =
            serde_json::from_slice(&bytes).map_err(|e| SignerError::Rejected {
                identity: self.identity_label.clone(),
                detail: format!("malformed sign response: {e}"),
            })?;
        crate::goldcoin::hex::decode_vec(&body.signature_hex).map_err(|e| SignerError::Rejected {
            identity: self.identity_label.clone(),
            detail: format!("sign response signature_hex is not valid hex: {e}"),
        })
    }

    /// Reads `resp`'s body up to [`MAX_RESPONSE_BODY_BYTES`], never
    /// buffering more than that regardless of what the endpoint claims or
    /// sends — every response this protocol ever legitimately produces is
    /// a few dozen to a few hundred bytes of JSON (see that constant's
    /// own docs), so this is a strict, documented, fail-closed bound
    /// against a compromised or misbehaving signer forcing unbounded
    /// memory use.
    ///
    /// Two layers, both enforced: a declared `Content-Length` over the
    /// limit is rejected immediately, before reading any body at all; and
    /// — since `Content-Length` can be absent (chunked transfer) or
    /// simply wrong — every chunk actually read is counted as it arrives,
    /// aborting the instant the running total exceeds the limit rather
    /// than accumulating an oversized body first and checking after.
    async fn read_bounded_body(&self, resp: reqwest::Response) -> Result<Vec<u8>, SignerError> {
        if let Some(len) = resp.content_length() {
            if len > MAX_RESPONSE_BODY_BYTES as u64 {
                return Err(SignerError::Rejected {
                    identity: self.identity_label.clone(),
                    detail: format!(
                        "response Content-Length ({len} bytes) exceeds the \
                         {MAX_RESPONSE_BODY_BYTES}-byte limit for this protocol's responses"
                    ),
                });
            }
        }
        let mut resp = resp;
        let mut buf = Vec::new();
        loop {
            let chunk = resp.chunk().await.map_err(|e| SignerError::Rejected {
                identity: self.identity_label.clone(),
                detail: format!("error reading response body: {e}"),
            })?;
            let Some(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk);
            if buf.len() > MAX_RESPONSE_BODY_BYTES {
                return Err(SignerError::Rejected {
                    identity: self.identity_label.clone(),
                    detail: format!(
                        "response body exceeded the {MAX_RESPONSE_BODY_BYTES}-byte limit for \
                         this protocol's responses (no Content-Length was declared, or it \
                         under-reported the actual size)"
                    ),
                });
            }
        }
        Ok(buf)
    }

    /// A connection-level failure (unreachable, TLS failure, DNS
    /// failure) or the reqwest client's own timeout firing — both map to
    /// `Unavailable`/`Timeout` respectively, never `Rejected` (the
    /// endpoint was never actually reached to "reject" anything).
    fn map_reqwest_error(&self, e: reqwest::Error) -> SignerError {
        if e.is_timeout() {
            SignerError::Timeout {
                identity: self.identity_label.clone(),
                millis: self.timeout.as_millis() as u64,
            }
        } else {
            SignerError::Unavailable {
                identity: self.identity_label.clone(),
                detail: e.to_string(),
            }
        }
    }

    /// A non-2xx response the endpoint was actually reached to produce.
    /// `5xx` is treated as a liveness problem on the signer's own side
    /// (`Unavailable`, retriable); `4xx` is treated as an explicit
    /// refusal of this specific request (`Rejected`) — matching
    /// `SignerError`'s own documented distinction.
    async fn map_error_status(&self, resp: reqwest::Response) -> SignerError {
        let status = resp.status();
        // The error-response body is subject to the exact same bounded
        // read as a successful response — a non-2xx status is not an
        // exemption from the size limit; an oversized error body still
        // fails closed, using the classification (Unavailable/Rejected)
        // the real HTTP status code already determined, not a generic
        // one.
        let detail = match self.read_bounded_body(resp).await {
            Ok(bytes) => match serde_json::from_slice::<ErrorResponse>(&bytes) {
                Ok(body) if !body.detail.is_empty() => format!("{}: {}", body.error, body.detail),
                Ok(body) if !body.error.is_empty() => body.error,
                _ => format!("HTTP {status}"),
            },
            Err(_) => format!("HTTP {status} (response body exceeded the size limit)"),
        };
        if status.is_server_error() {
            SignerError::Unavailable {
                identity: self.identity_label.clone(),
                detail,
            }
        } else {
            SignerError::Rejected {
                identity: self.identity_label.clone(),
                detail,
            }
        }
    }
}

/// A production Goldcoin vault signer reached over HTTPS — one of the
/// three genuinely separate custody domains the approved trust model
/// requires (docs/02-trust-model.md). See module docs for the wire
/// protocol and error mapping.
#[derive(Debug)]
pub struct RemoteVaultSigner {
    client: RemoteSignerClient,
    public_key: [u8; 33],
}

impl RemoteVaultSigner {
    /// Connects to `config`, fetches the endpoint's identity, and fails
    /// closed if it does not exactly match `expected_public_key` — the
    /// same discipline `Config::load_vault_signers`'s dev-file loader
    /// already applies to a local key file, applied here to a remote
    /// endpoint's own self-reported identity instead. Enforces
    /// `https://` — see `RemoteSignerConfig::validate_scheme`.
    pub async fn connect(
        config: &RemoteSignerConfig,
        expected_public_key: [u8; 33],
    ) -> Result<Self, RemoteSignerConfigError> {
        let identity_label = crate::goldcoin::hex::encode(&expected_public_key);
        let connected = RemoteSignerClient::connect(config, identity_label).await?;
        Self::finish_connect(connected, expected_public_key)
    }

    /// Test-only: identical to `connect` except it does not require
    /// `https://` — see `RemoteSignerClient::connect_unchecked`'s docs
    /// for why.
    #[cfg(test)]
    async fn connect_for_tests(
        config: &RemoteSignerConfig,
        expected_public_key: [u8; 33],
    ) -> Result<Self, RemoteSignerConfigError> {
        let identity_label = crate::goldcoin::hex::encode(&expected_public_key);
        let connected = RemoteSignerClient::connect_unchecked(config, identity_label).await?;
        Self::finish_connect(connected, expected_public_key)
    }

    fn finish_connect(
        (client, identity_bytes): (RemoteSignerClient, Vec<u8>),
        expected_public_key: [u8; 33],
    ) -> Result<Self, RemoteSignerConfigError> {
        let actual: [u8; 33] = identity_bytes.as_slice().try_into().map_err(|_| {
            RemoteSignerConfigError::ClientBuild {
                endpoint_url: client.base_url.clone(),
                detail: format!(
                    "identity response returned {} bytes, expected 33 (compressed \
                         secp256k1 public key)",
                    identity_bytes.len()
                ),
            }
        })?;
        if actual != expected_public_key {
            return Err(RemoteSignerConfigError::ClientBuild {
                endpoint_url: client.base_url.clone(),
                detail: format!(
                    "endpoint identity {} does not match configured expected_public_key {} — \
                     refusing to use this signer",
                    crate::goldcoin::hex::encode(&actual),
                    crate::goldcoin::hex::encode(&expected_public_key)
                ),
            });
        }
        Ok(RemoteVaultSigner {
            client,
            public_key: expected_public_key,
        })
    }
}

impl VaultSigner for RemoteVaultSigner {
    fn public_key(&self) -> [u8; 33] {
        self.public_key
    }

    fn sign_sighash<'a>(
        &'a self,
        sighash: &'a [u8; 32],
    ) -> BoxFut<'a, Result<Vec<u8>, SignerError>> {
        Box::pin(async move {
            let der = self.client.sign(sighash).await?;
            if !crate::goldcoin::multisig::verify_partial(&self.public_key, sighash, &der) {
                return Err(SignerError::Untrustworthy {
                    identity: self.client.identity_label.clone(),
                    detail: "remote signer returned a signature that fails local verification \
                              against the expected public key and payload"
                        .to_string(),
                });
            }
            Ok(der)
        })
    }
}

/// A production Solana attestation signer reached over HTTPS — one of
/// the three genuinely separate custody domains the approved trust model
/// requires (docs/02-trust-model.md). See module docs for the wire
/// protocol and error mapping.
#[derive(Debug)]
pub struct RemoteAttestationSigner {
    client: RemoteSignerClient,
    pubkey: Pubkey,
}

impl RemoteAttestationSigner {
    /// Connects to `config`, fetches the endpoint's identity, and fails
    /// closed if it does not exactly match `expected_pubkey`. Enforces
    /// `https://` — see `RemoteSignerConfig::validate_scheme`.
    pub async fn connect(
        config: &RemoteSignerConfig,
        expected_pubkey: Pubkey,
    ) -> Result<Self, RemoteSignerConfigError> {
        let identity_label = expected_pubkey.to_string();
        let connected = RemoteSignerClient::connect(config, identity_label).await?;
        Self::finish_connect(connected, expected_pubkey)
    }

    /// Test-only: identical to `connect` except it does not require
    /// `https://` — see `RemoteSignerClient::connect_unchecked`'s docs
    /// for why.
    #[cfg(test)]
    async fn connect_for_tests(
        config: &RemoteSignerConfig,
        expected_pubkey: Pubkey,
    ) -> Result<Self, RemoteSignerConfigError> {
        let identity_label = expected_pubkey.to_string();
        let connected = RemoteSignerClient::connect_unchecked(config, identity_label).await?;
        Self::finish_connect(connected, expected_pubkey)
    }

    fn finish_connect(
        (client, identity_bytes): (RemoteSignerClient, Vec<u8>),
        expected_pubkey: Pubkey,
    ) -> Result<Self, RemoteSignerConfigError> {
        let actual_bytes: [u8; 32] = identity_bytes.as_slice().try_into().map_err(|_| {
            RemoteSignerConfigError::ClientBuild {
                endpoint_url: client.base_url.clone(),
                detail: format!(
                    "identity response returned {} bytes, expected 32 (ed25519 public key)",
                    identity_bytes.len()
                ),
            }
        })?;
        let actual = Pubkey::new_from_array(actual_bytes);
        if actual != expected_pubkey {
            return Err(RemoteSignerConfigError::ClientBuild {
                endpoint_url: client.base_url.clone(),
                detail: format!(
                    "endpoint identity {actual} does not match configured expected_public_key \
                     {expected_pubkey} — refusing to use this signer"
                ),
            });
        }
        Ok(RemoteAttestationSigner {
            client,
            pubkey: expected_pubkey,
        })
    }
}

impl AttestationSigner for RemoteAttestationSigner {
    fn pubkey(&self) -> Pubkey {
        self.pubkey
    }

    fn sign_message<'a>(&'a self, message: &'a [u8]) -> BoxFut<'a, Result<Signature, SignerError>> {
        Box::pin(async move {
            let raw = self.client.sign(message).await?;
            let sig_bytes: [u8; 64] =
                raw.as_slice()
                    .try_into()
                    .map_err(|_| SignerError::Rejected {
                        identity: self.client.identity_label.clone(),
                        detail: format!(
                            "remote signer returned {} signature bytes, expected 64 (ed25519)",
                            raw.len()
                        ),
                    })?;
            let signature = Signature::from(sig_bytes);
            if !signature.verify(self.pubkey.as_ref(), message) {
                return Err(SignerError::Untrustworthy {
                    identity: self.client.identity_label.clone(),
                    detail: "remote signer returned a signature that fails local verification \
                              against the expected public key and payload"
                        .to_string(),
                });
            }
            Ok(signature)
        })
    }
}

/// A production Robinhood EIP-712 authorization signer reached over
/// HTTPS — one of the three genuinely separate custody domains whose
/// 2-of-3 quorum `GlcRobinhoodBridge` verifies.
///
/// # This is what closes the Phase F launch blocker
///
/// `docs/32-robinhood-settlement-phase-f.md` §13.A records that
/// production mode had no Robinhood authorization signers at all, could
/// not assemble a quorum, and therefore could not broadcast. That was the
/// correct fail-closed outcome for a protocol that did not yet exist.
/// This type is that protocol's client half.
///
/// # It cannot be asked to sign bytes
///
/// [`crate::robinhood::signer::EvmAuthSigner`] has one signing method and
/// it takes an [`EvmAuthRequest`]. This implementation serialises that
/// request into the v2 document and sends it; there is no code path here
/// that puts a caller-supplied digest on the wire as the thing to sign.
/// The custody domain on the other end runs
/// [`crate::signing::evm_policy::EvmSignerPolicy::evaluate`], derives the
/// digest itself, and signs that.
///
/// # Three independent checks on every answer
///
/// 1. The signature decodes as a 65-byte compact secp256k1 signature.
/// 2. It recovers, against the digest THIS side derived from the same
///    request, to an address.
/// 3. That address is the `expected_address` this signer was configured
///    with and whose identity endpoint confirmed it.
///
/// A domain that signed a different authorization fails (2): the digest
/// it signed is not the digest recovered against, so the recovered
/// address is not its own. That is why a disagreement between the two
/// sides can only ever produce a refusal, never a usable signature over
/// the wrong thing.
#[derive(Debug)]
pub struct RemoteEvmAuthSigner {
    client: RemoteSignerClient,
    address: crate::evm::EvmAddress,
}

impl RemoteEvmAuthSigner {
    /// Connects to `config`, fetches the endpoint's EVM identity, and
    /// fails closed if it does not exactly match `expected_address`.
    /// Enforces `https://` — see [`RemoteSignerConfig::validate_scheme`].
    pub async fn connect(
        config: &RemoteSignerConfig,
        expected_address: crate::evm::EvmAddress,
    ) -> Result<RemoteEvmAuthSigner, RemoteSignerConfigError> {
        config.validate_scheme()?;
        Self::connect_inner(config, expected_address).await
    }

    /// Test-only: identical to `connect` except it does not require
    /// `https://` — see [`RemoteSignerClient::connect_unchecked`]'s docs
    /// for why.
    #[cfg(test)]
    async fn connect_for_tests(
        config: &RemoteSignerConfig,
        expected_address: crate::evm::EvmAddress,
    ) -> Result<RemoteEvmAuthSigner, RemoteSignerConfigError> {
        Self::connect_inner(config, expected_address).await
    }

    async fn connect_inner(
        config: &RemoteSignerConfig,
        expected_address: crate::evm::EvmAddress,
    ) -> Result<RemoteEvmAuthSigner, RemoteSignerConfigError> {
        let identity_label = expected_address.to_checksum_string();
        // The v1 identity endpoint is NOT called: this domain's identity
        // is an address, and asking `/v1/identity` would either fail or,
        // worse, succeed against a signer serving a different curve
        // convention. Connecting therefore also proves the endpoint
        // speaks v2 at all, before any authorization is built.
        let auth = AuthToken::from_env(&config.auth_token_env)?;
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| RemoteSignerConfigError::ClientBuild {
                endpoint_url: config.endpoint_url.clone(),
                detail: e.to_string(),
            })?;
        let client = RemoteSignerClient {
            http,
            base_url: config.endpoint_url.trim_end_matches('/').to_string(),
            auth,
            timeout: config.timeout,
            identity_label,
        };
        let actual = client.fetch_evm_identity().await.map_err(|e| {
            RemoteSignerConfigError::ClientBuild {
                endpoint_url: client.base_url.clone(),
                detail: format!("evm-identity fetch failed: {e}"),
            }
        })?;
        if actual != expected_address {
            return Err(RemoteSignerConfigError::ClientBuild {
                endpoint_url: client.base_url.clone(),
                detail: format!(
                    "endpoint identity {} does not match configured expected_address {} — \
                     refusing to use this signer",
                    actual.to_checksum_string(),
                    expected_address.to_checksum_string()
                ),
            });
        }
        Ok(RemoteEvmAuthSigner {
            client,
            address: expected_address,
        })
    }
}

impl crate::robinhood::signer::EvmAuthSigner for RemoteEvmAuthSigner {
    fn address(&self) -> crate::evm::EvmAddress {
        self.address
    }

    fn sign_authorization<'a>(
        &'a self,
        request: &'a crate::robinhood::auth::EvmAuthRequest,
    ) -> crate::robinhood::signer::BoxFut<'a, Result<crate::evm::EvmSignature, SignerError>> {
        Box::pin(async move {
            // Derived HERE, from the request, by the same encoder the
            // custody domain will use. Not taken from anywhere else, and
            // not sent as the thing to sign.
            let digest = request.digest().map_err(|e| SignerError::Rejected {
                identity: self.client.identity_label.clone(),
                detail: format!("the authorization could not be encoded: {e}"),
            })?;
            let document = crate::signing::evm_policy::EvmAuthSignRequest::from_request(request)
                .map_err(|e| SignerError::Rejected {
                    identity: self.client.identity_label.clone(),
                    detail: format!("the authorization could not be encoded: {e}"),
                })?;
            let raw = self.client.sign_evm_auth(&document).await?;
            let signature = crate::evm::EvmSignature::try_from_slice(&raw).map_err(|e| {
                SignerError::Rejected {
                    identity: self.client.identity_label.clone(),
                    detail: format!(
                        "remote signer returned {} signature bytes that are not a compact \
                         secp256k1 signature: {e}",
                        raw.len()
                    ),
                }
            })?;
            let recovered =
                crate::evm::secp::recover_address(&digest, &signature).map_err(|e| {
                    SignerError::Untrustworthy {
                        identity: self.client.identity_label.clone(),
                        detail: format!(
                            "remote signer returned a signature that does not recover against the \
                         digest this side derived from the same authorization: {e}"
                        ),
                    }
                })?;
            if recovered != self.address {
                return Err(SignerError::Untrustworthy {
                    identity: self.client.identity_label.clone(),
                    detail: format!(
                        "remote signer's signature recovers to {} over this authorization, not \
                         the configured identity {} — the endpoint signed something else, or is \
                         not the domain it claims to be",
                        recovered.to_checksum_string(),
                        self.address.to_checksum_string()
                    ),
                });
            }
            Ok(signature)
        })
    }
}

#[cfg(test)]
mod tests;
