# Signer-side policy (custody domain operators)

Operator-facing companion to `service/src/signing/policy.rs`. Read
[26-production-signer-deployment.md](26-production-signer-deployment.md)
first — this document assumes you already run an endpoint that speaks the
`/v1/identity` + `/v1/sign` protocol described there, and it adds one thing
to it: **what your signer must decide before it signs.**

Added 2026-09-02 in response to the reserve-withdrawal incident. See
[29-reserve-withdrawal-hardening.md](29-reserve-withdrawal-hardening.md) for
the full incident analysis.

## The problem this solves

Your signer currently receives `{"payload_hex": "..."}` and a bearer token,
and returns a signature. It has no opinion about the bytes.

That was enough to lose the reserve. On 2026-09-02 someone with access to an
authenticated production shell obtained the admin keypair *and* the bearer
tokens for your endpoints — both were resident on the same host — and asked
you to sign a withdrawal to an account they controlled. You signed it,
correctly, because a valid token is all you were ever asked to check.

The design intends three genuinely separate custody domains and a 2-of-3
threshold. In practice the threshold reduced to "possession of two bearer
tokens", and both tokens lived in one filesystem. **Two factors that share a
host are one factor.**

The on-chain `RebalancePolicy` allowlist now bounds the damage regardless of
what you do. This document is the other half: making a stolen bridge-host
credential insufficient in the first place.

## What changes for you

Three things, in descending order of value.

### 1. Two credentials, not one (do this first)

Issue your endpoint **two separate bearer tokens** with different
authorizations:

| credential | authorizes | lives on |
|---|---|---|
| settlement token | `release_from_reserve`, `record_goldcoin_completion` | the bridge host — the daemon uses it continuously and unattended |
| operator token | `treasury_withdraw`, `refund_withdraw` | **the approval host only. Never the bridge host.** |

A third, for `propose_/execute_/cancel_*` governance actions, is recommended
and should live wherever your governance approvals are performed.

This single change closes the incident path even with a fully compromised
bridge host, and it needs no code from this repository. The daemon's
credential simply stops being able to ask for a withdrawal.

`service/src/signing/policy.rs` models this as
`SignerPolicy::allowed_classes` over `ActionClass::{Settlement,
ReserveWithdrawal, Governance}`.

### 2. Parse what you are asked to sign

Stop treating `payload_hex` as opaque. The canonical message format is
public, self-describing and fixed:

```
offset  len  field
0       16   domain tag: "GLC_RSV_CLAIM_V1" or "GLC_RSV_GOVRN_V1"
16      1    protocol version
17      32   Solana program id
49      8    attestation-key epoch (u64 LE)
57      1    action byte
58      ..   action-specific fields
```

| action | family | length |
|---|---|---|
| `0x01` | release from reserve | 166 |
| `0x02` | record Goldcoin completion | 146 |
| `0x03` | rebalance withdraw — **RETIRED, always refuse** | 138 |
| `0x05` | treasury withdraw | 178 |
| `0x06` | refund withdraw | 210 |
| `0x03`/`0x04`/`0x07`/`0x08`/`0x09` under the governance tag | governance | 90 |

`glc-reserve-bridge-service::signing::policy::parse_claim` implements this
and is the reference. If your signer is Rust, link it. If it is not,
reimplement it — and reimplement it **strictly**:

- the domain tag must match exactly;
- the action byte must be one you recognize;
- the total length must be exactly that action's length. An action byte at
  another family's length is a confusion attempt, not a rounding error;
- anything else is a `4xx` refusal, never a signature.

**Fail closed on the unknown.** A future protocol action must reach every
custody domain *before* it reaches the bridge, never the other way around.
If that ordering is inconvenient, the inconvenience is the control working.

### 3. Hold your own allowlist and your own ceiling

For `0x05` (treasury withdraw), check the destination at offset 74 against
**your own** configured treasury list, and the amount at offset 66 against
**your own** ceiling.

Not the bridge's list. Not a list you fetch from the bridge. Not the
on-chain list read through the bridge's RPC endpoint. A list your operators
configured through your change process, which an attacker on the bridge host
cannot reach.

This is the whole point, and it is easy to get wrong in a way that looks
right: a domain that "checks the allowlist" by reading it from the requester
has rebuilt the original vulnerability with extra steps.

Your list should mirror the on-chain `RebalancePolicy` allowlist — verify
that with `glc-admin rebalance-policy-show --rpc-url URL` against an RPC
endpoint you choose — but it must be maintained separately. An attacker then
has to subvert the governance quorum *and* every custody domain's
configuration, rather than just the quorum.

Your ceiling should be at or below the on-chain rolling budget. A
withdrawal larger than your ceiling is not necessarily an attack, but it is
necessarily a conversation.

## Per-action guidance

### `0x05` treasury withdraw

```
58   8   nonce (u64 LE)          — high bit MUST be clear
66   8   amount (u64 LE)
74   32  destination token account
106  32  reserve mint
138  32  source reserve token account
170  8   RebalancePolicy.version (u64 LE)
```

Refuse unless **all** of:

- destination ∈ your own treasury allowlist;
- amount ≤ your own ceiling;
- reserve mint is the one you serve;
- program id is the deployment you serve;
- the nonce's high bit is clear (it is reserved for refunds);
- for amounts above your escalation threshold, an out-of-band human
  approval reference you validated yourself.

The `policy_version` field is worth logging: it tells you which revision of
the on-chain allowlist the withdrawal is being authorized under. If it does
not match what you last saw, the allowlist changed — find out why before
signing.

### `0x06` refund withdraw

```
58   8   nonce (u64 LE)          — high bit MUST be set
66   8   amount (u64 LE)
74   32  destination token account (the depositor's ATA)
106  32  reserve mint
138  32  source reserve token account
170  8   withdrawal-obligation index (u64 LE)
178  32  the obligation's requester
```

A refund's destination is a member of the public, so no allowlist applies.
Verify it differently: read `WithdrawalObligation` #`obligation_index` from
**your own** Solana RPC endpoint and confirm

- its `requester` equals the requester at offset 178;
- its `amount` equals the amount at offset 66;
- its status is still `Pending`;
- the destination at offset 74 is the canonical ATA of
  `(requester, reserve mint, reserve token program)`.

The program enforces all four on chain. Checking them yourself is what makes
the enforcement independent rather than merely duplicated.

### Governance messages

The parameters are behind a SHA-256 commitment, so you cannot read the
proposed allowlist or key set out of the payload. That is deliberate — it
keeps the signed message fixed-length — and it means you **must** obtain the
proposal out of band, recompute the commitment yourself
(`shared::governance::{rotation_params, rebalance_policy_params}` then
SHA-256), and compare.

Default to refusing governance. Enter an approved commitment deliberately
when a proposal has been reviewed, and remove it once executed.

## What good looks like

An audit log line per request, whether approved or refused, recording: the
credential used, the parsed summary
(`ClaimRequest::summary()` produces one), the decision, and the reason on
refusal. If your signer logs nothing else, log that.

A refusal is a `4xx` with a useful `error`/`detail` — the daemon surfaces it
and does not retry identically. A `5xx` means "I am temporarily unavailable"
and *will* be retried; do not use it for policy refusals.

## What this does not solve

- **A compromised custody domain.** If an attacker owns your signer process,
  policy in that process is theirs too. That is what the threshold is for:
  they need two.
- **A compromised governance quorum.** Two of three attestation keys can
  change the on-chain allowlist. Your independently-held list is what makes
  that insufficient on its own — which only holds if you actually maintain
  it separately.
- **The admin key.** The admin still co-signs every withdrawal and still
  controls pause and `protected_minimum`. Nothing here changes that; see
  [29-reserve-withdrawal-hardening.md](29-reserve-withdrawal-hardening.md)
  for what does and does not bound a compromised admin.
