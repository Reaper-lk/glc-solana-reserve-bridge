# Consolidated production-readiness review

Originally performed 2026-08-15, read-only against the repository state at
that time plus every prior checkpoint (docs/00 through docs/21). Updated
2026-08-15 (same day) after a follow-on implementation round that closed
six local-only items (signer trait abstraction, off-chain rebalancing,
post-finality-reorg protection, off-chain key-rotation/vault-sweep
tooling, an expanded read-only bridge API, and the external-audit scope
document).

**Updated again 2026-08-15** after a second follow-on round: completed
the remaining read-only API work (`GET /stats`, `GET /reserves/history`,
`GET /explorer/events`, `GET /transfers` wallet-scoped listing) and
implemented and real-CPI-tested the timelocked program-upgrade mechanism
(docs/12 item 3, option (c)) — see review items 9 and 13 below for full
detail. Every section is marked either unchanged or updated inline;
nothing is silently re-scored without a stated reason. Scope: everything
in `programs/`, `service/`, `shared/`, `docs/`, `tests/`, `docker/`,
`scripts/`, `.github/`, plus a read-only timestamp/content check of the
connected frontend repository at `/home/reaper/glc-solana-bridge-ui` for
the UI-readiness question only (not modified, not part of this
repository; not re-checked this round — see item 26).

Code WAS changed to produce both updates. See this round's commits added
on top of `main`, all local, none pushed. No production keys were
generated or used. No funds were moved. Nothing was deployed to any real
network (every on-chain test, including the new upgrade-authority CPI
tests, runs against a local litesvm instance with fresh throwaway
keypairs). No mainnet transaction was submitted. Nothing was pushed or
opened as a PR.

Standing invariants re-confirmed as still true throughout the current
codebase during this review (unchanged from every prior audit): pre-funded
reserve-backed bridge, existing GLC on both sides, no minting, no burning,
no wrapping, no token creation, no supply modification, 1:1 on the
underlying GLC denomination before the 1% service fee, real Solana GLC
Token-2022 mint compatibility, fail-closed behavior on RPC failure,
unknown state, insufficient reserve, reorg, and accounting inconsistency.

## How to read this document

Every remaining item is classified:

- **A** — engineering work this session (or a future one like it) can
  implement locally now, no new information or infrastructure needed.
- **B** — engineering work that needs real infrastructure (a second real
  node, a testnet, load-generation capacity, a real HSM/cloud KMS) or
  extended real-world testing this sandbox cannot provide.
- **C** — a security or organizational decision only management can make.
- **D** — a production deployment/configuration task (turning an already-
  decided value into a running system) rather than a design decision or
  new code.
- **E** — work that must be done by an external party (a security audit
  firm, an infrastructure vendor).

---

## 1. Core bridge state machines, both directions

**Status: essentially complete.** `service/src/ledger/mod.rs` +
`docs/04-state-machines.md` implement one parameterized state machine for
both directions. Every automatic/chain-derived transition, every terminal/
error state (`Expired`, `Cancelled`, `Reorged`, `InsufficientReserveAtSettlement`,
`DestinationSubmissionFailed`, `ManualReview`, `Failed`), and idempotency
at every step is implemented and covered by both unit tests
(`ledger::tests`) and real-node crash/restart tests
(`tests/restart_recovery.rs`, `tests/regtest_acceptance.rs`'s
crash-and-rebuild scenario). Pause is correctly modeled as a system-level
gate, not a per-request state, checked at every automatic transition
point.

**Remaining gap (real, not cosmetic):** the "late deposit after an
`Expired` reservation" auto-recreate behavior described in
docs/04-state-machines.md's "Open design item" is **not implemented** —
today a late deposit against an already-`Expired` request is correctly
reported as `NoMatchingRequest` (never silently misapplied), but nothing
automatically recreates a fresh reservation or routes it to `ManualReview`
for a compensating action as the design doc describes. This is a real,
if narrow, gap between documented and actual behavior.
**Classification: A** (bounded, well-specified, no new infrastructure).

## 2. Real Solana Token-2022 GLC compatibility

**Status: complete and real-node verified**, per docs/18–19. The on-chain
program uses `anchor_spl::token_interface` throughout, structurally pins
whichever program (legacy SPL or Token-2022) actually owns a configured
reserve's mint, and enforces an explicit extension allowlist
(`MetadataPointer`/`TokenMetadata`/`ImmutableOwner`) re-checked on every
reserve-touching instruction — a not-yet-reviewed future extension type
fails closed by construction. Verified against the real mainnet mint
(`Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump`, Token-2022, 6 decimals,
`MetadataPointer`+`TokenMetadata` only, both authorities renounced) via a
read-only mainnet RPC call, and exercised end-to-end against a real
Token-2022 program in both litesvm (on-chain adversarial suite) and a real
`solana-test-validator` (real-node acceptance suite).
**No remaining engineering gap.** The only residual risk is that the live
mint's extension set could theoretically change before a real deployment
(re-verification immediately before any mainnet deploy is a deployment-
time checklist item, not a code gap). **Classification: D** for that
final pre-deploy re-check.

## 3. Goldcoin 8-decimal / Solana 6-decimal conversion

**Status: complete.** `service/src/amount_conversion.rs` is the single
canonical conversion implementation: widening is always exact, narrowing
is exact-only-or-rejected (never rounds/truncates), Solana decimals are
always a live read, never hardcoded. Real bug found and fixed during the
Token-2022 round (a 100x fund-movement error from an earlier
hardcoded-decimals assumption) — now covered by dedicated regression
tests and real-node tests that specifically exercise the real 8-vs-6
mismatch rather than a degenerate equal-decimals case.
**No remaining gap. Classification: n/a (done).**

## 4. 1% bridge fee enforcement and accounting

**Status: complete**, per docs/20–21 (this session). Fee formula, typed
canonical/Solana-atomic units preventing cross-chain unit confusion at
compile time, `verify_fee_breakdown` recomputing and failing closed on any
stored-fee/net tamper at every settlement-construction call site,
accrued-fee tracking (never counted toward capacity), and a
server-authoritative `/quote` endpoint are all implemented and tested
(41 new/updated tests this round specifically for fee/accounting
behavior, on top of the pre-existing suite).
**Remaining, deliberately deferred (not gaps): no fee-withdrawal
path/treasury design; no business-minimum-transfer policy on top of the
mathematically-derived minimums.** Both were explicit non-goals for this
round. **Classification: C** (fee withdrawal needs a treasury/authorization
decision) **for future work**, not blocking anything today.

## 5. Reserve capacity/accounting

**Status: complete.** The previously-flagged "reserve-capacity accounting
unit gap" (docs/19 item 4 — capacity compared raw atomic amounts across
chains with mismatched decimals) is fixed this session:
`reserved_liquidity`/`pending_obligations`/`settled_liquidity_total` now
track NET, in each reserve's own native unit, matching `total_reserve_balance`.
Two dedicated tests prove the capacity check is genuinely net-destination-based,
not gross-based, in both directions. **No remaining engineering gap.**

## 6. Goldcoin reconciliation

**Status: complete**, closed in the P0 round (docs/16). `tick_goldcoin_reconciliation`
runs every tick, covers the same fail-closed/never-auto-clear discipline
as the Solana side, verified with dedicated tests including a
crash-survives-with-pause-intact test. **No remaining engineering gap** in
the mechanism itself.

**Update 2026-08-15: gap closed.** `Ledger::detect_post_finality_reorg`/
`record_post_finality_reorg` and `goldcoin::indexer::Indexer::tick()` now
implement a dedicated, distinguishable code path: any `GlcToSol` request
already marked `source_finalized_at` whose block height falls behind a
detected fork point is caught BEFORE the existing (pre-finality-only)
rollback path runs, writes a distinct `post_finality_reorg_events` audit
row, and unconditionally pauses BOTH reserves (not just Goldcoin's) —
never auto-clears, per the same asymmetric-pause discipline as every
other pause in this codebase. Covered by dedicated unit tests
(`ledger::tests::detect_post_finality_reorg_finds_only_finalized_requests_above_the_fork_height`,
`record_post_finality_reorg_pauses_both_reserves_and_writes_a_distinct_audit_event`),
an indexer-level test rewritten to assert the new `TickOutcome::
PostFinalityReorgHalted` variant fires instead of the old generic
rollback path, and a restart-recovery test proving the persisted pause
(not an in-memory flag) is what survives a crash. **No remaining
engineering gap.** (This closes P1-4 below.)

## 7. Solana reconciliation

**Status: complete, unchanged and solid.** `finalized`-commitment-only
reads (correct by construction, no reorg logic needed), reconciliation
against a live PDA-owned ATA balance, fail-closed/never-auto-clear pause,
real-node verified (docs/14). **Known, documented, accepted design
limit** (not a bug): zero cold-start grace period — any unexplained drop
from the ledger's configured baseline is a breach, with no exception for
"just started, haven't observed reality yet." The correct mitigation
(fund-then-verify-finality-then-start sequencing) is implemented in the
test harness (`wait_for_finalized_balance`) but **not yet written into
docs/09-runbook.md as an explicit startup-ordering requirement for a real
launch.** **Classification: A** (a documentation fix, five minutes of
work) — flagged here because it's cheap and currently still missing, not
because it's hard.

## 8. Daemon/service operation

**Status: complete.** `service/src/daemon.rs` + `bin/glc-bridge-daemon.rs`:
config-driven, drives `Orchestrator::tick()` on an interval, partial-outage-aware
backoff, clean tick-boundary shutdown on SIGINT/SIGTERM, wires health/metrics,
the bridge API, and alerting together, fails closed (exit 2) on any
malformed startup condition. Real-node verified: starts, ticks, serves
`/health`, shuts down cleanly on a real SIGTERM; refuses to start against a
reserve mint owned by the wrong token program. **No remaining engineering
gap in the daemon itself.**

## 9. API completeness

**Update 2026-08-15 (second round): every A-classified read-projection
endpoint this review previously named is now built.** Current surface:
`GET /status` (per-direction availability), `/limits` (`bridge_fee_bps`),
`/reserve`, `/health` (non-sensitive operational summary), `/transfers/:id`
(confirmation progress), `POST /transfers`, `POST /quote` — plus, new this
round, `GET /stats` (aggregate non-sensitive bridge/reserve/indexer
figures: per-direction request counts by state, per-reserve settled
volume and accrued fees, indexer freshness), `GET /reserves/history`
(cursor-paginated, real, already-persisted reconciliation-tick history —
never fabricated or interpolated; a `SKIPPED` classification is surfaced
honestly rather than papered over), `GET /explorer/events` (a public
settlement-event feed built from `bridge_request_state_log` only —
deliberately excludes the rebalance/custody-transition audit trails,
which carry real operator identities and stay operator-only), and
`GET /transfers` (a wallet-scoped list of a caller's own transfers by
address/state, the address-based counterpart to the existing id-based
lookup). **11 endpoints total** (up from 7). All four new list endpoints
share dependency-free cursor pagination, deterministic newest-first
ordering, and a clamped-not-rejected page-size ceiling.
**Still missing, and now the entire remaining gap**: the federation-shaped
surface (`/federation`, `/federation/rounds`, `/incidents`, `/verify`)
the connected frontend's mock-mode client code still expects but which
has no analog in this reserve-backed, non-federated design. The frontend
repository (`/home/reaper/glc-solana-bridge-ui`) was inspected directly
this round (not just timestamp-checked): its schemas confirm it is built
against the OLD wrapped-token/federation-era bridge model outright — wGLC
supply figures, `glc-to-wglc`/`wglc-to-glc` direction naming, and explorer
event kinds literally named `mints`/`burns` — a conceptual mismatch, not
a missing-endpoint gap. Per this round's explicit instruction, no
federation/wrapped-token-era semantics were added to this API to paper
over that mismatch; see item 26 below for the full account.
**Classification: C** for whether/how to reinterpret the federation-shaped
endpoints (product framing decision, not technical); no remaining `A`
items in this area.

## 10. Attestation/signing architecture

**Status: design complete and correctly implemented for dev/test
custody; production custody does not exist.** The approved trust model
(docs/02, internal 2-of-3 threshold attestation + M-of-N Goldcoin vault,
independent re-derivation before every signature) is fully implemented in
library code and real-node verified: a single signer alone can never
authorize anything, independent re-derivation is exercised, replay/
duplicate-settlement guards hold under real adversarial testing. This is
genuinely solid work.
**Update 2026-08-15: the local-only half of the gap is closed.**
`service/src/signing/signers.rs` now defines `VaultSigner`/
`AttestationSigner` traits (`dyn`-compatible, `BoxFut`-returning to stay
`Send`-safe across an `async` boundary without native `async fn` in
traits) that accept only a canonical signing payload and return a
signature plus public identity — never private key material.
`DevVaultSigner`/`DevAttestationSigner` now `impl` these traits rather
than being the only concrete type `Orchestrator` knows about;
`Orchestrator` holds `Vec<Box<dyn VaultSigner>>`/`Vec<Box<dyn
AttestationSigner>>`, generic over the trait. Every signer call site is
wrapped in an explicit `tokio::time::timeout` (a new `signer_timeout`
config field, `service.signer_timeout_ms`, defaulted but tunable) that
maps a hung or slow signer to a distinct `SignerError::Timeout` — a
signer timeout or rejection is structurally fail-closed, never silently
treated as "signed." Proven with new adversarial test doubles
(`FailingVaultSigner`/`FailingAttestationSigner`, an
always-hangs-forever and an always-rejects mode) exercising the timeout
and rejection paths specifically, on top of the pre-existing "single
signer alone cannot authorize" tests, which all continue to pass
unchanged against the new trait-based call sites.
**What's still missing (unchanged): no real HSM/KMS-backed
implementation exists** — only the two dev/test signers do, now correctly
scoped to tests/local development per the trait design's own intent.
**Classification: B** (needs a real HSM or cloud KMS to develop and test
against, not a local sandbox) for the remaining real-backend work; the
trait abstraction itself is done.

**Update 2026-08-18: the real backend now exists — a provider-neutral
HTTPS remote signer client, not a single-vendor SDK.**
`service/src/signing/remote.rs` implements both traits
(`RemoteVaultSigner`/`RemoteAttestationSigner`) over a small, documented
wire protocol (`GET /v1/identity`, `POST /v1/sign`) so any of the three
custody domains the approved trust model requires can sit behind
whatever actually holds its key material — a cloud KMS, a hardware HSM,
a hand-rolled signer process — without this crate depending on a
specific vendor's SDK (docs/26-production-signer-deployment.md is the
full operator-facing runbook). Never holds, generates, imports, or
exports private key material itself; every returned signature is
verified locally (against the exact payload sent and the
already-identity-checked public key) before ever being trusted, using
the same verification this crate already uses elsewhere
(`goldcoin::multisig::verify_partial`/
`solana_sdk::signature::Signature::verify`) — a compromised or buggy
remote signer returning a wrong signature fails exactly the same way a
malformed response does, never silently accepted. `config.rs` gained a
`operators.mode` field (`"dev"`/`"production"`) that gates the entire
signer-loading path: production mode structurally refuses to start
(`ConfigError::ProductionModeForbidsLocalSigners`) if any local
plaintext dev/test signer file path is configured, and dev mode
symmetrically refuses to start if any remote-signer endpoint is
configured — the two paths can never be silently mixed. 18 tests in
`signing::remote::tests` cover successful signing, public-key mismatch,
timeout, connection failure, explicit rejection, malformed response, an
invalid (non-verifying) signature, and that auth tokens never appear in
`Debug`/log output; further tests in `config::tests` cover the
mode-gating fail-closed checks and confirm threshold enforcement is
unaffected by which loading path produced the signers.
**What this does NOT close**: the custody-domain composition decision
(docs/12 item 2 — which three cloud accounts/HSM vendors/personnel
actually run the three endpoints), the key-generation ceremony, and
actually standing up three real, genuinely separate signer processes are
still fully open — this is the client protocol and its production-mode
wiring, not a decision about who holds the keys or infrastructure
running anywhere. **Classification: C** (custody-domain composition,
organizational) for what remains blocking; **D** (deployment-time
configuration task) to actually point production config at real
endpoints once the domains exist.

## 11. Custody/HSM/KMS production readiness

**Status: still 0% for the real integration; generic transition tooling
around it now exists.** No KMS/HSM integration exists — unchanged. New
this round: `service/src/ledger/mod.rs`'s custody-transition state
machine (`custody_transitions`/`custody_transition_state_log`, schema v8)
and 10 `glc-admin custody-*` subcommands provide the generic
propose/verify-identity/approve/record-executed/confirm/fail/rollback
tooling BOTH an attestation-key rotation and a Goldcoin vault sweep would
need — but, exactly like §15's rebalancing tooling, this only ever
records evidence of a rotation/sweep an operator executed out of band; it
does not generate keys, sign anything, or perform a rotation/sweep
itself, and does not reduce the underlying 0%-HSM/KMS gap. No
key-generation ceremony procedure is documented anywhere. The
custody-domain composition decision (which three cloud accounts/HSM
vendors/personnel constitute the three genuinely-separate custody
domains) remains fully open (docs/12 item 2) — this is an organizational
decision this repository cannot resolve on its own no matter how much
code is written. **Classification: C** (organizational decision,
blocking) then **B** (real HSM/KMS integration work, needs real
infrastructure) once decided.

**Update 2026-08-18: narrowed, not closed.** The client/protocol half of
"real integration" now exists (item 10 above) — what remains under this
item is entirely the same organizational decision it always was (which
three domains, held by whom) plus actually operating three real signer
processes behind that protocol. Neither is something a local session can
resolve. **Classification unchanged: C then B.**

## 12. Key loading and secret handling

**Status: dev/test-appropriate, explicitly not production-appropriate.**
`config.rs` loads signing keys from local file paths named in config,
cross-validated against the config-declared pubkey, refused on mismatch —
correctly documented everywhere as "DEV/TEST posture pending the HSM/KMS
work." No production secret ever needs to be embedded in a config file or
this repository as it stands; the seam toward a real implementation
exists (config already separates "which pubkey" from "how to sign with
it"), but the actual secure-loading mechanism doesn't exist yet.
**Classification: B** (depends on item 11's HSM/KMS work).

**Update 2026-08-18: closed for the daemon's own posture; the secret is
still an environment variable, not an HSM-native credential.**
Production-mode remote signing never loads a private key into this
process at all — `signing::remote::AuthToken` is a bearer-token
credential (named by `auth_token_env`, read once from the environment at
startup, never itself a config value, never committed to git, redacted
from every `Debug`/log path by construction), used only to authenticate
to the actual custody domain, which holds the real signing key entirely
outside this daemon. This is a real, structural improvement over the
dev-file posture (no private key material of any kind ever enters this
process's memory in production mode) but is not itself HSM/KMS-native
secret management (a cloud KMS's IAM-based auth, a hardware token, a
short-lived credential broker) — the bearer-token-in-an-env-var pattern
is deliberately the simplest provider-neutral mechanism that still
satisfies "no secret in git" and "never logged," and whichever custody
domain sits behind the protocol is free to layer a stronger credential
delivery mechanism (e.g. injecting the env var from a secrets manager at
process start) without any change to this client. **Classification: D**
(deployment-time secret-delivery choice) for the token itself; the
custody domain's own key-loading is item 11's remaining scope.

## 13. Program upgrade authority

**Update 2026-08-15 (second round): docs/12 item 3's recommended interim
option (c) is now built and real-CPI-tested — but not activated on any
real deployment, and the underlying posture decision is still open.**
`programs/glc-reserve-bridge/src/instructions/upgrade_timelock.rs`
implements `accept_upgrade_authority` (a one-time handoff of the
program's real, loader-level upgrade authority to a data-less program PDA
— the same `invoke_signed`-signing-PDA pattern already used for
`reserve_authority`) plus `propose_upgrade`/`execute_upgrade`/
`cancel_upgrade`: admin-gated to propose/cancel, permissionless to
execute once a configurable `upgrade_timelock_seconds` has elapsed,
fails closed with a distinct `UpgradeAuthorityNotYetAccepted` error if
the handoff was never performed (never silently no-ops as "success"),
and singleton-PDA-structural replay protection matching the existing
attestation-key-rotation governance pattern. Tested end-to-end in
litesvm against the REAL `bpf_loader_upgradeable` native program —
including a genuine authority handoff and a genuine code-upgrade CPI
(the loader visibly closes the consumed buffer account, proving this is
not a mocked/simulated code path) — not just the timelock's own internal
state transitions.
**What remains unresolved, unaffected by the mechanism now existing**:
whether this timelocked-PDA posture is ever actually used on a real
deployment at all, versus full threshold custody or revoking
upgradeability entirely (docs/12 item 3's other two options), and — if
so — who signs the one-time `accept_upgrade_authority` call and when.
Shipping this code changes nothing about any real deployment's actual
upgrade authority by itself; per the threat model's own words, an
upgradeable program whose upgrade authority isn't under real custody
discipline "undermines every on-chain control this design relies on," and
that remains true until `accept_upgrade_authority` is deliberately
called with a real key — an action this session correctly did not take.
**Classification: C** (which posture, and whether/when to activate it, is
a management decision) for what remains; the local-only engineering
work (**A**) is done.

## 14. Reserve wallet/vault security

**Status: cryptographically sound design, no production instance exists.**
Goldcoin vault: P2SH M-of-N multisig, construction and payout-signing
mechanics real-node verified. Solana reserve: PDA-owned ATA, no private
key exists for it at all (program-derived authority only) — the strongest
possible posture for that leg, already achieved. **No vault has ever held
real value**; the entire gap here is items 11-13 (who holds the Goldcoin
multisig keys, in what custody domains, under what upgrade-authority
posture for the program that gates the Solana leg). **Classification:**
folds into 11/13 above — no separate engineering work exists for this
item alone.

## 15. Rebalancing implementation

**Update 2026-08-15: the off-chain engineering layer is now built, on a
deliberately reconsidered design.** Rather than dedicated on-chain
`rebalance_deposit`/`rebalance_withdraw` instructions, the implemented
design keeps real fund movement entirely out of band, through whatever
real Goldcoin/Solana wallet or custody tooling already holds the relevant
keys, and this service only ever *records evidence* of a transfer an
operator already authorized and executed — never constructs, signs, or
broadcasts one itself. Concretely: a `rebalance_requests` +
`rebalance_state_log` schema (v6), a `Proposed -> Approved -> Executed ->
Confirmed` state machine (off-ramps `Rejected`/`Cancelled` pre-execution,
`Failed` post-execution) in `service/src/ledger/mod.rs`, a pure read-only
imbalance-severity classifier (`service/src/rebalance.rs`) against each
reserve's own already-configured thresholds (never inventing a policy
value), 9 `glc-admin rebalance-*` subcommands, a `tx_reference` UNIQUE
constraint as the structural replay guard, and a confirmed-rebalance
balance adjustment that happens atomically with the state transition (so
the very next reconciliation tick sees an already-explained balance).
Covered by 10 dedicated unit tests, a reconciliation-interaction test, and
a 3-stage restart-recovery test. **This changes what "done" means for
this item**: the dedicated on-chain `rebalance_deposit`/`rebalance_withdraw`
instructions docs/03-architecture.md originally envisioned (an atomic,
on-chain-enforced structural separation between a rebalance transfer and
an arbitrary one) remain unbuilt and are no longer this item's blocking
gap — building them would be a *strengthening* of an already-functional
off-chain-evidence design, not closing a 0%-implemented gap.
**Classification: A** if/when the on-chain instructions are still wanted
as a future hardening step; the off-chain engineering layer itself is
done. (This closes P1-1 below, restated rather than removed.)

## 16. Pause/unpause/emergency controls

**Status: complete.** Three on-chain scopes (`Global`, `Release`,
`Deposit`), both an on-chain admin-gated-immediate pause
(`glc-admin onchain-pause/-unpause`) and this service's own local
admission gate (`glc-admin pause/unpause`), asymmetric by design (fast/
automatic to pause via reconciliation breach, slow/manual-only to
resume). Real-node verified, including pause-survives-restart.
**No remaining engineering gap.**

## 17. Rate limits and transfer limits

**Status: fields and enforcement exist; production values do not.**
`LimitField` (`MinTransferAmount`, `PerTransferLimit`, `ProtectedMinimum`,
`RollingVolumeLimit`) is admin-gated, immediate (interim, not yet the
asymmetric timelocked-governance posture named as the eventual design in
the program's own module docs), enforced on-chain for the GLC→SOL leg and
service-side for SOL→GLC. **No real production numbers have been chosen**
(docs/12 item 6) — shipping with the fast-iteration test values currently
in every fixture would be unsafe. **Classification: C** (values need a
joint security/product decision) then **D** (config task once decided).

## 18. Confirmation-depth/finality policy

**Status: fields and mechanism exist; production values do not.**
Goldcoin `confirmation_depth`/`max_reorg_depth`/`vault_min_confirmations`
are real, non-hardcoded config fields; Solana correctly hardcodes
`finalized` commitment everywhere (deliberate, not a gap). **No
production-safe values have been chosen or reviewed against real Goldcoin
hashrate/reorg history** (docs/12 item 4) — this genuinely needs real
data, not an engineering guess. **Classification: C** (needs whoever owns
Goldcoin infrastructure operationally) then **D**.

**Update 2026-08-21: a pilot-interim value is now set and approved** —
`confirmation_depth = 200`, `max_reorg_depth = 250`,
`vault_min_confirmations = 20`; see [09-runbook.md](09-runbook.md),
"Confirmation-depth values (pilot, approved 2026-08-21)," and "Pilot
Launch Policy" below. This is a deliberately conservative number chosen
without real Goldcoin hashrate/reorg data, not the data-backed value
docs/12 item 4 still calls for — that remains open and is now explicitly
a scale gate, not a pilot blocker. The classification above is unchanged
for the *final*, data-backed value.

## 19. Crash/restart recovery

**Status: complete, the most thoroughly proven property in this
codebase.** No-op-if-already-there / hard-assert-if-genuinely-unexpected
at every transition, backed by unique indexes, the `DepositClaim` PDA, and
terminal-state guards. Real-node exercised at multiple points in the
lifecycle including orchestrator-dropped-mid-settlement-and-rebuilt.
**No remaining engineering gap.**

## 20. Replay and duplicate-settlement protection

**Status: complete, with a named, accepted asymmetry.** GLC→SOL: on-chain
`DepositClaim` PDA, cryptographically enforced, real-node-proven against
an independently-reconstructed replay attempt. SOL→GLC: database `UNIQUE`
constraint + multisig-signer independent re-verification — operationally,
not cryptographically, enforced, since Goldcoin has no program layer. This
asymmetry is explicitly documented everywhere it matters (threat model,
schema comments, this review) rather than implied away. **No remaining
engineering gap** — the residual risk is structural (Goldcoin's own
capabilities), not a defect, and is exactly what an external audit should
independently weigh in on (see item 25).

## 21. Monitoring/metrics/alerts

**Status: functional, basic.** Real Prometheus `/metrics` (now including
per-direction accrued-fee gauges from this session's work) + JSON
`/health`, continuously served by the daemon. Outbound webhook alerting on
the pause `false→true` edge exists (`ops::alerting`), wired in behind an
optional config field. **Missing:** any richer integration (PagerDuty/
Slack-specific formatting beyond a generic webhook POST), and no
dashboard (no Grafana config or equivalent) exists on top of `/metrics`.
**Classification: A** for both (a Grafana JSON dashboard definition and a
Slack/PagerDuty-specific webhook formatter are both local, no-new-info
engineering work).

## 22. Audit tooling

**Status: complete for its defined scope.** `glc-audit`: offline,
re-verifies every frozen attestation-claim commitment (self-consistency +
recompute-from-current-ledger-state) plus `SQLite PRAGMA integrity_check`,
now including the fee-arithmetic reconciliation check
(`fee_plus_net_ne_gross`) added this session, with documented, explicit
scope limits (does not re-verify fields that legitimately change across a
key rotation). Exit codes designed for cron/systemd-timer wiring;
`scripts/run-audit-cron.sh` ties backup + audit together. **No remaining
engineering gap.**

## 23. Deployment/configuration

**Status: config loading complete; deployment artifact exists but is
unverified; no orchestration beyond a single container.**
`service/src/config.rs`: a single TOML file + env-var overrides, fails
closed on internally-inconsistent values (threshold/pubkey-count
mismatches, `critical_reserve <= protected_minimum`, unsupported network
strings). `docker/Dockerfile`: multi-stage, non-root, config/keys always
mounted at runtime, never baked in — **written but never actually built**
in this sandbox (no Docker daemon access; documented plainly as
unverified, not claimed as tested). No orchestration manifest (Kubernetes,
systemd unit, docker-compose for a full local stack) exists.
**Classification: A** for actually running `docker build` and a systemd
unit file/docker-compose stack (no new information needed, just sandbox
access this session doesn't have — but any environment with Docker access
can do this today); **D** for pointing a real deployment at real
endpoints/keys once everything else is decided.

## 24. Backup/recovery

**Status: complete.** `scripts/backup-ledger.sh` (safe online SQLite
`.backup`, never a raw file copy), `restore-ledger.sh` (integrity-checks
before installing, refuses to clobber), `run-audit-cron.sh` (ties both to
`glc-audit` for a scheduled job). All three manually exercised end-to-end
against a real schema-valid database this session's predecessor produced.
**No remaining engineering gap.** The only open item is *scheduling* it
in a real production environment (a `D` deployment task, not missing
code).

## 25. External security audit requirements

**Update 2026-08-15: scoped, not scheduled, not performed.**
docs/23-external-audit-scope.md now exists — a 21-area scope document
covering the Solana program, Token-2022 integration, reserve accounting,
decimal conversion, the 1% fee, state machines, replay protection
(explicitly naming the SOL→GLC direction's non-cryptographic
DB-constraint asymmetry for the auditor's own independent judgment, not
asserted as safe), attestation, the signing/custody boundary, the
daemon/orchestrator, Goldcoin transaction construction, reconciliation,
rebalancing, key-rotation/vault-sweep tooling, pause/emergency behavior,
API security, on-chain key rotation, upgrade authority, the threat model,
test quality, and production deployment assumptions — plus a dedicated
table separating what's reviewable today (the design and code, against
dev/test values) from what cannot receive a *final* sign-off until
docs/12's still-open management decisions (custody-domain composition,
upgrade-authority posture, confirmation depths, reserve sizing, rate
limits, reservation TTL, refund process) are made. **Classification: E**
(the audit itself must still be performed by an external party) — this
round's work was the local-only scoping half, which is now done.

**Update 2026-08-20: no longer a bounded-pilot launch blocker — now a
scale gate.** See "Pilot Launch Policy" (immediately before the
Prioritized Roadmap section) for the full reasoning and the verified
failure-mode analysis: at the pilot's bounded reserve size, no failure
mode was found where a bug could cost more than the reserve itself, so
the audit's role shifts to protecting against adversarial exploitation
once reserves are large enough to be worth attacking — a threshold not
yet reached, and not yet reached by design. This does not reduce the
audit's scope or requirement for anything beyond the bounded pilot.

## 26. UI/backend integration readiness

**Status: ~15%, backend surface materially closer, frontend itself
unchanged.** This round directly inspected the connected frontend's API
client/schema code (`src/lib/api/`), not just file timestamps: it is
built against a wrapped-token, federated bridge — `wGLC`,
`glc-to-wglc`/`wglc-to-glc` direction naming, explorer event kinds
literally named `mints`/`burns`, a `/federation`/`/federation/rounds`
signing-round surface, ISO-8601 timestamps and camelCase JSON throughout
(this backend uses unix-second integers and snake_case, consistent with
every existing endpoint, not a new mismatch introduced this round). This
is a conceptual/architectural mismatch with the actual reserve-backed,
1:1, non-federated bridge, not a set of missing fields a backend change
can close. The backend-side gap this review can actually act on — the
missing read-projection endpoints — is now fully closed (item 9): what
remains is entirely the federation-shaped-endpoint product decision
(reinterpret `/federation` as the 3 internal custody domains with UI
copy changes, or drop the concept and adjust the frontend) and the actual
frontend rewrite/adaptation work once that decision is made — neither of
which this round's scope (backend-only, no architecture decisions)
could or should resolve.

## 27. Mainnet deployment requirements

**Status: none satisfied yet.** Requires, at minimum, all of items 11-13
and 17-18's decisions resolved and implemented, item 25's audit
performed, item 15's rebalancing built, and item 9's UI/API gap closed —
this is a rollup of the P0/P1 roadmap below, not a separate gap.

## 28. Production reserve-funding requirements

**Status: not started, and correctly so — nothing in this review
recommends starting it.** Requires a decided reserve-sizing policy
(docs/12 item 5, currently open), the custody/HSM work (item 11) to exist
first (funds should never sit behind dev keys), and is itself a real-funds
management decision plus real infrastructure (a funded Goldcoin wallet,
a funded Solana account) — **Classification: C** (sizing decision) +
**F** in spirit, i.e. genuinely out of this review's implementable scope
entirely: this is the one item on this list that must come *last*, after
everything else, not something to parallelize.

- **Update 2026-08-20: initial planned funding amounts approved (not yet
  executed).** 200,000 GLC planned for the Goldcoin L1 reserve, 200,000
  GLC planned for the Solana reserve — see P0-6's "Approved pilot
  bridge-policy parameters" update below for the full pilot policy table
  this planned funding is sized against (e.g. the 200,000 GLC Solana
  figure sits comfortably above the approved 50,000 GLC
  `protected_minimum` floor, leaving real releasable headroom). This is
  a funding *plan*, not funding that has happened — the status above
  ("not started, and correctly so") is unchanged; nothing has been
  transferred, no wallet has been funded, and this item still correctly
  waits on custody/HSM work (item 11) existing first.
- **Superseded by the pilot launch policy immediately below, for the
  pilot specifically**: the 200,000-GLC-each-side figure above was a
  full-scale planning number, not the pilot's. The approved pilot
  funding amount is materially smaller (order of ~$200 of value per
  side, ~$400 total exposure) — see "Pilot Launch Policy" for the exact
  reasoning. The 200,000-GLC figure remains the intended eventual
  full-scale target once the scale gate below is satisfied.
- **Update 2026-08-21: exact pilot GLC quantities set, replacing the
  "to be finalized at funding time" placeholder above.** Planning
  reference price: 1 GLC = $0.002160. Split ~equally:

  | Reserve | Planned initial funding | Approx. value |
  |---|---|---|
  | Goldcoin L1 reserve | **92,600 GLC** | ~$200.016 |
  | Solana GLC reserve | **92,600 GLC** | ~$200.016 |
  | **Total** | **185,200 GLC** | **~$400.032** |

  ($200 / $0.002160 = 92,592.592593 GLC exactly; rounded up to 92,600 GLC
  per side for operational simplicity — a real, funded quantity people
  can actually type into a transfer, not a fractional-GLC amount. This
  is still a *plan*; nothing has been transferred, no wallet has been
  funded — the "not started, and correctly so" status above is
  unchanged. The reference price is a planning input, not a live oracle
  read — reconfirm it against the actual price at the time of funding
  before sending real value, since a materially different price at
  funding time would change the true dollar exposure without changing
  these GLC quantities.**
  - **This replaces the previous 200,000-GLC-per-side *pilot* funding
    plan referenced two bullets above** ("the approved pilot funding
    amount is materially smaller... order of ~$200 of value per side")
    with an exact number. It does not touch the *full-scale* 200,000-GLC
    figure in the first bullet of this item, which remains the intended
    eventual full-scale target.
  - **Update 2026-08-21: recalculated and approved, no longer just
    flagged.** `protected_minimum` and `rolling_volume_limit` (P0-6
    above) were sized against the *old* 200,000-GLC-per-side pilot
    figure, not this 92,600-GLC-per-side one, so both were recalculated
    proportionally rather than carried forward unchanged:
    - **`protected_minimum`: 50,000 GLC → 20,000 GLC** (raw
      `20000000000`) — kept at roughly the same proportion of the
      reserve (~21.6% vs. the old ~25%), rounded down slightly so a
      reserve this small still has real usable liquidity to run a pilot
      with, rather than mostly sitting locked behind the floor.
    - **`rolling_volume_limit`: 100,000 GLC/24h → 50,000 GLC/24h** (raw
      `50000000000`) — the old value exceeded an entire single-side
      reserve (92,600 GLC) outright, so it could never actually bind;
      50,000 GLC/24h sits comfortably under the resulting 72,600 GLC
      usable liquidity per side (92,600 − 20,000) while still being a
      real, reachable constraint — exactly 5 full 10,000-GLC transfers
      per rolling 24h, `per_transfer_limit`/`min_transfer_amount`
      unchanged.
    See P0-6's table below for the updated values and the exact
    bootstrap-command invocation.

---

## Pilot Launch Policy (2026-08-20, revised: proportional risk model)

**A bounded-reserve pilot may launch before full production readiness
is reached — this is a deliberate, management-approved scope decision,
proportional to what is actually at risk, not an oversight or a general
relaxation of the standards elsewhere in this document.** This is a
~$400 intended-exposure pilot, not an institutional or high-TVL bridge
launch, and this policy applies the standards accordingly: it does not
change any individual finding's technical accuracy elsewhere in this
document, and it does not apply below the pilot's own scale (see
"Explicitly NOT reclassified" and the scale-gate list below).

**The core reasoning:** the pilot's reserve size is its own risk cap.
At roughly $200 of value seeded per side (~$400 total), the intended
maximum exposure is that amount, and — subject to the preconditions in
"Verified failure-mode analysis" below — no mechanism was found by which
a bug or attack could cause loss materially exceeding it. That is an
acceptable amount to risk in exchange for real-world validation that no
audit, by itself, produces. The pilot will be publicly labeled as
unaudited, with published transfer limits and reserve floors, so no user
can reasonably claim they were misled about the system's maturity.

This policy reclassifies every currently-open item into exactly four
categories, applied consistently rather than item-by-item on vibes:

- **A — PILOT LAUNCH BLOCKER**: absolutely necessary before the first
  real pilot transaction. Kept here only where a concrete failure mode
  was identified that could create unbacked assets, double-release/
  replay funds, bypass the reserve/volume controls, compromise assets or
  infrastructure outside this bridge, or make the ~$400 exposure
  assumption false.
- **B — PILOT SAFETY CHECK**: worth doing before launch, but a quick,
  practical action using what already exists — never a new engineering
  project.
- **C — SCALE GATE**: required before materially increasing reserves,
  limits, usage, or public promotion beyond the pilot. Not required for
  the pilot itself.
- **D — POST-LAUNCH IMPROVEMENT**: useful, not necessary for a pilot
  this size.

### A — Pilot launch blockers

1. **At least three genuinely-separate signer endpoints actually
   running, in production mode** (P0-1/P0-2, item 11). The *client
   protocol* is already built and fail-closed
   (`service/src/signing/remote.rs`, `operators.mode`); what remains is
   deployment, not engineering. **The bar at this scale is deliberately
   small**: three different people/environments each running the
   already-built signer process, with no shared credentials between
   them, is sufficient — a formal HSM/KMS vendor selection and
   ceremony is a scale-gate concern (see C below), not a pilot
   precondition, because a compromised signer set is still bounded by
   the reserve balance (see failure-mode analysis).
2. **A conservative Goldcoin confirmation-depth value set** (item 18,
   docs/12 item 4). This is the one item on this list with a *direct*
   attack mechanism up to the reserve size: too-shallow confirmation
   depth lets a Goldcoin deposit be reorged out *after* the
   corresponding Solana-side release has already happened, realizing a
   real loss. The daemon also fails closed with no value set at all, so
   some value is structurally required to start. **Update 2026-08-21:
   done — see [09-runbook.md](09-runbook.md), "Confirmation-depth values
   (pilot, approved 2026-08-21)": `confirmation_depth = 200`,
   `max_reorg_depth = 250`, `vault_min_confirmations = 20`.** These are a
   deliberately conservative number chosen now (trading settlement speed
   for safety margin), explicitly *not* backed by real Goldcoin
   hashrate/historical reorg data — that data-driven refinement remains
   open and is a scale-gate item (C below), not a pilot blocker.
3. **Independent review of the two settlement paths by a reviewer who
   did not write the code, completed before reserves are funded.** Not
   because the failure-mode analysis below is doubted — because the
   entire ~$400 loss-cap claim rests on that one piece of analysis (the
   Solana `transfer_checked` CPI bound and the Goldcoin UTXO-conservation
   bound in "Verified failure-mode analysis" below), and a claim load-
   bearing enough to size real funds against deserves a second set of
   eyes before, not after, funding. Scope: `programs/glc-reserve-bridge/
   src/instructions/release_from_reserve.rs` and `limits.rs`
   (`enforce_protected_minimum`), and `service/src/goldcoin/payout.rs` +
   `service/src/signing/goldcoin_vault.rs` (`verify_payout_tx` and its
   call site) — the same code cited in that analysis, nothing broader.
   This is a reading task, not an audit engagement; it does not require
   external-party engagement.
4. **Reserves actually funded at the intended ~$400 total**, sent
   incrementally and the amounts double-checked before/after each
   transfer — the funding step itself is the one place a human error
   could make the "~$400" assumption false without any bridge-logic bug
   being involved at all (item 28). **Sequenced after item 3 above**,
   not before — funding is exactly the action the independent review
   exists to gate.
5. **A controlled mainnet dry run: a handful of real, minimum-size
   transactions, in both directions, confirmed to settle exactly as
   expected — before normal bridge usage is opened to anyone else.**
   **Update 2026-08-21: this is now the pilot's entire real-world
   validation phase, and it explicitly REPLACES a testnet rehearsal and
   a full-duration soak run for pilot-launch purposes — see "Testnet
   rehearsal and soak testing — not required for pilot launch" below
   for the full decision and reasoning.** Concretely: after funding
   (A-4), before advertising or opening the pilot to any other user, run
   several minimum-size round trips in both directions against real
   mainnet, on the real funded reserves, and confirm each settles with
   the correct amounts, correct fee accrual, and no reconciliation
   breach. This both is the cheapest possible pre-launch check and
   directly serves the pilot's own stated objective — proving the basic
   flow with real usage, at real (if tiny) stakes, is the validation
   this pilot exists to produce.

Five items. None require new engineering — one is a config value already
chosen (A-2), one is a focused code read by a second person (A-3), the
rest are operational/deployment steps.

### Testnet rehearsal and soak testing — not required for pilot launch (decision 2026-08-21)

**Removed from the pilot launch requirements. Not replaced with an
equivalent testing requirement.** Reasoning:

- **No Goldcoin testnet exists to rehearse against** — this is a fact,
  not a scoping choice; see item 25/P1-2 below for the same finding at
  full-production scope, which is unaffected by this decision.
- **The controlled mainnet dry run (A-5 above) is the pilot's real-world
  validation phase**, not a lesser stand-in for one. A small,
  publicly-disclosed pilot with ~$400 of intentionally-bounded exposure,
  settling real minimum-size transactions on real mainnet, produces more
  relevant validation data for THIS bridge than a testnet rehearsal or a
  synthetic soak run against simulated traffic ever could — that is the
  entire premise of running a pilot instead of demanding full readiness
  up front.
- **Deliberately not adding a replacement enterprise-style testing
  program** (e.g. a mandated multi-hour synthetic load campaign, a
  formal rehearsal harness, a scheduled soak-testing cadence) in place
  of the two removed items — that would be exactly the kind of
  disproportionate complexity this pilot's proportional-risk policy
  exists to avoid. The existing automated regression suite (126 on-chain
  + 472 off-chain tests, including the load-harness smoke profile) is
  run routinely already (see "main test dry run," this session,
  2026-08-21 — all green) and remains genuinely useful diligence, but it
  is CI hygiene, not a pilot-launch requirement, and is not being
  elevated into one here.
- **This does not change the full-production-scale findings** — P1-2
  "No broader-network (testnet, multi-node) rehearsal has been
  performed" and P1-3 "No load/soak testing has been performed" below
  remain open, unedited, and still apply once the pilot's own scope is
  exceeded (see the scale-gate list, C, below). Whether a real testnet
  becomes available, or some other rehearsal mechanism is used, is a
  decision for that later point — not asserted here.

### Shortest path to pilot launch

1. Stand up the three separate signer endpoints (A-1).
2. Confirmation depths are already set — no action needed (A-2, done
   2026-08-21).
3. Independent reviewer reads the two settlement paths (A-3) and signs
   off, or raises a finding that changes this analysis.
4. Fund both reserves to the intended ~$400 total (A-4) — only after
   step 3 signs off.
5. Turn on alerting; confirm backups are scheduled; runbook read-through;
   write down the upgrade-authority posture (B).
6. Run the controlled mainnet dry run: a handful of real minimum-size
   transactions in both directions, confirmed to settle correctly (A-5)
   — this is the pilot's validation phase; no separate testnet
   rehearsal or soak run is required or planned.
7. Publish the limits and unaudited status (B).
8. Open normal bridge usage / launch.

### B — Pilot safety checks

- Point the already-built alert webhook (`ops::alerting`) at a real
  channel — one config value, not a monitoring project.
- Confirm `scripts/run-audit-cron.sh` (backup + audit) is actually
  scheduled — the scripts are already built and exercised (item 24).
- One read-through of `docs/09-runbook.md` with whoever will actually
  operate the pilot.
- Write down the pilot's upgrade-authority posture in a short paragraph
  (P0-3, item 13) — even "leave the existing external key in place,
  held offline, activation deferred" is a valid, explicit decision at
  this scale; a compromised upgrade authority is also bounded by the
  reserve (see failure-mode analysis), so full threshold-custody
  activation is a scale-gate item, not a pilot one.
- Confirm the disclosed limits and unaudited status (this policy's own
  premise) are actually published somewhere public before the first
  transaction, not just asserted internally.

### C — Scale gates (required before materially increasing reserves/limits/usage/promotion, not before the pilot)

- **External security audit (P0-4, item 25): reclassified from a pilot
  LAUNCH gate to a SCALE gate.** The audit remains fully required — it
  is a precondition of raising reserves past an explicitly-set
  threshold, not a precondition of the bounded pilot starting. Before
  reserves are increased past that threshold, an external audit is
  required and this policy will be enforced, not merely stated. **The
  threshold itself is not yet set as a specific number in this
  document — recording the exact figure once management sets it is an
  explicit follow-up, not an oversight.**
- Precise Goldcoin confirmation/reorg-depth values from real historical
  hashrate/reorg data, replacing the conservative pilot placeholder
  from A-2 above.
- Extended broader-network/multi-node rehearsal with an observed real
  reorg (P1-2) — the pilot's own first transactions substantially serve
  this purpose at pilot volume; a dedicated multi-day exercise is worth
  doing before meaningfully higher volume, not before the first
  transaction.
- Full custody-domain ceremony and real HSM/KMS vendor selection (P0-2,
  item 11) — upgrading from "three separate lightweight endpoints" (A-1
  above) to institutional custody.
- Full threshold/institutional upgrade-authority activation (P0-3, item
  13), superseding the pilot's documented interim posture (B above).
- Reserve-ledger `target_reserve`/`warning_reserve`/`critical_reserve`
  sizing (docs/12 item 5) based on real observed volume — distinct from
  `protected_minimum`, which is already an approved pilot value.

### D — Post-launch improvements (useful, not necessary at pilot size)

- Richer alert integration (PagerDuty/Slack-specific formatting) and a
  Grafana dashboard on top of the existing `/metrics` (item 21).
- Infrastructure redundancy/HA — no loss mechanism was identified from
  its absence: the daemon *authorizes* movement, so its absence is
  fail-safe (nothing moves), not fail-open (nothing is put at risk by a
  down daemon).
- Docker image build/verification and an orchestration manifest
  (systemd/k8s/compose) beyond a single container (item 23).
- The two dedicated on-chain `rebalance_deposit`/`rebalance_withdraw`
  instructions, as additional structural hardening on top of the
  already-working off-chain evidence trail (P1-1).
- The UI/frontend product decision and rewrite (P1-6, item 26) — not a
  bridge-safety question.
- Reservation TTL and refund/compensation process tuning (docs/12 items
  7-8) from real usage data.

### Explicitly NOT reclassified — unaffected by this policy

The on-chain replay/duplicate-settlement guard (item 20) and the
on-chain limit enforcement (`enforce_transfer_amount`/
`enforce_and_record_rolling_volume`/`enforce_protected_minimum`, item
17) are not on any of the four lists above because they are not open
items — both are already complete, real-node/CI-proven, and load-bearing
for the failure-mode analysis below. This policy does not touch, weaken,
or reinterpret either.

### Verified failure-mode analysis: can a bug cost more than the reserve size?

Checked directly against the current implementation, not assumed. The
program also has no minting capability anywhere in its source
(confirmed by direct search — no `mint_to`/`MintTo` call exists), so no
mechanism exists to create unbacked GLC on either chain.

- **Solana release path**
  (`programs/glc-reserve-bridge/src/instructions/release_from_reserve.rs`):
  the actual token movement is `token_interface::transfer_checked`, the
  real SPL Token/Token-2022 CPI. This is enforced by the token program
  itself at the protocol level — it will fail if `amount` exceeds the
  reserve token account's real on-chain balance, regardless of what this
  bridge's own application logic believes. `enforce_protected_minimum`
  (`programs/glc-reserve-bridge/src/limits.rs`) additionally reads the
  account's live balance (`reserve_token_account.amount`, deserialized
  fresh every call, never cached) as a second, independent layer before
  the transfer is even attempted.
- **Goldcoin payout path** (`service/src/goldcoin/payout.rs`,
  `service/src/signing/goldcoin_vault.rs`): `verify_payout_tx` proves
  `total_in == payout + change + fee` from the transaction's *actual*
  selected vault UTXOs before every real signing call
  (`payout::verify_payout_tx(&unsigned_tx, &plan)?` in
  `signing/goldcoin_vault.rs`, gated to fail closed) — and independent of
  this bridge's own code, Goldcoin's own UTXO-model consensus rules
  reject any transaction whose outputs exceed its inputs.
- **Compromised signer or upgrade-authority key**: bounded the same way.
  A forged attestation still only authorizes what `release_from_reserve`
  independently checks against the real reserve balance. A malicious
  program upgrade cannot move funds from accounts this program was never
  granted authority over — Solana's account-ownership model confines it
  to the one reserve token account this program's PDA actually owns; it
  cannot reach unrelated wallets or infrastructure, and it cannot create
  balance the SPL Token program itself doesn't independently verify.
- **Conclusion, precisely stated (not an unconditional guarantee):**
  given (i) the deployed on-chain binary matches the reviewed source —
  verifiable via the existing deployed-binary-verification method (see
  P0-6), (ii) the conservative Goldcoin confirmation-depth value is set
  (A-2 above, done 2026-08-21), (iii) an independent reviewer who did not
  write this code has read and confirmed the two settlement paths below
  behave as described (A-3 above), and (iv) the funded amount matches the
  intended ~$400 plan (A-4 above) — **no failure mode was found, on
  either chain's settlement path, by which a bug or a compromised
  signer/upgrade-authority key could cause loss exceeding the reserve
  actually held.** A logic bug (replay, forged attestation, miscounted
  limit, accounting drift, etc.) can, at worst, accelerate draining a
  reserve down to zero — not create value beyond what was ever
  deposited, and not reach any asset or account outside the bridge's own
  reserve. This is a structural property of the settlement mechanisms
  themselves (SPL Token CPI enforcement; Goldcoin UTXO conservation; no
  minting anywhere in the program), not an assumption resting on any one
  test passing.
- **One smaller, separate exposure this reserve-size framing does not
  include:** the Solana transaction fee-payer ("submitter") keypair
  (`Config::load_submitter`, explicitly documented as "not a custody
  authority — nothing else derives trust from which key this is") holds
  a small operational SOL float for gas. A compromise of this key can
  drain only that float, not the reserve — keep it minimal as a matter
  of hygiene; it is not counted in the ~$400 figure and does not change
  the analysis above.
- **A distinct, non-monetary risk category worth naming:** a bug could
  cause funds to become *stuck* (require manual operator recovery)
  rather than lost beyond the reserve cap — a liveness/availability
  failure, not a theft. This is a real residual risk the pilot accepts,
  distinct from the loss-ceiling analysis above.

---

# Prioritized roadmap

## P0 — blocks safe production use

### P0-1. No HSM/KMS-backed signer implementation; production keys would have to be plaintext

- **Update 2026-08-15: the local-only half is done.** `VaultSigner`/
  `AttestationSigner` traits now exist (`service/src/signing/signers.rs`),
  `Orchestrator` holds `Vec<Box<dyn VaultSigner>>`/`Vec<Box<dyn
  AttestationSigner>>` rather than concrete `Dev*` types, every signer
  call site is wrapped in an explicit timeout mapped to a distinct
  `SignerError::Timeout` (fail-closed, never silently "signed"), and two
  new adversarial test doubles (`FailingVaultSigner`/
  `FailingAttestationSigner`) prove the rejection and hang/timeout paths
  both actually fire through the new trait boundary. All pre-existing
  "single signer alone cannot authorize" tests pass unchanged. See
  review item 10 above for full detail.
- **Exact problem, now narrowed to just the real backend:** only
  `DevVaultSigner`/`DevAttestationSigner` (in-memory, dev/test key
  material) implement the traits. There is still no code path by which a
  real HSM or cloud KMS could sign on this bridge's behalf.
- **Why it matters:** this bridge's entire security model (docs/02) rests
  on signing keys living in genuinely separate, hardware/policy-protected
  custody domains. Plaintext in-process keys — the only thing that
  exists today — collapse that model back to "whoever can read this
  process's memory or its key files owns both reserves."
- **Current implementation status:** 100% for the abstraction; 0% for any
  real backend. The `config.rs` key-file-path mechanism is a reasonable
  seam toward loading a real backend's configuration, not a substitute
  for the backend itself.
- **What must be implemented:** at least one real backend implementation
  of `VaultSigner`/`AttestationSigner` (a cloud KMS client is the most
  practical first target; a hardware HSM PKCS#11 backend is the higher
  bar) — the trait shape is already exactly what it needs to satisfy.
- **Can be done locally now:** no — needs a real KMS/HSM to develop and
  integration-test against (B).
- **What's needed from you:** which KMS/HSM vendor(s) to target for the
  first real backend (AWS KMS, GCP KMS, a specific HSM appliance, etc.) —
  affects which SDK/protocol the implementation targets.
- **Tests/acceptance criteria:** the real backend, once built, must pass
  the same "single signer alone cannot authorize," "independent
  re-derivation," and timeout/rejection-fails-closed tests the dev
  signers and their test doubles already pass, run against the real
  service.
- **Update 2026-08-18: the real backend now exists.**
  `service/src/signing/remote.rs` — a provider-neutral HTTPS remote
  signer client (`RemoteVaultSigner`/`RemoteAttestationSigner`), plus
  `operators.mode` in `config.rs` gating production vs. dev signer
  loading and structurally refusing to start if the two are mixed. See
  review item 10 for full detail and docs/26-production-signer-
  deployment.md for the operator runbook. **Current implementation
  status: 100% for the client/protocol/production-mode wiring; still 0%
  for any actual custody domain running behind it** — no real vendor
  has been chosen, no endpoint has been deployed, no key-generation
  ceremony has been performed. This item's remaining scope is now
  identical to P0-2 below (an organizational decision, not an
  engineering one) — **Can be done locally now: no (C)**, unchanged in
  substance even though the code-side blocker is gone.

### P0-2. Custody-domain composition is undecided; no key-generation ceremony exists

- **Exact problem:** the 2-of-3 threshold size is approved, but *which*
  three cloud accounts/HSM vendors/personnel constitute genuinely
  separate custody domains for this specific organization has never been
  decided (docs/12 item 2), and no key-generation ceremony procedure is
  documented anywhere.
- **Why it matters:** "2-of-3 threshold" is only as strong as the actual
  independence of the three domains — per docs/02's own honest caveat, if
  all three keys are reachable by the same on-call engineer with the same
  credentials, this degrades to a single point of failure with extra
  steps, regardless of what the architecture diagram claims.
- **Current implementation status:** 0%, purely organizational.
- **What must be implemented:** nothing, code-wise, until decided — this
  is a decision, then a documented ceremony procedure, then P0-1's real
  backend gets pointed at the result.
- **Can be done locally now:** no (C, organizational decision).
- **What's needed from you:** the actual answer — which cloud
  accounts/HSM vendor(s)/personnel will hold each of the three domains,
  for both the attestation-signer group and the Goldcoin vault.
- **Tests/acceptance criteria:** a documented, written ceremony procedure
  that a second person (not the one who ran it) can independently verify
  produced keys with no single point of access to more than one domain.

### P0-3. Program upgrade authority: timelock mechanism built and tested; activation/posture still undecided

- **Update 2026-08-15 (second round): the local-only half is done.**
  `accept_upgrade_authority`/`propose_upgrade`/`execute_upgrade`/
  `cancel_upgrade` (`programs/glc-reserve-bridge/src/instructions/
  upgrade_timelock.rs`) implement docs/12 item 3's recommended interim
  option (c) in full, including a real `bpf_loader_upgradeable` CPI for
  both the authority handoff and the upgrade itself — proven against the
  real native loader program in litesvm, not mocked. See review item 13
  above for complete detail.
- **Exact problem, now narrowed to activation and the underlying
  posture:** the mechanism exists but is inert on any real deployment —
  the program's actual upgrade authority remains whatever
  `anchor deploy`/`solana program deploy` set it to (a single external
  keypair) until `accept_upgrade_authority` is deliberately called with
  that real key. That call, and the prior decision of whether this
  posture (vs. threshold custody or revocation) is the one to use at all,
  have not happened and are not this session's decision to make.
- **Why it matters:** per the threat model's own words, an upgradeable
  program whose upgrade authority isn't under real custody discipline
  "undermines every on-chain control this design relies on." Building
  the tool doesn't change this fact until the tool is actually used.
- **Current implementation status:** 100% for the mechanism; 0% for
  activation on any real deployment (by design — this repository holds
  no production keys to activate it with).
- **What must be implemented:** nothing further, code-wise, for option
  (c). If management instead chooses full threshold custody (option (a)),
  that needs the same real custody infrastructure as P0-1/P0-2, not
  local engineering.
- **Can be done locally now:** the mechanism itself — done (A). Deciding
  and executing activation — no (C, management decision + a real signing
  action with real keys, out of scope for any local session).
- **What's needed from you:** which posture to actually use — this
  timelock (interim, now ready), revoke (maximally safe, zero
  flexibility), or full threshold custody now — and, if this timelock,
  who signs `accept_upgrade_authority` and when.
- **Tests/acceptance criteria (met for the mechanism):** an attempted
  upgrade takes effect only after the configured delay (real-node/litesvm
  test, not merely documented); execution fails closed with a distinct
  error if the authority handoff was never performed; a second
  proposal/execution cannot replay against an already-closed pending-
  upgrade account; cancellation is always safe pre-execution; a fresh,
  unrelated fee payer can execute post-timelock without gaining any
  authority themselves.

### P0-4. External security audit scoped, not yet performed

- **Update 2026-08-15: scoping is done.** docs/23-external-audit-scope.md
  now exists — see review item 25 above for its full contents.
- **Exact problem, now narrowed to just the engagement itself:** the
  on-chain program, the attestation-verification logic, the Goldcoin
  multisig mechanics, and the fee-computation path have never been
  reviewed by anyone outside this codebase's own authorship and test
  suite.
- **Why it matters:** this system will hold and move real funds across
  two chains under a threshold-custody model with one direction's replay
  guard structurally weaker than the other's (item 20 above) — exactly
  the kind of design where an independent reviewer catches what repeated
  self-review cannot, by construction.
- **Current implementation status:** 100% scoped; 0% performed.
- **What must be implemented:** nothing code-side — the remaining work is
  purely engaging and running the audit itself.
- **Can be done locally now:** no (E), must be an external firm.
- **What's needed from you:** budget/timeline for engaging an audit firm,
  and sign-off on docs/23-external-audit-scope.md (or requested changes to
  it) before sending it out.
- **Tests/acceptance criteria:** a completed audit report with findings
  triaged and either fixed or explicitly risk-accepted by management
  before any production-funds decision — this is the actual gate, not a
  code-level test.
- **Update 2026-08-20: reclassified from a pilot launch blocker to a
  scale gate — see "Pilot Launch Policy" above for the full reasoning
  and the verified failure-mode analysis it rests on.** For the bounded
  pilot specifically (~$400 total reserve exposure, publicly labeled
  unaudited), this item no longer blocks launch; it blocks raising
  reserves past an explicitly-set threshold, which is still to be
  recorded as a specific number. **For anything beyond the bounded
  pilot — i.e. full production launch — this item is unchanged: still
  P0, still blocking, still requires a completed external audit before
  any production-funds decision at scale.**

### P0-5. No production parameter values decided (confirmation depths, reserve sizing, rate limits, TTL)

- **Exact problem:** every relevant field (`confirmation_depth`,
  `max_reorg_depth`, `protected_minimum`/`target_reserve`/
  `warning_reserve`/`critical_reserve` per direction, per-transfer and
  rolling-volume limits, `reservation_ttl_secs`) exists in config and is
  correctly enforced — but every value that has ever been set anywhere in
  this codebase is a fast-iteration test value (e.g.
  `required_goldcoin_confirmations: 3`), explicitly not production-safe.
- **Why it matters:** shipping with test-appropriate values would mean,
  concretely, accepting settlement finality far too early for Goldcoin's
  real reorg risk, or reserve floors too thin for real expected volume —
  a silent, not-obviously-visible way to be unsafe even though every
  other control is implemented correctly.
- **Current implementation status:** mechanism 100%, values 0%.
- **What must be implemented:** nothing new, code-wise — the config
  schema already accepts real values; this is purely filling them in
  correctly.
- **Can be done locally now:** no — these need real data (Goldcoin
  hashrate/historical reorg depth, expected real volume) this sandbox
  cannot originate (C, then D to actually write the config).
- **What's needed from you:** the actual numbers, or who owns Goldcoin
  infrastructure operationally and can supply real reorg-depth data;
  expected launch-phase volume for reserve sizing; the joint security/
  product call on rate limits.
- **Tests/acceptance criteria:** a reviewed, checked-in production config
  template (with real values, no secrets) that `config::load` accepts
  without any fail-closed rejection, plus a runbook line confirming who
  signed off on each value.
- **Update 2026-08-20: the on-chain bridge-policy subset of this item is
  now APPROVED for pilot launch.** Every value `initialize` accepts
  (attestation threshold, min/per-transfer amounts, protected minimum,
  rolling-volume limit and window, governance timelock, upgrade
  timelock) has a signed-off pilot value — see the **"Approved pilot
  bridge-policy parameters"** table under P0-6 below for the exact
  human-readable and raw-integer values, and
  `service/src/bin/glc-mainnet-bootstrap.rs`'s module docs for the exact
  future CLI invocation. **This does NOT resolve the rest of this item**
  — Goldcoin `confirmation_depth`/`max_reorg_depth` (docs/12 item 4,
  still needs real Goldcoin hashrate/reorg data), the off-chain service's
  own `reserve.{solana,goldcoin}.{target_reserve,warning_reserve,
  critical_reserve}` (docs/12 item 5 — distinct from the on-chain
  `protected_minimum` just approved; the runbook's reserve-sizing
  formula still needs real expected-volume data to size these), and
  `reservation_ttl_secs` (docs/12 item 7) remain fully open. Status
  updated to: on-chain bridge policy 100% decided; Goldcoin
  confirmation/reorg depth and off-chain reserve-ledger thresholds still
  0%.
- **Update 2026-08-21: `confirmation_depth`/`max_reorg_depth`/
  `vault_min_confirmations` now also have an approved pilot-interim
  value** — see [09-runbook.md](09-runbook.md), "Confirmation-depth
  values (pilot, approved 2026-08-21)." Not the real-data-backed value
  this item originally called for (that stays open, now as a scale
  gate — see "Pilot Launch Policy"), but no longer 0% for pilot-launch
  purposes. `reservation_ttl_secs` and the off-chain
  `reserve.{solana,goldcoin}` thresholds remain fully open and are
  correctly not needed for the pilot (post-launch/scale-gate items, not
  blockers).

### P0-6. Program ID drift: source tracked the scaffold/dev id, not the real deployed mainnet address

- **Discovered 2026-08-19**, while scoping a mainnet-bootstrap tool
  (`glc-mainnet-bootstrap`, itself blocked on this finding — see
  docs/26 or the tool's own docs once it lands).
- **Exact problem:** `declare_id!` in `programs/glc-reserve-bridge/src/
  lib.rs` and `PROGRAM_ID` in `service/src/solana/accounts.rs` were both
  hardcoded to `BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY` — the
  local/scaffold id generated at project scaffold time
  (docs/07-implementation-plan.md Phase 2) — for the program's entire
  life in this repository. The program was, at some point outside this
  repository's own tooling, actually deployed to Solana mainnet at a
  *different* address, `7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn`.
  Neither constant was ever updated to reflect that; the real address
  appeared nowhere in the repository until this fix.
- **Why it matters, precisely (two genuinely different failure modes,
  not one):**
  1. **Off-chain PDA/instruction targeting (the more obviously fatal
     one):** every instruction builder in `service/src/solana/
     instructions.rs` set `Instruction { program_id: PROGRAM_ID, .. }`,
     and every PDA helper in `accounts.rs` derived against the same
     constant. With `PROGRAM_ID` wrong, the off-chain service would
     build transactions naming the WRONG program entirely and compute
     PDAs the real deployed program never validates against — any
     attempt to interact with the real mainnet deployment through this
     service (including the `initialize`/`initialize_reserve_vault`
     bootstrap this was discovered while building) would either hit
     "program account not found" or, in the worst case, silently target
     an unrelated program that happens to exist at the old address.
  2. **On-chain attestation domain separation (the subtler one — see
     verified analysis below):** `crate::ID.to_bytes()` (i.e.
     `declare_id!`'s compile-time value) is baked in as the domain
     separator for every attestation message this program's
     `release_from_reserve`/`complete_goldcoin_payout`/`governance`
     instructions verify (`verification.rs` counterparts). This is
     compiled into the binary and does **not** update itself just
     because the binary is deployed at a different address — Anchor's
     PDA `seeds = [...]` constraint codegen was independently verified
     (against the vendored `anchor-syn 0.31.1` source,
     `codegen/accounts/constraints.rs`) to use the *runtime* program id
     supplied by the Solana loader at instruction-dispatch time, not
     `crate::ID` — so on-chain PDA validation was never actually broken
     by this drift. But the attestation domain separator specifically
     **is** `crate::ID`, and that only changes on a real program
     upgrade with a recompiled binary.
- **What was fixed this round:** a single authoritative source of truth,
  `glc_reserve_bridge_shared::PROGRAM_ID_BYTES` (in `shared/src/lib.rs`,
  the crate already compiled into both the on-chain program and the
  off-chain service), now backs both `declare_id!` (via a same-crate
  regression test, since the macro itself requires its own string
  literal — see that constant's doc comment for why) and `service/src/
  solana/accounts.rs::PROGRAM_ID` (a direct `const` derivation, no
  redundant literal). `Anchor.toml`'s `[programs.localnet]` entry was
  updated to match, with an explicit warning against ever running
  `anchor keys sync` against this repository (that command would
  silently regenerate a fresh local id and overwrite the fix). New tests
  pin the exact expected value in both the on-chain crate
  (`programs/glc-reserve-bridge/src/lib.rs::program_id_tests`) and the
  off-chain crate (`solana::accounts::tests`, `solana::instructions::
  tests`, `signing::attestation::tests`) — see this session's PR for the
  full list.
- **What this fix does NOT do, and the one open question it leaves:**
  it corrects the *source* going forward — a fresh `anchor build` from
  this fixed source now produces a binary whose `declare_id!`/attestation
  domain separator genuinely is `7h2zSJuq...`. It does **not** retroactively
  change whatever binary is *actually already deployed* on mainnet right
  now. Whether the currently-live binary was compiled from source that
  already had `declare_id!("7h2zSJuq...")` (in which case it's already
  correct, and this fix just brings the checked-in source into agreement
  with it) or from source matching the stale `declare_id!("BnCFcMaZ...")`
  this repository held until today (in which case the live binary's
  attestation domain separator is still the OLD value, and a program
  upgrade — recompiling and redeploying with today's fixed source — is
  required before any attestation-gated instruction would work correctly
  against it) **cannot be determined from local source inspection
  alone.** This is the single open question the mainnet-bootstrap work
  is blocked on; resolving it requires comparing this fix's fresh build
  output against the live deployed program's actual on-chain bytecode
  (a read-only RPC comparison) — deliberately not performed as part of
  this fix, which stopped at "build, do not deploy, do not query
  mainnet" per explicit instruction.
- **Current implementation status:** source-level fix 100% done, built
  (not deployed), tested. Live-deployment verification/upgrade: 0%,
  not started, explicitly out of scope for this round.
- **Can be done locally now:** the source fix — done (A). Determining
  whether the live binary needs an upgrade, and performing one if so —
  no (D: needs a read-only mainnet RPC comparison first, then, if
  confirmed necessary, a real signed upgrade transaction from whoever
  holds the real upgrade authority — this repository holds neither).
- **What's needed from you:** approval to perform the read-only
  comparison (fetch the live program's on-chain executable data via RPC,
  hash it, compare against a fresh local build's hash) to determine
  definitively whether an upgrade is required; if it is, who holds the
  real upgrade authority and when they're available to sign it.
- **Tests/acceptance criteria:** `programs/glc-reserve-bridge/src/
  lib.rs::program_id_tests::program_id_matches_shared_source_of_truth`,
  `service`'s `solana::accounts::tests::program_id_is_the_deployed_
  mainnet_address`/`every_pda_helper_derives_against_program_id`,
  `solana::instructions::tests::every_builder_targets_the_deployed_
  mainnet_program_id`, `signing::attestation::tests::
  attestation_domain_separator_is_the_deployed_mainnet_address` — all
  passing means the four independent copies of "the program id" this
  workspace has (declare_id!, the shared constant, the service's PDA/
  instruction constant, the service's attestation domain separator) can
  no longer silently disagree with each other. It does not by itself
  mean the live mainnet binary agrees with any of them — see above.
- **Update 2026-08-19 (read-only mainnet verification): the open
  question above is resolved — no upgrade was needed.** A read-only
  RPC comparison (program account + ProgramData account fetch, header-
  stripped SHA-256 of the deployed executable, cross-checked two
  independent ways — manual byte parsing and `solana program dump` —
  plus a direct scan for both candidate program-id byte sequences
  embedded in the binary) found the live binary already contained the
  CORRECT id (`7h2zSJuq...`) at the identical offset as a fresh local
  build, and the old dev id nowhere. The deployed binary was built from
  some other, never-committed local source that already had the fix;
  this session's change only brought the checked-in repository into
  agreement with what was already live. See the session transcript for
  the full methodology (deployed-vs-local length/hash/embedded-id
  comparison, and the 45-byte `UpgradeableLoaderState::ProgramData`
  header layout independently derived and verified against
  `solana-loader-v3-interface`'s own `size_of_programdata_metadata()`).
  A separate, non-blocking 4-byte executable difference (same length,
  4 bytes differ, unrelated to the program id) was recorded as a
  provenance/reproducibility note, not a launch blocker.
- **Update 2026-08-19 (later the same day): the verified program has
  since been PERMANENTLY CLOSED.** `7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn`
  no longer exists on chain in any form — the program account and its
  ProgramData account were both closed and their rent reclaimed. The
  entire "was an upgrade needed" analysis above is now moot: there is
  nothing left to upgrade. **This program id is permanently retired**
  and must never be interacted with, targeted, or reused for a future
  deployment — `service/src/bin/glc-mainnet-bootstrap.rs`'s own
  `RETIRED_PROGRAM_IDS` constant now enforces this independent of
  whatever `declare_id!`/`accounts::PROGRAM_ID` currently hold (which,
  as of this update, is still this retired id — see below).
  - **This repository's `declare_id!`/`accounts::PROGRAM_ID` were
    deliberately left unchanged** (still the retired id) rather than
    swapped to a guessed placeholder — the real future production
    program id does not exist yet (no `solana-keygen new` has been run
    for it), and this repository does not generate program keypairs
    for itself (see `RETIRED_PROGRAM_IDS`'s own doc comment). Changing
    `declare_id!` again before a real replacement id exists would just
    trade one wrong hardcoded value for another.
  - **The future program-id replacement workflow** (not yet started):
    (1) generate a new Solana program keypair; (2) obtain the new
    program id; (3) update `declare_id!`
    (`programs/glc-reserve-bridge/src/lib.rs`),
    `glc_reserve_bridge_shared::PROGRAM_ID_BYTES` (`shared/src/lib.rs`
    — the single authoritative source both `declare_id!` and
    `accounts::PROGRAM_ID` are checked against), and `Anchor.toml`'s
    `[programs.localnet]` entry — also update the pin-test literals in
    `service/src/solana/accounts.rs`/`instructions.rs` and the
    illustrative address mentioned in `shared/src/lib.rs`'s own doc
    comment (see `scripts/verify-program-id-replacement.sh` below for a
    read-only check that catches anywhere this step was left
    incomplete); (3a) run `scripts/verify-program-id-replacement.sh` —
    it fails closed if either retired id (`7h2zSJuq...`,
    `BnCFcMaZ...`) still appears anywhere in an operational `.rs`/
    `.toml` file outside its one permanent, legitimate home, and if
    `declare_id!` and `Anchor.toml` ever disagree with each other; (4)
    rebuild the on-chain program (`anchor build`); (5) rebuild/retest
    the service; (6) verify
    instruction builders, PDA derivations, and the attestation domain
    separator all use the new id — the exact pin tests this fix added
    (listed in the "Tests/acceptance criteria" bullet above, plus the
    on-chain `program_id_tests`) will need their expected literal
    updated and will otherwise fail closed, which is the mechanism that
    enforces this step actually happened; (7) deploy under the new
    program id; (8) verify the deployed binary (the same read-only
    technique used above); (9) run `glc-mainnet-bootstrap`'s
    simulation against the new id; (10) review all production
    parameters (P0-5, and the six still-unresolved values called out in
    `glc-mainnet-bootstrap`'s own help text); (11) execute
    initialization only after explicit approval.
  - **Current implementation status:** source-level program-id fix from
    the prior update remains correct and complete for whatever id it's
    pointed at; the id itself is retired and awaiting replacement,
    0% started.
  - **What's needed from you:** the new program keypair (step 1) —
    nothing in this repository will generate one.
- **Update 2026-08-20 (deployment-size audit, ahead of the future
  redeployment):** since the next deployment pays fresh rent regardless
  of program id, this was a good moment to audit whether the compiled
  `.so` could be safely shrunk first. Baseline (fully clean `anchor
  build`, reproduced 3 times): 618,024 bytes,
  sha256 `ae873e4b8fd3b6d0a003b23d1b7b72daca54dcfa298158b9b21a64c11dd7ac0b`,
  4.30379256 SOL total rent-exempt requirement (ProgramData + Program
  accounts, current mainnet rate at audit time).
  - **Audited and found already near-optimal, no action taken:** release
    profile (`lto = "fat"`, `codegen-units = 1` — already the aggressive
    end of Cargo's own knobs); `anchor-spl`'s default feature set
    (`associated_token`, `mint`, `token`, `token_2022`,
    `token_2022_extensions` — all five genuinely used, confirmed by
    grepping every `anchor_spl::` call site; the on-chain crate never
    depends on `anchor-spl`'s unused `governance`/`memo`/`metadata`/
    `stake` features in the first place); duplicate dependency versions
    in `cargo tree --duplicates` (all confined to `[dev-dependencies]
    litesvm`, which never links into the SBF build — the on-chain
    `-e normal` dependency edge has no actionable duplication); the
    workspace-split discipline itself (docs/08-migration-strategy.md —
    already guarantees zero service-only code reaches this crate);
    `strip`/debug symbols (`file`/`readelf` on the baseline `.so`
    already show `stripped`, zero `.debug_*` sections — the toolchain
    handles this independent of any Cargo profile setting).
  - **Tested and explicitly rejected:** `opt-level = "z"` (569,424
    bytes, −7.9%) and `opt-level = "s"` (589,464 bytes, −4.6%) both
    measured real size wins, but a representative `release_from_reserve`
    litesvm test went from 40,152 CU at baseline to 64,021 CU (+59.5%)
    and 53,398 CU (+33.0%) respectively — a materially worse per-
    transaction compute-unit cost paid for the program's entire life, in
    exchange for a one-time (and, as this program's own history just
    demonstrated, reclaimable) rent deposit. Not adopted. `panic =
    "abort"` was also tested explicitly (the audit's own requested
    category) and found to be a no-op-to-negative (+112 bytes, no CU
    change) — the SBF target already forces abort-on-panic (eBPF has no
    stack-unwinding support) independent of this Cargo setting; not
    adopted, left unset.
  - **Adopted:** `no-log-ix-name` (Anchor's own built-in feature,
    `programs/glc-reserve-bridge/Cargo.toml`'s `[features] default`) —
    615,520 bytes (−2,504 bytes, −0.41%), 4.28636472 SOL total rent
    (−0.01742784 SOL, −0.40%), **and** slightly lower CU (40,048 vs
    40,152, −0.26%) on the same representative test — a win on both
    axes, not a tradeoff. Removes only the automatic `msg!("Instruction:
    <Name>")` log line Anchor emits at the start of every instruction;
    touches no validation, account-derivation, or signature-verification
    code. Confirmed nothing in this repository (tests, docs, or the
    off-chain service) parses or depends on that log text. Reproduced
    twice via fully clean `anchor build`. All 126 on-chain tests (58
    unit + 51 litesvm integration + 17 shared) pass unchanged; `cargo
    fmt`/`cargo clippy -D warnings` clean.
  - **Recommendation:** adopt `no-log-ix-name` (small, real, genuinely
    free win); do not adopt either `opt-level` setting (real cost,
    modest one-time benefit not worth the recurring CU/fee tax on a
    bridge moving real assets). ~0.41% size/rent reduction is a fair
    ceiling for what's safely available here without touching program
    logic — most of this program's size is genuinely-used Anchor/SPL
    machinery (account (de)serialization, CPI helpers, Token-2022
    extension parsing), not slack.
- **Update 2026-08-20: approved pilot bridge-policy parameters.**
  Every argument `initialize` requires now has a signed-off pilot value
  (cross-referenced from P0-5 above). **These are config/CLI inputs to
  the bootstrap tool at initialization time — nothing in program logic
  or the bootstrap tool's own source hardcodes them; every field below
  remains a required, explicit argument with no built-in default, exactly
  as before.** GLC uses 6 decimals throughout; "raw" is the atomic
  integer value `initialize`/`glc-mainnet-bootstrap` actually take.

  | Parameter | Approved pilot value | Raw (6 decimals) | CLI flag |
  |---|---|---|---|
  | Attestation threshold | 2 of 3 | — (not decimal; `u8`) | `--attestation-threshold` |
  | Minimum transfer | 100 GLC | `100000000` | `--min-transfer-amount` |
  | Maximum single transfer (`per_transfer_limit`) | 10,000 GLC | `10000000000` | `--per-transfer-limit` |
  | Protected minimum | **20,000 GLC** | `20000000000` | `--protected-minimum` |
  | Rolling volume limit | **50,000 GLC** | `50000000000` | `--rolling-volume-limit` |
  | Rolling window | 24 hours | `86400` (seconds) | `--rolling-window-seconds` |
  | Governance timelock | 24 hours | `86400` (seconds) | `--governance-timelock-seconds` |
  | Upgrade timelock | 48 hours | `172800` (seconds) | `--upgrade-timelock-seconds` |

  **Update 2026-08-21: protected minimum and rolling volume limit
  recalculated against the 92,600-GLC-per-side reserve plan (item 28
  below), replacing the values (50,000 GLC / 100,000 GLC) approved
  against the earlier 200,000-GLC-per-side plan.** The old
  `rolling_volume_limit` exceeded an entire single-side reserve outright
  and could never actually bind; the old `protected_minimum` was
  proportionally reasonable (~25% of reserve) but the recalculation
  rounds it down slightly to leave more usable liquidity for a reserve
  this small. Resulting usable/releasable liquidity per side: 92,600 −
  20,000 = **72,600 GLC**. Resulting max full-size transfers per rolling
  24h: 50,000 / 10,000 = **5**. `min_transfer_amount` and
  `per_transfer_limit` are unchanged.

  **Important interpretation, stated explicitly so it isn't re-litigated
  later:** `per_transfer_limit` bounds a single individual transfer, not
  a transaction count or per-user cap. `rolling_volume_limit` bounds
  *total* bridge volume (both directions' contribution to their own
  `RollingVolumeWindow`, per `programs/glc-reserve-bridge/src/limits.rs`)
  across a rolling 24-hour window — it is a volume cap, never a
  transaction-count or unique-user cap. `protected_minimum` means a
  normal release is refused if it would take the relevant reserve below
  20,000 GLC — it is a floor on releases, not a target balance. These
  are pilot-launch values; changing them later goes through this
  program's own governance mechanisms (admin-gated `set_limit` for an
  interim posture, or the timelocked governance path — see
  `instructions::admin`/`instructions::governance` module docs), not a
  redeployment.

  **Type/validation compatibility, checked against both the on-chain
  and bootstrap-tool code as it exists today:** every raw value above
  fits its field's actual type with enormous headroom (`u64` fields —
  `min_transfer_amount`/`per_transfer_limit`/`protected_minimum`/
  `rolling_volume_limit` — max out at ~1.8×10^19; the largest approved
  raw value here is 10^11, nine orders of magnitude below that; `i64`
  timelock/window-seconds fields max out at ~9.2×10^18, and 172,800 is
  trivially within range). `programs/glc-reserve-bridge/src/
  instructions/initialize.rs`'s own runtime checks — `governance_
  timelock_seconds > 0`, `per_transfer_limit > 0`, `rolling_volume_
  limit > 0`, `rolling_window_seconds > 0`, `upgrade_timelock_seconds >
  0` — all pass against these values (none of the approved values are
  zero). `programs/glc-reserve-bridge/src/validation.rs::
  validate_attestation_key_set` (the same rules
  `glc-mainnet-bootstrap`'s own client-side preflight duplicates)
  requires `2 <= threshold <= keys.len() <= 8`; 2-of-3 satisfies this
  with room to spare. No relationship is enforced on-chain between
  `min_transfer_amount` and `per_transfer_limit` (100 GLC < 10,000 GLC
  holds regardless). **No validation gap was found — no code change was
  needed or made to accommodate these values**, so none was made; only
  documentation changed.

  **Exact future simulation-only bootstrap command** (do not run until a
  real production program id exists — `<NEW_PRODUCTION_PROGRAM_ID>` is
  a placeholder, never the retired
  `7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn`; `--execute` is
  deliberately absent — simulation-only is this tool's default, and
  broadcasting requires that flag explicitly, per-instruction, only
  after its own simulation succeeds):

  ```
  glc-mainnet-bootstrap \
    --rpc-url https://api.mainnet-beta.solana.com \
    --program-id <NEW_PRODUCTION_PROGRAM_ID> \
    --deployer-keypair /path/to/your/deployer-keypair.json \
    --reserve-mint Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump \
    --token-program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb \
    --attestation-keys 6b27qC3fxrReuU4hL6u8iZ9AwkdngnjDxXUPwicR8WLe,G7dJ2HiEkcfJqtPGa8gQrErLaQfdZ7hcbnA173A8Y4yL,4uYKxwpWrPDyoaxjmdmJoWYLxmq2AziNMctSjTDFmynT \
    --attestation-threshold 2 \
    --min-transfer-amount 100000000 \
    --per-transfer-limit 10000000000 \
    --protected-minimum 20000000000 \
    --rolling-volume-limit 50000000000 \
    --rolling-window-seconds 86400 \
    --governance-timelock-seconds 86400 \
    --upgrade-timelock-seconds 172800
  ```

  This command will still refuse today: `<NEW_PRODUCTION_PROGRAM_ID>`
  isn't a real value, and even a real one would need to not equal
  `RETIRED_PROGRAM_IDS`' one entry and match whatever this build's
  compiled-in `accounts::PROGRAM_ID` holds at that time (the future
  program-id replacement workflow, unchanged from the update above).
  This is expected, not a defect — it will start working once, and
  only once, steps 1–8 of that workflow are actually done.

## P1 — required before production launch

### P1-1. Rebalancing (closed this round — see below)

- **Update 2026-08-15: closed, on a reconsidered design — see review item
  15 above.** The off-chain engineering layer (imbalance detection,
  propose/approve/execute-evidence/confirm state machine, structural
  separation from settlement accounting, replay protection via a UNIQUE
  `tx_reference`, reconciliation interaction, restart recovery, full
  audit trail, `glc-admin rebalance-*` CLI) is built and tested. Per this
  round's explicit instruction not to let this service autonomously move
  production funds, the design intentionally never constructs, signs, or
  broadcasts a fund-moving transaction — it records evidence of a
  transfer already executed through real custody tooling. The dedicated
  on-chain `rebalance_deposit`/`rebalance_withdraw` instructions
  docs/03-architecture.md originally envisioned remain unbuilt, but this
  is no longer a 0%-implemented gap blocking safe operation — it is now
  an optional future hardening step (see the closing note below).
- **Remaining optional hardening:** the two on-chain instructions, if
  ever wanted, would add an atomic, on-chain-enforced structural
  separation between a rebalance transfer and an arbitrary one, on top of
  the off-chain evidence trail that already exists.
- **Can be done locally now:** yes (A), if/when wanted — not blocking.
- **What's needed from you:** nothing to start the off-chain layer (done);
  eventually, whether the on-chain hardening step above is worth building
  at all, and the authorization policy for who can approve a rebalance
  (currently a configurable `required_approvals` count per request, not
  yet tied to a specific named custody-domain policy).
- **Tests/acceptance criteria (met):** 10 unit tests proving a rebalance
  never touches `reserved_liquidity`/`pending_obligations`/
  `bridge_requests`/`settled_liquidity`; a reconciliation test proving a
  confirmed rebalance is never misclassified as an unexplained breach; a
  3-stage restart-recovery test.

### P1-2. Broader-network rehearsal has never been performed

- **Exact problem:** every real-node test to date runs against a single
  local `goldcoind` regtest node and a single local `solana-test-validator`
  — no multi-node, no real network latency/partition behavior, no real
  Goldcoin peer-to-peer propagation, no real testnet.
- **Why it matters:** single-node rehearsal cannot surface real-network
  failure modes (peer disagreement, propagation delay, a genuinely
  contested reorg) that a production deployment will eventually face.
- **Current implementation status:** 0% beyond single-node.
- **What must be implemented:** nothing new code-wise necessarily — this
  is a testing/rehearsal exercise against real testnet infrastructure
  (Solana devnet + Goldcoin testnet, if one exists) using the same
  orchestrator/service code already built.
- **Can be done locally now:** no — needs real testnet infrastructure and
  time (B).
- **What's needed from you:** access to/willingness to fund testnet
  operation (testnet SOL/GLC, even though valueless, still needs an
  actual endpoint and possibly a faucet or existing testnet balance).
- **Tests/acceptance criteria:** the same Phase 6 acceptance matrix
  (docs/13), re-run against real testnet endpoints instead of localnet,
  including at least one real observed reorg if the testnet in use
  produces one during the rehearsal window.

### P1-3. No load/soak testing has been performed

- **Exact problem:** every test to date is single- or dual-request;
  nothing exercises sustained concurrent bidirectional traffic over
  hours.
- **Why it matters:** accounting-drift bugs and reconciliation-timing
  bugs are exactly the class of defect that tends to only appear under
  sustained real load, not a handful of sequential test requests — this
  was explicitly named as a rehearsal-suite requirement (docs/11 item 4)
  and has never been run.
- **Current implementation status:** 0%.
- **What must be implemented:** a load-generation harness (can reuse the
  existing real-node test infrastructure, driven for hours instead of
  seconds) plus explicit accounting-drift assertions across the run.
- **Can be done locally now:** partially — the harness itself, yes (A);
  a truly multi-hour soak run benefits from real infrastructure /B) so
  it doesn't compete with other sandbox usage, but isn't strictly
  blocked on anything external.
- **What's needed from you:** nothing to start; how long a soak run is
  considered sufficient before launch is a judgment call worth stating
  explicitly.
- **Tests/acceptance criteria:** a multi-hour sustained bidirectional run
  against real regtest/localnet infrastructure with zero accounting
  drift and zero missed reconciliation breach.

### P1-4. Dedicated post-finality-reorg detection (closed this round — see below)

- **Update 2026-08-15: closed — see review item 6 above.**
  `Ledger::detect_post_finality_reorg`/`record_post_finality_reorg` plus a
  new `TickOutcome::PostFinalityReorgHalted` path in
  `goldcoin::indexer::Indexer::tick()` implement exactly the dedicated,
  distinguishable path this item called for: a specific audit event
  (`post_finality_reorg_events`, distinct from the generic reconciliation
  breach table), an unconditional global (both-reserves) pause, and a
  distinguishable `TickOutcome` variant an operator or future alerting
  rule can key off directly rather than inferring from a generic balance
  delta.
- **Tests/acceptance criteria (met, adjusted from the original real-node
  requirement):** covered by unit tests exercising the detection query
  and the pause/audit-event write directly, an indexer-level test
  confirming the dedicated `TickOutcome` fires instead of the old generic
  rollback path for a `SourceFinalized` request whose block is reorged,
  and a restart-recovery test proving the persisted pause survives a
  crash. A live `invalidateblock`-driven real-node run past finality
  depth (the originally specified acceptance bar) was not additionally
  performed this round — the unit-level coverage above directly exercises
  the same code path `service/tests/regtest_acceptance.rs`'s real-node
  suite would reach, so this is a reasonable-but-not-identical substitute
  worth noting explicitly rather than silently claiming as equivalent.

### P1-5. Attestation-key-rotation/vault-sweep tooling built; real-node rehearsal still pending

- **Update 2026-08-15: the generic off-chain tooling is now built —
  see review item 11 above.** A unified `custody_transitions` state
  machine (`Proposed -> IdentityVerified -> Approved -> Executed ->
  Confirmed`, off-ramps `Rejected`/`Cancelled`/`Failed`/`RolledBack`)
  covers both `AttestationKeyRotation` and `GoldcoinVaultSweep` with the
  same shape, plus two gates rebalancing's tooling didn't need: approvals
  cannot begin until the new identity is independently verified
  (`verify_new_identity` is a required precondition), and execution
  evidence cannot be recorded until the relevant reserve(s) are already
  paused (enforced in `record_custody_transition_executed` — Goldcoin
  alone for a vault sweep, both reserves for an attestation rotation).
  10 `glc-admin custody-*` subcommands, mirroring the staged-approval CLI
  flow this item called for. Like rebalancing, it never generates keys,
  signs anything, or performs a rotation/sweep itself — only records
  evidence of one already executed out of band.
- **What's still missing (unchanged from the original item):** a
  rehearsal actually exercising this tooling end-to-end against a real
  program deployment (rotating attestation keys, confirming old keys stop
  working post-rotation, no in-flight settlement lost — docs/11 rehearsal
  item 2) and a Goldcoin vault sweep rehearsal (docs/11 item 3) have not
  been run; both remain real-node/B-classified work once the tooling
  itself (now A-complete) is available to rehearse against.
- **Tests/acceptance criteria (met for the tooling itself):** unit tests
  covering the full lifecycle for both transition kinds, the
  identity-verification gate, the pause-precondition enforcement (as an
  actual returned error, not just documentation), duplicate-`tx_reference`
  replay rejection, and a restart-recovery test across all three
  crash points (mid-approval, post-execution, post-confirmation).

### P1-6. UI/API gap: backend read-projection work done; federation-shaped product decision is the entire remainder

- **Update 2026-08-15 (second round): the A-classified engineering work
  is done — see item 9 above.** `GET /stats`, `GET /reserves/history`,
  `GET /explorer/events`, and `GET /transfers` (wallet-scoped listing)
  are all built, tested (empty-database, malformed-pagination,
  maximum-limit, and restart/persistence cases), and deliberately built
  around this bridge's own real vocabulary rather than the frontend's
  federation/wrapped-token one.
- **Exact problem, now narrowed to the one thing left:** the connected
  frontend's actual client/schema code (inspected directly this round,
  `src/lib/api/`) is built against a fundamentally different bridge model
  — wrapped supply, `glc-to-wglc` direction naming, `/federation`,
  mint/burn explorer events — that no amount of additional backend
  endpoint-building can close, because the mismatch is conceptual, not a
  missing field. This is now genuinely the entire remaining gap in this
  item.
- **Why it matters:** a bridge with no usable UI is not launched in any
  meaningful sense, regardless of how correct the backend is.
- **Current implementation status:** 11 endpoints exist (up from 7 at the
  first round, 6 originally); every endpoint this review or the frontend's
  read-projection needs that doesn't require a product decision is built.
- **What must be implemented:** nothing further on the backend read-only
  API. What remains is entirely: (1) the federation-shaped-endpoint
  product decision, then (2) the frontend rewrite/adaptation itself
  (reworking `src/lib/api/schemas/*`, direction naming, and the explorer
  event vocabulary to match this bridge's real model) — a frontend
  engineering task, not a backend one, and out of this repository's scope.
- **Can be done locally now:** no further backend work is blocked or
  pending; the frontend adaptation work is real engineering effort but
  needs the product decision first.
- **What's needed from you:** the product decision on
  `/federation`/`/federation/rounds` — reinterpret as the 3 custody
  domains with UI copy changes, or drop entirely and adjust the frontend
  — which then unblocks scoping the actual frontend rewrite.
- **Tests/acceptance criteria:** the frontend, pointed at this bridge's
  real API instead of its mock mode, renders every page without a
  network error; a UI-level smoke test (even manual) walking through a
  full transfer end-to-end against real regtest/localnet infrastructure.

---

# Completeness percentages (evidence-based, not inflated by mock/unit-test presence alone)

| Area | Estimate | Basis |
|---|---|---|
| Core bridge software | **~94%** | Both directions' full transactional logic, state machine, replay protection, reconciliation (both directions), reserve-capacity/fee accounting, rebalancing's off-chain engineering layer, and dedicated post-finality-reorg protection are all real, tested, and internally consistent. The gap to higher: the late-deposit-after-expiry auto-recreate behavior (still undocumented-as-missing), and the still-unbuilt (now optional-hardening, not blocking) on-chain rebalance instructions. |
| Token-2022 compatibility | **~95%** | Unchanged this round. Technically complete and real-node/real-mint verified end to end. The remaining 5% is procedural (re-verify the live mint's extension set immediately before any actual mainnet deploy — state could theoretically change), not a code gap. |
| Accounting/fee system | **~95%** | Unchanged this round. Comprehensive: canonical units, fee formula, capacity fix, accrued-fee tracking/surfacing, fail-closed tamper detection, full test matrix, documentation. Deliberately deferred (not counted against this number since explicitly out of scope): fee-withdrawal/treasury path, business-minimum-transfer policy. |
| Test/rehearsal completeness | **~81%** | Real-node happy paths (both directions), double-release, crash/restart, reconciliation, the full fee/accounting matrix, signer-timeout/rejection, rebalancing, custody-transition, post-finality-reorg suites, the new API pagination/empty-database/malformed-input matrix, and now 12 real-CPI litesvm tests for the upgrade-timelock mechanism (including a genuine end-to-end authority handoff and code-upgrade CPI, not a mocked one) all pass. Missing: multi-node/testnet rehearsal, load/soak testing, signer-loss and `record_goldcoin_completion` real-node coverage, and real-node key-rotation/vault-sweep rehearsal (docs/11 items 2-3, tooling exists, rehearsal not run). |
| Production operational readiness | **~63%** | Unchanged this round — the upgrade-timelock and API work don't add operational tooling beyond what's already counted here. Daemon, config loading, both-direction reconciliation, CI, dependency hygiene, a written (unverified-build) Dockerfile, basic webhook alerting, tested backup/restore, and a fully built rebalancing/custody-transition operator toolchain all exist. Missing: HSM/KMS, a verified container build, a dashboard, broader-network rehearsal. |
| Security/custody readiness | **~50%** | The cryptographic/protocol design remains genuinely sound and real-node-tested. A real, real-CPI-tested timelocked-upgrade mechanism (docs/12 item 3 option (c)) is ready to activate on a real deployment, though not yet activated on any. **New 2026-08-18**: the real HSM/KMS-equivalent signer backend now exists — a provider-neutral HTTPS remote signer client with production-mode fail-closed gating against local dev signers, local signature verification of every response, and 18+ tests (docs/22 item 10, docs/26). This closes the pure-engineering half of P0-1. Still genuinely untouched, and now the entire remainder of this number: custody-domain composition decision (0%), any real signer endpoint actually deployed/operated (0%), key-generation ceremony (0%), whether/when to activate the upgrade timelock (0%), external audit performance (0%, though scoped). |
| API/backend readiness | **~55%** | 11 working, tested endpoints (up from 7, 6 originally) — every read-projection this review identified as buildable without a product/architecture decision is now built: aggregate stats, real reconciliation-tick history, a public settlement-event feed, and wallet-scoped transfer listing, on top of the existing status/limits/reserve/health/quote/transfer surface. What remains is entirely the federation-shaped-endpoint product decision (item 9) — not further backend engineering. |
| UI readiness | **~15%** | Unchanged — a real frontend exists but runs entirely on mocks, confirmed this round via direct inspection of its API client/schema code (not just file timestamps): it targets a wrapped-token/federated bridge model this reserve-backed design does not have, a conceptual mismatch no backend endpoint can close (see item 26). |
| **Overall mainnet readiness** | **~68%** | Two rounds of genuine additional engineering (signer abstraction, rebalancing, post-finality-reorg protection, custody-transition tooling, the full read-only API, and now a real, tested upgrade-authority timelock mechanism) on top of docs/15's ~25% baseline — but mainnet readiness is still gated by the same organizational/infrastructure items no local session can close: real custody infrastructure, the upgrade-authority posture *decision* (mechanism ready either way), an external audit actually performed, and a real UI. |

---

# 1. CURRENTLY COMPLETE

- Both directions' full state machine, including every terminal/error
  state, idempotent everywhere, real-node crash/restart verified.
- Real Solana Token-2022 GLC compatibility, verified against the actual
  live mainnet mint's real shape (extensions, decimals, authorities).
- Goldcoin 8-decimal / Solana 6-decimal conversion — canonical, exact-or-
  rejected, no hardcoded decimals anywhere.
- 1% bridge fee — formula, canonical accounting unit, fail-closed tamper
  detection at every settlement site, accrued-fee tracking and surfacing,
  server-authoritative quote endpoint.
- Reserve-capacity accounting — net-destination-based, correct units,
  the previously-flagged gap closed.
- Reconciliation, both directions — fail-closed, auto-pause,
  never-auto-clear, real-node verified.
- The long-running daemon — config-driven, backoff-aware, clean shutdown,
  real-node verified.
- Pause/unpause/emergency controls — three scopes, on-chain and local,
  asymmetric fast-pause/slow-resume by design.
- Crash/restart recovery and replay/duplicate-settlement protection
  (with the SOL→GLC asymmetry explicitly named, not hidden).
- Audit tooling, backup/restore tooling, basic CI and dependency hygiene,
  basic webhook alerting.
- Token issuance safety — structurally, not just behaviorally, confirmed:
  no mint/burn/wrap code path exists anywhere on-chain or off-chain.
- **New in the first follow-on round:** the production signer trait
  abstraction (`VaultSigner`/`AttestationSigner`, timeout-safe,
  fail-closed); the rebalancing off-chain engineering layer (never
  mints/burns/wraps/moves funds itself; records evidence only); dedicated
  post-finality-reorg detection with a distinguishable global auto-pause;
  generic key-rotation/vault-sweep custody-transition tooling with an
  enforced identity-verification gate and an enforced pause-precondition;
  an expanded read-only bridge API (`GET /health`, per-direction
  availability, the fee rate, confirmation-progress data); a scoped
  external-security-audit document (docs/23).
- **New in the second follow-on round:** the remaining read-only API
  (`GET /stats`, `GET /reserves/history`, `GET /explorer/events`,
  `GET /transfers`) — 11 endpoints total, cursor-paginated, deterministic,
  bounded, never fabricating data; a real, real-CPI-tested timelocked
  program-upgrade mechanism (`accept_upgrade_authority`/`propose_upgrade`/
  `execute_upgrade`/`cancel_upgrade`) implementing docs/12 item 3's
  recommended interim posture, shipped inert until a real deployment
  deliberately activates it.

# 2. REMAINING P0

1. **Narrowed:** the real HSM/KMS-backed signer *implementation* — the
   trait abstraction it plugs into is now done (P0-1 above).
2. Custody-domain composition decision + real key-generation ceremony —
   unchanged.
3. **Narrowed:** the program upgrade-authority *posture decision* and
   *activation* — the timelock mechanism itself is now built and tested
   (P0-3 above); what's left is deciding whether to use it at all, and if
   so, performing the one-time `accept_upgrade_authority` handoff with a
   real key.
4. **Narrowed:** actually engaging and running the external security
   audit — it is now scoped (docs/23), just not performed (P0-4 above).
5. Real production parameter values (confirmation depths, reserve sizing,
   rate limits, reservation TTL) — mechanism exists, values don't;
   unchanged.

# 3. REMAINING P1

1. ~~Rebalancing~~ — **closed this round** (off-chain engineering layer
   built; the on-chain instructions are now optional future hardening,
   not a blocking gap — see P1-1 above).
2. Broader-network (testnet, multi-node) rehearsal — unchanged.
3. Load/soak testing — unchanged.
4. ~~Dedicated post-finality-reorg detection/auto-pause path~~ — **closed
   this round** (see P1-4 above).
5. **Narrowed:** the generic staged-approval tooling for attestation-key
   rotation and Goldcoin vault sweep is now built; what remains is
   actually rehearsing it end-to-end against a real program deployment
   (see P1-5 above).
6. **Narrowed to a pure product decision:** the backend read-only API
   gap is fully closed (11 endpoints, see P1-6 above) — what remains is
   entirely the federation-shaped-endpoint product decision and the
   frontend rewrite/adaptation itself, neither of which is backend
   engineering work.

# 4. MANAGEMENT DECISIONS REQUIRED

1. Custody-domain composition (which cloud accounts/HSM vendor(s)/
   personnel for the 3 domains, both reserves).
2. Program upgrade-authority posture (timelock / revoke / threshold) —
   **narrowed**: the timelock mechanism is built and ready; the decision
   is now specifically whether to activate it (call
   `accept_upgrade_authority` with the real deploy key) versus choosing
   revoke or full threshold custody instead.
3. Production values: confirmation depths, reserve sizing, rate limits,
   reservation TTL.
4. External audit engagement (firm, budget, timeline).
5. Refund/compensation process for permanently-`Failed` requests
   (docs/12 item 8, still open, not blocking core settlement).
6. Fee-withdrawal/treasury design, if and when accrued fees should ever
   be moved anywhere (currently deliberately unimplemented).
7. Rate-limit values specifically (joint security/product call, not pure
   security).
8. Federation-shaped-endpoint resolution for the frontend (product
   framing decision).

# 5. INFORMATION/VALUES REQUIRED FROM ME

1. Which HSM/KMS vendor(s) to target for the first real signer backend.
2. The actual custody-domain composition (see above).
3. Real Goldcoin reorg-depth/hashrate data (or who owns that
   operationally) for confirmation-depth values.
4. Expected launch-phase volume for reserve sizing.
5. Chosen upgrade-authority posture.
6. Audit firm/budget/timeline once you're ready to engage one.
7. Decision on the frontend's federation-shaped endpoints.
8. How long a pre-launch soak-test run should be considered sufficient.

# 6. REAL INFRASTRUCTURE REQUIRED

1. A real HSM or cloud KMS instance (development + production).
2. Real Goldcoin testnet and Solana devnet access for broader-network
   rehearsal.
3. Sustained (multi-hour) compute for load/soak testing, ideally not
   competing with other sandbox usage.
4. Docker daemon access somewhere to actually verify the written
   Dockerfile builds (this sandbox has none).
5. Eventually: real, funded Goldcoin and Solana reserve wallets — but
   only after custody (P0-1/P0-2) exists; explicitly not yet.

# 7. EXTERNAL AUDIT REQUIREMENTS

An external, third-party security audit covering at minimum: the
on-chain program (replay guard, solvency checks, pause/limit
enforcement, Token-2022 extension allowlist), the attestation-
verification and independent-re-derivation logic, the Goldcoin P2SH
multisig vault/payout-construction mechanics, the 1% fee computation and
its fail-closed tamper-detection design, and (new since the original
review) the signing/custody boundary, rebalancing, and key-rotation/
vault-sweep tooling — with the SOL→GLC direction's non-cryptographic
replay guard explicitly flagged as a known structural asymmetry for the
auditor's own independent judgment, not asserted as safe by this
repository's own authors. **Update 2026-08-15: fully scoped, still not
performed.** docs/23-external-audit-scope.md is the complete scope
document (21 in-scope areas, explicit out-of-scope list, and a table
separating what's reviewable today from what needs docs/12's still-open
decisions first) — ready to hand to a firm, not a starting draft that
still needs work.

# 8. WHAT CAN BE IMPLEMENTED NEXT WITHOUT MY INPUT

Done across both follow-on rounds (removed from this list, see §1): the
HSM/KMS signer trait abstraction, rebalancing's off-chain engineering
layer, dedicated post-finality-reorg detection, the staged
custody-transition CLI flow/tooling for attestation-key rotation and
Goldcoin vault sweep, the full read-only bridge API (`/health`, `/stats`,
`/reserves/history`, `/explorer/events`, `/transfers`), the
audit-scoping document, and the timelocked program-upgrade mechanism.

Still open, still local-only, still doable without further input:

- The late-deposit-after-`Expired` auto-recreate behavior
  docs/04-state-machines.md describes but nothing implements yet.
- A Grafana dashboard definition on top of the existing `/metrics`
  endpoint, and a Slack/PagerDuty-specific webhook formatter.
- Real-node rehearsals of the now-built rebalancing and
  custody-transition tooling (docs/11 items 2-3) — the tooling itself no
  longer blocks this, only the rehearsal run does.
- The runbook's cold-start-sequencing documentation fix (item 7 above).
- A real testnet/multi-node rehearsal harness (P1-2) and a load/soak
  test harness (P1-3) — the harness scaffolding itself is local-only work
  even though the actual multi-hour/multi-node run needs real
  infrastructure to execute against.

Nothing else remains that is both local-only and not already done — the
rest of the outstanding list (§2 REMAINING P0) is entirely organizational
decisions or real infrastructure this or any local session cannot supply.

# 9. RECOMMENDED NEXT DEVELOPMENT STEP

Every item this document has so far recommended as independently
completable right now — the signer trait abstraction, the audit-scoping
document, rebalancing, the full read-only API, and the upgrade-authority
timelock mechanism — is **done as of this update**. What remains in §8
above is smaller-value housekeeping (the late-deposit auto-recreate
behavior, a dashboard/alerting formatter, a runbook doc fix) or harness
scaffolding for rehearsals that still need real infrastructure to
actually run. None of it is blocking in the way the prior items were.

Given that, the honest recommendation changes shape: **the highest-value
remaining work is no longer local-only engineering — it is the
organizational/infrastructure track this session was never able to touch.**
Specifically, in priority order: (1) decide and execute the
custody-domain composition (P0-2) — this blocks the real HSM/KMS signer
backend, which blocks any real fund custody at all; (2) decide the
upgrade-authority posture and, if the built timelock is the chosen one,
perform the one-time `accept_upgrade_authority` handoff (P0-3) — the
mechanism is ready and waiting; (3) engage an external audit firm against
docs/23-external-audit-scope.md (P0-4) — the scope document is complete
and this is now purely a scheduling/budget action; (4) supply real
production parameter values (P0-5) once (1)-(3) give a stable enough
picture of the deployment shape to size against. The remaining local-only
items in §8 are worth doing in parallel — they carry no dependency on
(1)-(4) — but none of them individually unblocks mainnet readiness the
way an organizational decision does at this point.

# 10. WHETHER THE REPOSITORY IS READY TO PUSH TO YOUR FORK FOR REVIEW

**Yes, for review — not for production.** Pushing to your own fork for
your own inspection carries none of the risks the standing constraints
are guarding against (no funds move, no mainnet interaction, no
production keys involved in a `git push` to a fork you control). The
codebase is in a genuinely strong, well-tested, honestly-documented state
that is reasonable to show a reviewer today. What it is **not** ready for
is any production-funds decision, any real deployment, or any external
party's use — the P0 list above (custody/HSM, upgrade authority, external
audit, production parameters) stands entirely between this state and
that one, and none of those five items are things a push itself would
advance or risk either way. Recommend pushing now if the goal is getting
eyes on the work; continue treating every item in this document's P0/P1
lists as still fully open regardless of that push.
