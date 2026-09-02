# Reserve-withdrawal hardening (2026-09-02)

Incident analysis and the design of the patch on
`security/reserve-withdrawal-hardening`.

**Nothing in this document has been deployed.** It describes code that
exists in the repository and a migration that has not been performed.

## 1. What happened

An unauthorized operator obtained access to an authenticated production
shell and used the legitimate Solana reserve rebalance withdrawal workflow:

1. globally paused the bridge (`set_paused`),
2. created a rebalance withdrawal to an arbitrary external token account,
3. obtained the required signer attestations,
4. simulated it,
5. executed it,
6. unpaused the bridge.

Every cryptographic control worked exactly as designed. The admin signature
was genuine. The 2-of-3 threshold attestation was genuine signatures from
genuine current attestation keys over the exact bytes that executed. Nothing
was forged, bypassed or exploited.

## 2. Why it worked

### The two factors were not independent

`rebalance_withdraw` required two things: the admin's signature, and a
threshold attestation. The module documentation was explicit that "neither
party alone is sufficient."

In deployment both were reachable from one shell:

| intended factor | actual secret | where it lived |
|---|---|---|
| admin signature | `admin-keypair.json` | production host filesystem |
| 2-of-3 threshold from three separate custody domains | ≥2 bearer tokens | production host environment |

The attestation signers were **blind oracles**: `POST /v1/sign` took an
opaque `payload_hex` and a bearer token, and the entire authorization
decision was whether the token was valid. So the threshold did not
distribute trust across three custody domains — it reduced to possession of
two secrets from one filesystem.

Two factors that share a host are one factor.

### Nothing bounded the blast radius

Given that single factor, the instruction placed almost no limit on what
could be taken or where it could go:

- **Destination was arbitrary.** The only constraints were `token::mint` and
  `token::token_program`, plus "not the reserve account itself". Any token
  account of the reserve mint on Solana qualified.
- **No per-withdrawal limit.** `BridgeConfig.per_transfer_limit` is enforced
  on settlements (`release_from_reserve`, `deposit_to_reserve`) and was
  never applied here.
- **No velocity limit.** Neither `RollingVolumeWindow` applied either.
- **`protected_minimum` was the only cap** — and `set_limit(ProtectedMinimum,
  0)` is admin-immediate with no zero-check, so the same key could remove
  it.
- **The pause precondition became camouflage.** Pausing is admin-immediate
  in both directions, so pause → drain → unpause reads as a maintenance
  window.

### The dual-control workflow was not on the execution path

`rebalance_requests` (ledger schema v6) has `required_approvals`,
`approved_by`, and a full propose → approve → execute state machine, exposed
through `glc-admin rebalance-propose/-approve`. Neither
`glc-rebalance-withdraw-solana` nor `service/src/solana/refund.rs` ever
imported the ledger or referenced a rebalance id. The approval workflow was
a bookkeeping record the withdrawal tooling never consulted, and it has no
`destination` column, so even a fully-approved rebalance did not commit to
where funds went.

## 3. Findings

| # | severity | finding | status in this patch |
|---|---|---|---|
| F-1 | critical | Attestation signers sign arbitrary bytes on bearer-token authentication alone | Addressed off-chain: `signing::policy` + `docs/28-signer-policy.md`. **Requires action outside this repository.** |
| F-2 | critical | `rebalance_withdraw` accepts an arbitrary destination | Fixed: instruction retired; replaced by `treasury_withdraw` (allowlist) and `refund_withdraw` (derived destination) |
| F-3 | critical | Upgrade authority and `BridgeConfig.admin` are one key on one host | Tooling added (`show-authorities`, `transfer-admin`, `accept-admin`). **The rotation itself is a manual action outside this repository.** |
| F-4 | high | No per-withdrawal and no velocity limit on the withdrawal path | Fixed: `RebalancePolicy.per_withdrawal_limit` + `rolling_limit` |
| F-5 | high | `set_limit(ProtectedMinimum, 0)` is admin-immediate with no zero-check | **Not changed** — see §7 |
| F-6 | high | Pause is admin-immediate, so the precondition is attacker-controlled | **Not changed** — deliberate; see §7 |
| F-7 | medium | The off-chain dual-control workflow is not on the execution path | **Not changed** — see §7 |
| F-8 | medium | Refunds are not once-only on chain | **Not changed** — see §7 |
| F-9 | medium | `transfer_admin` has no timelock and no CLI | CLI added; timelock not added (see §7) |
| F-10 | low | No policy-revision binding in the claim | Fixed: `policy_version` is in the treasury claim message |
| F-11 | low | `BridgeConfig`'s doc table advertised a `reserved` field it never had | Fixed |

## 4. What the patch changes

### On chain

**`RebalancePolicy`** (new PDA, seed `rebalance_policy`) holds three
independent bounds on operator withdrawals:

1. **Where** — `treasuries[..treasury_count]`, an exact-match allowlist of
   destination token accounts. Not a prefix, not a derivation, not an owner
   check.
2. **How much at once** — `per_withdrawal_limit`.
3. **How much over time** — `rolling_limit` over a fixed
   `rolling_window_seconds` bucket, tracked in the same account.

Its own PDA rather than new `BridgeConfig` fields, because `BridgeConfig`
has **no reserved padding** (F-11) and extending it would mean reallocating
a live account holding the bridge's entire governance state.

The rolling window lives inside `RebalancePolicy` rather than in a
`RollingVolumeWindow`, deliberately: `reset_rolling_volume_window` is
admin-gated, and reusing that account type would have handed a compromised
admin a one-transaction reset of the limit that exists to contain a
compromised admin.

**Governance of the policy** is threshold-attested and, for every change
after creation, timelocked — the same mechanism that protects the
attestation-key set, for the same reason. An allowlist a single admin could
edit is not an allowlist: the attacker would add their own token account and
then take the ordinary, fully-attested path.

- `initialize_rebalance_policy` — threshold proof, one-time, not timelocked
  (it can only move from "nothing permitted" to "these permitted").
- `propose_/execute_/cancel_rebalance_policy` — threshold proof plus
  `governance_timelock_seconds` of public delay.

Executing an update deliberately does **not** reset the rolling window: a
governance change is not a budget top-up.

**`treasury_withdraw`** replaces the withdrawal half of the retired
instruction. Every prior check is preserved (global pause, admin signature,
epoch, `amount > 0`, `protected_minimum`, extension revalidation, threshold
attestation, nonce replay guard) and four are added: nonce namespace,
policy validity, allowlist membership, per-withdrawal and rolling limits.
Its claim binds `policy_version`, so an approval collected under one
allowlist revision dies when governance moves to the next.

**`refund_withdraw`** replaces the refund half. Its destination is not
allowlisted — a depositor is a member of the public — it is **derived**:
Anchor's `associated_token::authority` constraint recomputes the ATA from
the obligation's own immutable `requester`. The amount must equal the
obligation exactly and the obligation must still be `Pending`. An operator
chooses which obligation to refund and nothing else.

**`rebalance_withdraw`** returns `RebalanceWithdrawRetired` before touching
any state. Kept as a fail-closed stub rather than deleted so stale tooling
and replayed pre-upgrade transactions fail loudly with an error naming their
replacement, and so the incident replay test can present the exact
transaction shape that used to succeed.

**Nonce namespaces** are now enforced on chain: `treasury_withdraw` requires
the high bit clear, `refund_withdraw` requires it set. Previously this split
(`Ledger::SOLANA_REFUND_NONCE_DOMAIN`) was convention only.

### Off chain

- `glc-rebalance-withdraw-solana` → **`glc-treasury-withdraw`**, with
  `--destination` removed entirely. `--treasury` disambiguates among
  already-allowlisted entries and is checked against the allowlist. With one
  allowlisted treasury — the production posture — no destination input is
  accepted at all. The rename is deliberate: an operator with muscle memory
  gets "command not found" and reads the runbook.
- `service/src/solana/refund.rs` retargeted to `refund_withdraw` and the
  `0x06` claim family. Plan/verify/simulate/confirm structure unchanged.
- `signing::policy` — the parser and policy engine each custody domain runs
  to understand and refuse what it is asked to sign (F-1).
- `glc-admin show-authorities` / `transfer-admin` / `accept-admin` /
  `rebalance-policy-show`.

## 5. Test coverage

| suite | tests | what it establishes |
|---|---|---|
| `tests/incident_replay.rs` | 12 | The incident, replayed with genuine credentials, now fails at every step — and legitimate operations still work |
| `tests/treasury_withdraw.rs` | 23 | Allowlist, both limits, and every preserved invariant re-asserted from scratch |
| `tests/refund_withdraw.rs` | 15 | Destination derivation, obligation binding, preserved invariants |
| `tests/rebalance_policy.rs` | 19 | Governance: who may create, change, cancel — and that the admin key alone can do none of it |
| `tests/rebalance_withdraw.rs` | 4 | The retirement is unconditional across the argument space |
| `signing::policy` | 19 | A policy-enforcing signer refuses the incident payload; the bridge host's credential cannot authorize any withdrawal |
| `glc-treasury-withdraw` | 30 | CLI refuses unallowlisted destinations, over-limit amounts, and stale policy versions before contacting a signer |

The incident-replay suite grants the attacker everything they actually had:
a real admin signature, real 2-of-3 attestations over the exact executing
bytes, and a genuinely paused bridge. Nothing there depends on the attacker
failing to obtain a credential.

## 6. Migration

See `RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md` for the operator procedure.
The ordering constraint that matters: **phases 1–3 need no program change
and already close the incident path.** The on-chain work is defence in depth
that survives a future signer-side mistake.

Backwards compatibility:

- `rebalance_withdraw` is retired, so `glc-treasury-withdraw` and
  `service/src/solana/refund.rs` must ship in the same release. In-flight
  `attested-plan.json` files are dead by construction (new action byte, new
  length).
- `BridgeConfig` is **not touched** — no realloc, no `protocol_version`
  bump, no risk to the live account.
- `RebalanceWithdrawal`'s layout is unchanged, so `decode_rebalance_withdrawal`
  keeps working on every historical record. `reserved[0]` now carries the
  withdrawal class (`0x00` for pre-split records).
- `PROTOCOL_VERSION` stays 1. The claim families change by *adding* action
  bytes `0x05`/`0x06`; `0x01`/`0x02` are untouched, so settlement signatures
  in flight during the upgrade are unaffected.
- Ledger schema: **no migration in this patch.** Adding `destination` and
  `class` columns to `rebalance_requests` belongs with F-7.

## 7. Deliberately not changed

Each of these is a real finding. Each was left alone for a stated reason,
not overlooked.

**F-5, `set_limit(ProtectedMinimum, 0)`.** Still admin-immediate with no
zero-check. Moving it behind governance is correct and recommended, but it
is a change to the settlement path's limit machinery, and this patch is
scoped to the withdrawal path. It now buys an attacker nothing — the
withdrawal limits live in a different account the admin cannot reach — and
`incident_replay.rs` asserts exactly that.

**F-6, admin-immediate pause.** Preserved on explicit instruction. It is the
control the attacker turned into camouflage, and once the destination is
allowlisted and the amount capped it arguably buys less than it costs in
outage time. That is a policy decision, not a hardening one, and it should
be taken separately.

**F-7, the dual-control workflow off the execution path.** Requires a ledger
schema migration (`destination`, `class` on `rebalance_requests`) plus
wiring `glc-treasury-withdraw` to require an `Approved` rebalance id. Worth
doing; it is an additional control on top of the allowlist rather than a
substitute for it.

**F-8, refunds not once-only on chain.** `refund_withdraw` does not mark the
obligation, so a second refund under a different nonce remains on-chain
legal. Today this is prevented off-chain by `solana_refunds`' primary key,
exactly as before — the guarantee is unchanged, not weakened. Closing it
means adding `WithdrawalStatus::Refunded`, which changes a wire value that
`service::solana::{refund, accounts, indexer, manual_review_settle}` all
match on. That is a settlement-path change and needs its own audit.

**F-9, no timelock on `transfer_admin`.** The two-step handover is sound
against a *lost* key; against a *compromised* one, an attacker can still
take permanent ownership in one transaction. A threshold-attested recovery
rotation would fix that. Explicitly out of scope: "do not invent a second
routine admin-rotation mechanism unless required for recovery."

## 8. What still requires action outside this repository

The patch bounds the damage. It does not, and cannot, fix the deployment.

1. **Move the program upgrade authority off the bridge host**, to a hardware
   or multisig key. Until this happens, `9Ldtd…` can replace the program
   with `solana program deploy` and every control described here is
   downstream of one key on one machine. This is the deepest issue and the
   patch does not touch it.
2. **Rotate `BridgeConfig.admin`** to a key that never touches the bridge
   host, separate from the upgrade authority.
3. **Deploy signer-side policy and action-scoped credentials** at all three
   attestation domains (`docs/28-signer-policy.md`). This is the change that
   closes the incident path even with a fully compromised host, and it needs
   no redeploy.
4. **Move the attestation credentials to an approval host** and adopt the
   three-host `plan` → `attest` → `execute` split.
5. **Choose the treasury and the limits.** The allowlist is only as strong
   as the custody of the wallet behind the allowlisted token account — an
   allowlisted destination whose owner key sits on the same compromised host
   would defeat the entire control.
6. **Revoke and reissue every credential** assumed compromised: admin,
   submitter, deployer, both sets of bearer tokens, and the Goldcoin RPC
   credentials. Rebuild the host.
