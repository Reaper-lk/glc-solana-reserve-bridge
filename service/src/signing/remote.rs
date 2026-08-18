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
//! the same way (`SignerError::Rejected`), never silently accepted.
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
//! - A `4xx` response, a malformed/unparseable response body, or a
//!   returned signature that fails local verification →
//!   [`SignerError::Rejected`] (the endpoint was reached, but its
//!   response cannot be trusted or was an explicit refusal).
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
                return Err(SignerError::Rejected {
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
                return Err(SignerError::Rejected {
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

#[cfg(test)]
mod tests;
