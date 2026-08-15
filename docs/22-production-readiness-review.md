# Consolidated production-readiness review

Originally performed 2026-08-15, read-only against the repository state at
that time plus every prior checkpoint (docs/00 through docs/21).
**Updated 2026-08-15** after a follow-on implementation round (the same
day) that closed six of this review's own local-only (`A`-classified)
items: the production signer trait abstraction, an off-chain rebalancing
engineering layer, dedicated post-finality-reorg protection, off-chain
key-rotation/vault-sweep tooling, an expanded read-only bridge API, and
this review's own recommended external-audit scope document
(docs/23-external-audit-scope.md). Every section below is marked either
unchanged or updated inline; nothing was silently re-scored without a
stated reason. Scope: everything in `programs/`, `service/`, `shared/`,
`docs/`, `tests/`, `docker/`, `scripts/`, `.github/`, plus a read-only
timestamp/content check of the connected frontend repository at
`/home/reaper/glc-solana-bridge-ui` for the UI-readiness question only
(not modified, not part of this repository).

Code WAS changed to produce this update (unlike the original review) —
see the six commits this round added on top of `main`, all local, none
pushed. No production keys were generated or used. No funds were moved.
Nothing was deployed. No mainnet transaction was submitted. Nothing was
pushed or opened as a PR.

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

**Status: improved this round, still materially incomplete relative to
the existing frontend's expectations.** Current surface: `GET /status`
(now including per-direction `glc_to_sol_available`/`sol_to_glc_available`,
derived from pause state and destination-reserve capacity), `/limits`
(now including `bridge_fee_bps` so the fixed 1% rate is discoverable
without a quote round trip), `/reserve`, `/health` (new — a small,
non-sensitive operational-health summary: indexer-halted flag,
manual-review backlog count, post-finality-reorg event count),
`/transfers/:id` (now including `required_source_confirmations` for
GLC→SOL confirmation-progress display), `POST /transfers`, `POST /quote`
(7 endpoints total, `/health` new this round; the other six gained
fields). Still missing, unchanged from the original review: `/stats`,
`/explorer/events`, `/federation`, `/federation/rounds`, `/incidents`,
`/reserves/history`, `/verify`. The connected frontend repository
(`/home/reaper/glc-solana-bridge-ui`) was not re-checked this round but
was confirmed **completely unchanged** since before docs/15's audit as of
the original review — still running entirely on mock fixtures, still
carrying genuinely federation-shaped client code (`getFederation`,
`listSigningRounds`) and a `glc-to-wglc` (wrapped-GLC) comment that do not
map onto this reserve bridge's actual model — this remains a real
product/UI decision to resolve (reinterpret as the 3 internal custody
domains, or drop), not an engineering ambiguity, and this round's API work
did not attempt to resolve it.
**Classification: A** for the remaining endpoints that are pure
read-projections of already-existing ledger/reconciliation data
(`/stats`, `/reserves/history`, `/explorer/events` if scoped to
`bridge_request_state_log`); **C** for whether/how to reinterpret
`/federation`-shaped endpoints, since that's a product framing decision,
not a technical one.

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

## 13. Program upgrade authority

**Status: unresolved, matches the threat model's own stated top concern.**
No on-chain instruction manages, rotates, or timelocks the program's
upgrade authority — it is whatever `anchor deploy`/`solana program deploy`
set it to, by default a single unprotected keypair. docs/12 item 3 names
three options (threshold custody, revoke entirely, timelock) and
recommends timelock as an interim posture; **none is implemented.** Per
the threat model's own words, an upgradeable program whose upgrade
authority isn't under the same custody discipline as the reserve keys
"undermines every on-chain control this design relies on."
**Classification: C** (which posture — revoke/timelock/threshold — is a
management decision) then **A/B** to implement it (a timelock wrapper is
local engineering work; migrating upgrade authority to a real multisig/
threshold scheme needs the same real custody infrastructure as item 11).

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

## 26. UI/backend integration readiness

**Status: ~15%, unchanged since docs/17.** Confirmed via file-timestamp
check that the connected frontend repository has not been touched since
before docs/15's audit — it still runs entirely on mock fixtures. See
item 9 for the exact endpoint gap and the federation-shaped-endpoint
product decision this blocks on.

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

### P0-3. Program upgrade authority is unresolved (single unprotected keypair by default)

- **Exact problem:** no on-chain mechanism manages, timelocks, or
  thresholds the Solana program's upgrade authority. Left as-is at
  deployment, it is a single keypair with the power to replace the
  entire program's logic — including bypassing every on-chain check this
  design relies on (replay guard, solvency check, pause enforcement) —
  instantly and unilaterally.
- **Why it matters:** per the threat model's own words, this
  "undermines every on-chain control this design relies on." An audited,
  correct, threshold-custodied reserve program is meaningless if a single
  key can silently replace it.
- **Current implementation status:** 0%. docs/12 item 3 names three
  options (threshold custody, revoke entirely, timelock) and recommends
  timelock as an interim posture; none is implemented.
- **What must be implemented:** depends on the chosen posture — a
  timelock wrapper (a PDA holding upgrade authority, gated by a
  mandatory delay before any upgrade takes effect, mirroring the existing
  governance-timelock pattern already used for limit changes) is the
  most self-contained to build; migrating upgrade authority to a real
  multisig/threshold scheme needs the same custody infrastructure as
  P0-1/P0-2.
- **Can be done locally now:** the timelock-wrapper mechanism itself —
  yes (A). Pointing final authority at a real threshold custody scheme —
  no, depends on P0-1/P0-2 (B/C).
- **What's needed from you:** which posture to implement — timelock
  (interim), revoke (maximally safe, zero flexibility), or full threshold
  custody now.
- **Tests/acceptance criteria:** an attempted upgrade takes effect only
  after the configured delay and is publicly observable (an event/log)
  during that window; a real-node test simulates an upgrade attempt and
  confirms the delay is enforced, not merely documented.

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

### P1-6. UI/API gap: the bridge has no way for an external user to interact with it beyond a handful of endpoints

- **Update 2026-08-15: partial progress, not closed — see item 9
  above.** `GET /health` was added, and `/status`/`/limits`/
  `/transfers/:id` gained fields (direction availability, the fee rate,
  confirmation-progress data) a future UI needs. This narrows but does
  not close this item: the endpoints the connected frontend actually
  calls today (`/stats`, `/explorer/events`, `/federation`,
  `/federation/rounds`, `/incidents`, `/reserves/history`, `/verify`)
  are all still unimplemented, and the federation-shaped-endpoint product
  decision below is still unresolved.
- **Exact problem:** see item 9 above — the connected frontend expects a
  materially larger API surface and still carries federation-shaped
  client code that doesn't map onto this bridge's actual model.
- **Why it matters:** a bridge with no usable UI is not launched in any
  meaningful sense, regardless of how correct the backend is.
- **Current implementation status:** 7 endpoints exist (up from 6); the
  frontend still needs roughly the same set of additional endpoints as
  before, plus a product decision on the federation-shaped ones.
- **What must be implemented:** the missing read-projection endpoints
  (`/stats`, `/reserves/history`, `/explorer/events`) — straightforward
  once the federation-shaped-endpoint question is resolved, since some of
  the frontend's expected surface may be dropped rather than built.
- **Can be done locally now:** the read-projection endpoints — yes (A).
  The federation-shaped-endpoint question — no (C, product decision).
- **What's needed from you:** the product decision on
  `/federation`/`/federation/rounds` — reinterpret as the 3 custody
  domains with UI copy changes, or drop entirely and adjust the frontend.
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
| Test/rehearsal completeness | **~80%** | Real-node happy paths (both directions), double-release, crash/restart, reconciliation, the full fee/accounting matrix, and now the signer-timeout/rejection, rebalancing, custody-transition, and post-finality-reorg unit/restart-recovery suites all pass. Missing: multi-node/testnet rehearsal, load/soak testing, signer-loss and `record_goldcoin_completion` real-node coverage, and real-node (not just unit-level) key-rotation/vault-sweep rehearsal suites (docs/11 items 2-3, tooling now exists but the rehearsal itself hasn't been run). |
| Production operational readiness | **~63%** | Daemon, config loading, both-direction reconciliation, CI, dependency hygiene, a written (unverified-build) Dockerfile, basic webhook alerting, tested backup/restore, and now a fully built rebalancing/custody-transition operator toolchain all exist. Missing: HSM/KMS, a verified container build, a dashboard, broader-network rehearsal. |
| Security/custody readiness | **~34%** | The cryptographic/protocol design (threshold signatures, independent re-derivation, on-chain replay guard, fail-closed everywhere) remains genuinely sound and real-node-tested. New this round: a real signer-abstraction seam (trait-based, timeout-safe, fail-closed) a production HSM/KMS backend can now be written against without touching settlement logic, and generic custody-transition tooling for key rotation/vault sweep. Still essentially untouched: the real HSM/KMS backend itself (0%), custody-domain decision (0%), program upgrade-authority resolution (0%), external audit performance (0%, though now scoped). |
| API/backend readiness | **~40%** | 7 working, tested endpoints (up from 6) including a server-authoritative fee quote, per-direction availability, the fee rate, confirmation progress, and a non-sensitive health summary. Still missing roughly the same set of frontend-expected endpoints as before (`/stats`, `/explorer/events`, `/federation`, `/reserves/history`, `/verify`), and the federation-shaped-endpoint question is unresolved. |
| UI readiness | **~15%** | Unchanged since docs/17 — a real frontend exists but runs entirely on mocks; not re-checked this round (see the header note above), last confirmed untouched via file timestamps. |
| **Overall mainnet readiness** | **~65%** | Genuine additional engineering landed this round (signer abstraction, rebalancing, post-finality-reorg protection, custody-transition tooling, API/audit-scope work) on top of docs/15's ~25% and the prior review's ~60% baseline — but mainnet readiness is still gated by the same organizational/infrastructure items this round could not touch: real custody infrastructure, a resolved upgrade-authority posture, an external audit actually performed, and a real UI. |

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
- **New this round:** the production signer trait abstraction
  (`VaultSigner`/`AttestationSigner`, timeout-safe, fail-closed); the
  rebalancing off-chain engineering layer (never mints/burns/wraps/moves
  funds itself; records evidence only); dedicated post-finality-reorg
  detection with a distinguishable global auto-pause; generic
  key-rotation/vault-sweep custody-transition tooling with an enforced
  identity-verification gate and an enforced pause-precondition; an
  expanded read-only bridge API (`GET /health`, per-direction
  availability, the fee rate, confirmation-progress data); a scoped
  external-security-audit document (docs/23).

# 2. REMAINING P0

1. **Narrowed:** the real HSM/KMS-backed signer *implementation* — the
   trait abstraction it plugs into is now done (P0-1 above).
2. Custody-domain composition decision + real key-generation ceremony —
   unchanged.
3. Program upgrade-authority posture resolved and implemented —
   unchanged.
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
6. UI/API gap — narrowed slightly (`GET /health` and several field
   additions), still missing most of the frontend's expected endpoints,
   plus the federation-shaped-endpoint product decision (see P1-6 above).

# 4. MANAGEMENT DECISIONS REQUIRED

1. Custody-domain composition (which cloud accounts/HSM vendor(s)/
   personnel for the 3 domains, both reserves).
2. Program upgrade-authority posture (timelock / revoke / threshold).
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

Done this round (removed from this list, see §1): the HSM/KMS signer
trait abstraction, rebalancing's off-chain engineering layer, dedicated
post-finality-reorg detection, the staged custody-transition CLI
flow/tooling for attestation-key rotation and Goldcoin vault sweep, an
expanded read-only bridge API, and the audit-scoping document.

Still open, still local-only, still doable without further input:

- A timelock wrapper for the program upgrade authority (mechanism only;
  which posture to finally use still needs your decision).
- The late-deposit-after-`Expired` auto-recreate behavior
  docs/04-state-machines.md describes but nothing implements yet.
- A Grafana dashboard definition on top of the existing `/metrics`
  endpoint, and a Slack/PagerDuty-specific webhook formatter.
- The missing read-projection API endpoints (`/stats`,
  `/reserves/history`, `/explorer/events`) — the plain read-projections
  only; the federation-shaped ones still need your product call first.
- Real-node rehearsals of the now-built rebalancing and
  custody-transition tooling (docs/11 items 2-3) — the tooling itself no
  longer blocks this, only the rehearsal run does.
- The runbook's cold-start-sequencing documentation fix (item 7 above).

# 9. RECOMMENDED NEXT DEVELOPMENT STEP

The three items this document previously recommended as independently
completable right now — the signer trait abstraction, the audit-scoping
document, and rebalancing — are **done as of this update**. Of what
remains local-only (§8 above), the highest-value next step is the
missing read-projection API endpoints (`/stats`, `/reserves/history`,
`/explorer/events`): they directly unblock reconnecting the existing
frontend once the federation-shaped-endpoint product decision is made,
they're a pure read-projection over data this codebase already
maintains (no new design), and they close the largest remaining piece of
P1-6. In parallel, a timelock wrapper for the program upgrade authority
is worth prototyping even before the final posture (timelock/revoke/
threshold) is decided, since the governance-timelock pattern it would
reuse already exists for limit changes — having the mechanism ready
shortens the path once that decision lands. Everything else in the
remaining P0 list (custody-domain composition, upgrade-authority
*posture*, production parameter values, actually engaging an audit firm,
the real HSM/KMS backend) is blocked on a decision or real infrastructure
this session doesn't have, unchanged from the original review.

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
