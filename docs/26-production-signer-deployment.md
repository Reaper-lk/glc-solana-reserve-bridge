# Production Signer Deployment Runbook

Operator-facing companion to `service/src/signing/remote.rs` (docs/22-production-readiness-review.md item 10/P0-1, docs/12-management-decisions.md item 2). Covers: the wire protocol a custody domain must implement, the production config schema, a per-domain deployment checklist, and troubleshooting. Read docs/02-trust-model.md first if you haven't — this document assumes the approved 2-of-3 threshold-custody model and does not re-argue it.

## What this is, in one paragraph

`glc-bridge-daemon` never holds a production signing key. In production mode (`operators.mode = "production"`), it instead makes small, authenticated HTTPS calls to up to three independent endpoints per signer group (three attestation signers, three Goldcoin vault signers) — each endpoint is a genuinely separate custody domain's own responsibility to run, and each speaks the same small protocol below regardless of what's behind it (a cloud KMS, a hardware HSM, a bespoke signer process). The daemon verifies every returned signature locally before trusting it, and refuses to start at all if it's misconfigured to mix production and dev/test signing.

## The wire protocol your custody domain must implement

Two endpoints, both required, relative to your `endpoint_url`. Nothing else is needed — this daemon never calls anything else on your signer.

### `GET {endpoint_url}/v1/identity`

Called once, when the daemon starts. Must return your signer's real public key, unconditionally — the daemon cross-checks this against the `expected_public_key` in its own config and **refuses to start** if they don't match. Get this endpoint right before anything else; every other call depends on identity having already checked out.

Request: no body, but still carries the `Authorization: Bearer <token>` header (see [Authentication](#authentication) below) — authenticate this call the same as `/v1/sign`.

Response, `200`:
```json
{"public_key_hex": "<hex>"}
```
- Attestation signers: 32 bytes (Solana ed25519 public key), hex-encoded (64 hex characters).
- Goldcoin vault signers: 33 bytes (compressed secp256k1 public key), hex-encoded (66 hex characters).

### `POST {endpoint_url}/v1/sign`

Called once per settlement signature. The daemon has already independently re-derived and fully assembled the exact bytes to sign before this call — your signer never receives a "request ID" or anything to look up; it receives the final bytes and nothing else. **Sign exactly what you're given, nothing more** — do not reinterpret, re-derive, or substitute a different payload, even if you have your own view of what "should" be signed. Independent re-derivation is this bridge's job, done before this call; your job is only the cryptographic operation.

Request:
```json
{"payload_hex": "<hex of the exact bytes to sign>"}
```
- Attestation signers: an ed25519 message — Ed25519 sign it directly, no additional hashing (the payload is already the final canonical message).
- Vault signers: exactly 32 bytes — a secp256k1 ECDSA signature over it, DER-encoded (no sighash-type byte appended; the daemon appends that itself).

Response, `200`, on success:
```json
{"signature_hex": "<hex>"}
```
- Attestation: 64 bytes (raw ed25519 signature), hex-encoded.
- Vault: a DER-encoded ECDSA signature, hex-encoded (variable length, typically ~70-72 bytes).

Response, non-`200`, on any refusal:
```json
{"error": "<short code>", "detail": "<human-readable reason>"}
```
Use a `4xx` status for an explicit policy refusal (a `5xx` is treated as your signer being temporarily unavailable and retried later; a `4xx` is treated as a considered "no" for this specific request). `error`/`detail` are surfaced in the daemon's own logs and error messages — put something useful there, but never put a secret in it.

#### Error mapping and exact failure behavior

Every way a response can be wrong maps to a `SignerError`, fail-closed, with no retry inside the signer client itself:

| What happened | Result |
|---|---|
| `2xx` with a well-formed body, but the returned signature fails local verification against `expected_public_key` | `SignerError::Rejected` — the daemon never trusts a signature it can't itself verify, even from an endpoint that passed identity pinning |
| `2xx` but the body isn't valid JSON, or is missing/mistyped the expected field | `SignerError::Rejected` (malformed response) |
| Any `3xx` (redirect) | Not followed — treated as a plain non-`2xx` status, same as a `4xx` (see above); `SignerError::Rejected` |
| `4xx` | `SignerError::Rejected` |
| `5xx`, connection refused/reset, DNS failure, TLS handshake failure | `SignerError::Unavailable` |
| Response exceeds 4096 bytes (`Content-Length` too large, or the streamed body crosses the bound) | `SignerError::Rejected` — the body is never fully buffered or parsed; for a non-`2xx` status, the detail falls back to `"HTTP {status} (response body exceeded the size limit)"` rather than including the truncated/oversized body |
| No response within `timeout_ms` (or the daemon's own `service.signer_timeout_ms`, whichever is tighter) | `SignerError::Timeout` |
| `/v1/identity` at connect time doesn't match `expected_public_key` | Daemon refuses to start (`ConfigError::RemoteSignerConnect`) — this is the only one of these checks that happens once, at startup, rather than per-signing-call |

### Authentication

Every request (`/v1/identity` and `/v1/sign` both) carries:
```
Authorization: Bearer <token>
```
`<token>` comes from the environment variable named by your `auth_token_env` config entry — **never itself a config value, never committed to git**. Your endpoint should reject any request with a missing or incorrect bearer token with `401`. How you provision that token to both sides (the daemon's environment and your signer's own auth check) is your custody domain's own operational concern — a secrets manager injecting it into both processes' environments at start is a reasonable default; this daemon does not care how you do it, only that the same shared secret authenticates both sides.

### TLS

Your `endpoint_url` **must** be `https://` — the daemon refuses to construct a signer against anything else, at config-resolution time, before any network call happens. Certificate validation is the daemon's normal TLS stack (rustls with the platform's native root store) — a self-signed certificate will not validate; use a certificate from a trusted CA (including an internal CA whose root the daemon's host trusts).

### Redirects are forbidden

The daemon's HTTP client is configured with `redirect::Policy::none()` — it never follows a `3xx` response, on either `/v1/identity` or `/v1/sign`. If your endpoint (or something in front of it — a load balancer, a proxy) returns a redirect, the daemon treats the unfollowed `3xx` exactly like any other non-`2xx` response (see [Error mapping](#error-mapping-and-exact-failure-behavior) below) — it does **not** transparently follow it, and it never forwards the `Authorization` header to whatever host the `Location` pointed at. This is deliberate: a same-origin redirect would still carry the bearer token under reqwest's own default policy, and a cross-origin redirect is exactly the shape of an SSRF vector. Point `endpoint_url` at the final URL your signer actually serves — a redirect chain in front of it will not work.

### Maximum response size

Both endpoints must return small, tiny JSON bodies (a single hex string, or a short error object) — the daemon enforces a hard **4096-byte** cap (`MAX_RESPONSE_BODY_BYTES` in `service/src/signing/remote.rs`) on every response body, whichever endpoint it came from. This is checked against `Content-Length` immediately if the header is present (rejected before any body bytes are read at all), and enforced while streaming if `Content-Length` is absent or understated (e.g. `Transfer-Encoding: chunked`) — the daemon never buffers an unbounded body waiting to see how large it gets. An oversized response is treated as a fail-closed error, not truncated and parsed; if your signer's error responses ever grow verbose `detail` text, keep it well under this bound.

### Duplicate signer identities are forbidden

Production config validation rejects, at config-resolution time (before any network call): two entries in the same signer group (`attestation_remote_signers` or `vault_remote_signers`) sharing the same `expected_public_key`, and two entries in the same group sharing the same `endpoint_url` — even if their keys differ. Both are refused for the same reason: this deployment's 2-of-3 threshold assumes three *genuinely separate* custody domains (docs/02-trust-model.md, docs/12-management-decisions.md item 2). A duplicated key means fewer real domains exist than the config claims; a duplicated endpoint means two "domains" share a single network/operational compromise blast radius regardless of what keys they report. Use three distinct endpoints with three distinct keys per signer group.

## Config schema

Two arrays, one per signer group, each entry independently specifying `endpoint_url`, `expected_public_key`, `auth_token_env`, and `timeout_ms`:

```toml
[operators]
mode = "production"                 # gates the entire signer-loading path — see below
admin_pubkey = "..."
attestation_threshold = 2           # unchanged by mode — same threshold semantics either way
attestation_pubkeys = ["<pk1>", "<pk2>", "<pk3>"]
# attestation_key_paths intentionally absent/empty in production mode —
# see "Fail-closed mode gating" below.
attestation_remote_signers = [
  { endpoint_url = "https://attest-domain-a.example.com", expected_public_key = "<pk1>", auth_token_env = "GLC_BRIDGE_ATTEST_1_TOKEN", timeout_ms = 5000 },
  { endpoint_url = "https://attest-domain-b.example.com", expected_public_key = "<pk2>", auth_token_env = "GLC_BRIDGE_ATTEST_2_TOKEN", timeout_ms = 5000 },
  { endpoint_url = "https://attest-domain-c.example.com", expected_public_key = "<pk3>", auth_token_env = "GLC_BRIDGE_ATTEST_3_TOKEN", timeout_ms = 5000 },
]

vault_threshold = 2
vault_pubkeys = ["<vk1>", "<vk2>", "<vk3>"]
vault_remote_signers = [
  { endpoint_url = "https://vault-domain-a.example.com", expected_public_key = "<vk1>", auth_token_env = "GLC_BRIDGE_VAULT_1_TOKEN", timeout_ms = 5000 },
  { endpoint_url = "https://vault-domain-b.example.com", expected_public_key = "<vk2>", auth_token_env = "GLC_BRIDGE_VAULT_2_TOKEN", timeout_ms = 5000 },
  { endpoint_url = "https://vault-domain-c.example.com", expected_public_key = "<vk3>", auth_token_env = "GLC_BRIDGE_VAULT_3_TOKEN", timeout_ms = 5000 },
]
submitter_key_path = "..."           # unaffected by mode — see note below
```

`expected_public_key` is deliberately redundant with the positionally-matching entry in `attestation_pubkeys`/`vault_pubkeys` — the two are cross-checked against each other at config-load time (`ConfigError::RemoteSignerExpectedKeyMismatch` if they disagree), and each is independently checked against your endpoint's own `/v1/identity` response at connect time. A copy/paste error between the two lists is caught before the daemon ever makes a network call, not discovered later against a live endpoint.

`timeout_ms` bounds the HTTP call itself; the daemon also applies its own separate `service.signer_timeout_ms` as defense in depth around the whole signing operation (unchanged from dev mode — see `signing::signers` module docs).

`submitter_key_path` (the Solana transaction fee-payer keypair) is **not** part of this system and is unaffected by `operators.mode` — it is explicitly not a custody authority (see `Config::load_submitter`'s own docs: "nothing else derives trust from which key this is"). It stays a local plaintext key file in both dev and production mode today; treat it as a lower-stakes secret (losing it only lets someone pay transaction fees on the bridge's behalf, not authorize settlements) but still don't commit it to git.

## Fail-closed mode gating

`operators.mode` is one of two values, and the daemon refuses to start (`ConfigError`, exit 2) if the populated fields disagree with it:

| `mode` | `*_key_paths` | `*_remote_signers` |
|---|---|---|
| `"dev"` | required, 1:1 with pubkeys | must be empty |
| `"production"` | must be empty | required, 1:1 with pubkeys |

There is no third, "mixed" mode. A deployment is either entirely dev/test-posture (local plaintext keys, matching `DevVaultSigner`/`DevAttestationSigner`) or entirely production-posture (remote signers only) — this is a deliberate simplification so "is this a real deployment" is answerable by reading one field, not by auditing every signer entry individually.

## Per-domain deployment checklist

For each of the three attestation-signer domains and each of the three vault-signer domains (six total, unless your organization scales the vault beyond 3 — see docs/12 item 2):

1. **Generate the real key material** inside that domain's own custody boundary (HSM, cloud KMS, hardware token) — this key must never exist anywhere else, including never transiting through this daemon or its config.
2. **Stand up the `/v1/identity` and `/v1/sign` endpoints** as described above, behind a real TLS certificate from a trusted CA.
3. **Provision the bearer token** to both the signer endpoint (for its own auth check) and the daemon's host environment (the variable named in `auth_token_env`) via your organization's secrets-management process — never a config file, never git.
4. **Verify independently, before pointing production config at it**: `curl -H "Authorization: Bearer $TOKEN" https://your-endpoint/v1/identity` returns the expected public key; a signed test payload through `/v1/sign` actually verifies (secp256k1 for vault, ed25519 for attestation) against that same public key.
5. **Record the public key** in both `attestation_pubkeys`/`vault_pubkeys` and the matching `attestation_remote_signers`/`vault_remote_signers` entry's `expected_public_key` — identically, to pass the cross-check at config load.
6. **Set `operators.mode = "production"`** only once all endpoints for both groups are verified — the daemon will refuse to start otherwise (missing/miscounted entries), which is the intended fail-closed behavior, not a bug to work around.

## What this deployment does NOT do

- It does not perform the custody-domain composition decision (docs/12 item 2) — which three cloud accounts/HSM vendors/personnel actually run these six endpoints is an organizational decision this document assumes has already been made.
- It does not run a key-generation ceremony for you — that procedure still needs to be written and executed per docs/12 item 2's own open item.
- It does not activate the on-chain upgrade-authority timelock (docs/12 item 3) — a separate, independent decision and action.
- It does not fund any reserve or interact with mainnet by itself — deploying this signer configuration is a prerequisite for production initialization, not the initialization itself.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Daemon exits at startup: `ProductionModeForbidsLocalSigners` | `operators.mode = "production"` but `attestation_key_paths`/`vault_key_paths` is still populated — remove it entirely. |
| Daemon exits at startup: `DevModeForbidsRemoteSigners` | `operators.mode = "dev"` (or omitted, defaulting to dev) but a `*_remote_signers` array is populated — either remove it or set `mode = "production"`. |
| Daemon exits at startup: `RemoteSignerCountMismatch` | The number of entries in `attestation_remote_signers`/`vault_remote_signers` doesn't match the number of pubkeys in `attestation_pubkeys`/`vault_pubkeys` — these must be 1:1, same as the dev-mode key-path lists always were. |
| Daemon exits at startup: `RemoteSignerExpectedKeyMismatch` | A remote-signer entry's `expected_public_key` doesn't match the positionally-corresponding entry in `attestation_pubkeys`/`vault_pubkeys` — check for a copy/paste error or reordered list. |
| Daemon exits at startup: `DuplicateRemoteSignerPubkey` | Two entries in the same signer group (`attestation_remote_signers` or `vault_remote_signers`) declare the same `expected_public_key` — these are supposed to be three genuinely separate custody domains; give each its own key. |
| Daemon exits at startup: `DuplicateRemoteSignerEndpoint` | Two entries in the same signer group share an `endpoint_url` — even with distinct keys, this collapses two "domains" onto one network endpoint; point each entry at its own domain's endpoint. |
| Daemon exits at startup: `RemoteSignerConnect` / `InsecureEndpoint` | `endpoint_url` isn't `https://` — production mode requires it unconditionally. |
| Daemon exits at startup: `RemoteSignerConnect` / an identity mismatch detail | Your endpoint's `/v1/identity` response doesn't match the configured `expected_public_key` — verify you're pointing at the right endpoint and that endpoint really holds the key you think it does. |
| Daemon exits at startup: `AuthTokenMissing`/`AuthTokenEmpty` | The environment variable named by `auth_token_env` isn't set (or is set empty) in the daemon's own process environment — this is a daemon-host-side secret provisioning gap, not a config file problem. |
| A settlement fails with `SignerError::Unavailable` | Your endpoint returned a `5xx`, or the connection failed outright (network/DNS/TLS) — a liveness problem on your side; the daemon retries next tick. |
| A settlement fails with `SignerError::Rejected` | Your endpoint returned a `4xx` (an explicit policy refusal), a malformed response body, or a signature that failed local verification — the daemon does *not* retry this identically; investigate what your signer actually did with this specific request. |
| A settlement fails with `SignerError::Timeout` | Your endpoint didn't respond within `timeout_ms` (or the daemon's own `service.signer_timeout_ms`, whichever is tighter) — check your signer's own latency, not just its correctness. |
