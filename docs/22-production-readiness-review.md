# Consolidated production-readiness review

Performed 2026-08-15, read-only against the current repository state plus
every prior checkpoint (docs/00 through docs/21). Scope: everything in
`programs/`, `service/`, `shared/`, `docs/`, `tests/`, `docker/`,
`scripts/`, `.github/`, plus a read-only timestamp/content check of the
connected frontend repository at `/home/reaper/glc-solana-bridge-ui` for
the UI-readiness question only (not modified, not part of this
repository).

No code was changed to produce this review. No production keys were
generated or used. No funds were moved. Nothing was deployed. No mainnet
transaction was submitted. Nothing was pushed or opened as a PR.

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

**Remaining real gap, carried since docs/14/15, still open:** no dedicated
post-finality-reorg detect-and-page code path exists — the threat model
(docs/10) claims this is an automatic global-pause trigger, but the only
code that would actually catch it today is the generic unexplained-
balance-drop check inside `reconciliation::reconcile`, which is not
reorg-specific and has never been tested against a simulated
post-finality-reorg scenario. **Classification: A** (implementable now:
add explicit reorg-depth-vs-finality-depth detection wired to a dedicated,
tested global-pause path) or **B** if "tested against a simulated
post-finality reorg on a real node" is required for acceptance (needs
`invalidateblock`/`reconsiderblock` against a real regtest node past
finality depth, which this repo's harness can do, so likely still A).

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

**Status: minimally functional, materially incomplete relative to the
existing frontend's expectations.** Current surface: `GET /status`,
`/limits`, `/reserve`, `/transfers/:id`, `POST /transfers`, `POST /quote`
(6 endpoints total, `/quote` added this session). The connected frontend
repository (`/home/reaper/glc-solana-bridge-ui`, confirmed via file
timestamps to be **completely unchanged** since before docs/15's audit —
still running entirely on mock fixtures) expects a materially larger
contract: `/stats`, `/explorer/events`, `/federation`, `/federation/rounds`,
`/incidents`, `/reserves/history`, `/verify`, none of which exist on the
bridge side. The frontend's own client code still carries genuinely
federation-shaped calls (`getFederation`, `listSigningRounds`) and a
`glc-to-wglc` (wrapped-GLC) comment that do not map onto this reserve
bridge's actual model at all — this is a real product/UI decision to
resolve (reinterpret as the 3 internal custody domains, or drop), not an
engineering ambiguity.
**Classification: A** for endpoints that are pure read-projections of
already-existing ledger/reconciliation data (`/stats`, `/reserves/history`,
`/explorer/events` if scoped to `bridge_request_state_log`); **C** for
whether/how to reinterpret `/federation`-shaped endpoints, since that's a
product framing decision, not a technical one.

## 10. Attestation/signing architecture

**Status: design complete and correctly implemented for dev/test
custody; production custody does not exist.** The approved trust model
(docs/02, internal 2-of-3 threshold attestation + M-of-N Goldcoin vault,
independent re-derivation before every signature) is fully implemented in
library code and real-node verified: a single signer alone can never
authorize anything, independent re-derivation is exercised, replay/
duplicate-settlement guards hold under real adversarial testing. This is
genuinely solid work.
**The gap:** `DevVaultSigner`/`DevAttestationSigner` are the *only* signer
types that exist anywhere in the codebase — concrete structs, not behind
a trait — and `Orchestrator` is hard-typed to them (`Vec<DevVaultSigner>`,
`Vec<DevAttestationSigner>`). There is **no signer abstraction** a real
HSM/KMS-backed implementation could be written against yet; this is new
architecture work (introduce a trait, then a real implementation), not a
config swap. **Classification: A** for the trait abstraction itself
(can be designed and built locally without any real HSM); **B** for the
real HSM/KMS-backed implementation (needs a real HSM or cloud KMS to
develop and test against, not a local sandbox).

## 11. Custody/HSM/KMS production readiness

**Status: 0%.** No KMS/HSM integration exists. No key-generation ceremony
procedure is documented anywhere. The custody-domain composition decision
(which three cloud accounts/HSM vendors/personnel constitute the three
genuinely-separate custody domains) remains fully open
(docs/12 item 2) — this is an organizational decision this repository
cannot resolve on its own no matter how much code is written.
**Classification: C** (organizational decision, blocking) then **B**
(real HSM/KMS integration work, needs real infrastructure) once decided.

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

**Status: 0% implemented, fully scoped on paper.** No
`rebalance_deposit`/`rebalance_withdraw` on-chain instructions, no
`rebalance_events` ledger table, no `glc-admin` subcommands exist — the
only trace anywhere in the codebase is the string `'rebalance'` permitted
in an audit-log `CHECK` constraint. Without this, a live bridge will
eventually drain one reserve direction and pause it with no built-in way
to top it up except manual, ad hoc, unaudited fund movement.
**Classification: A** — fully specified in docs/03/05/06, no new
information needed, implementable locally now.

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

**Status: not scoped, not scheduled, not performed — 0%.** docs/12 item 9
recommended scoping this "once Phase 2-4 code exists" — that code has
existed since well before this review and nothing has happened since. The
codebase now includes real money-moving logic across two chains, a 1%
fee, Token-2022 support, and reserve accounting — squarely the kind of
system an audit firm should review before any production-funds decision,
independent of how much internal testing exists. **Classification: E**
(must be performed by an external party) — but scoping the engagement
(what's in scope: the on-chain program, the attestation-verification
logic, the Goldcoin multisig mechanics, the fee-computation path; what's
explicitly named as a known asymmetry for the auditor to weigh in on: the
SOL→GLC direction's non-cryptographic replay guard) is **Classification:
A**, doable now, and would materially speed up actually getting one
booked.

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

- **Exact problem:** `DevVaultSigner`/`DevAttestationSigner` are the only
  signer types anywhere in the codebase, held as concrete struct fields
  (`Vec<DevVaultSigner>`, `Vec<DevAttestationSigner>`) directly on
  `Orchestrator`, not behind a trait. There is no code path by which a
  real HSM or cloud KMS could sign on this bridge's behalf.
- **Why it matters:** this bridge's entire security model (docs/02) rests
  on signing keys living in genuinely separate, hardware/policy-protected
  custody domains. Plaintext in-process keys — the only thing that
  exists today — collapse that model back to "whoever can read this
  process's memory or its key files owns both reserves."
- **Current implementation status:** 0% for the abstraction, 0% for any
  real backend. The `config.rs` key-file-path mechanism is a reasonable
  seam toward this, not a substitute for it.
- **What must be implemented:** (1) a `VaultSigner`/`AttestationSigner`
  trait capturing exactly the operations `independently_sign`/
  `independently_attest_release`/`independently_attest_completion`
  actually need (sign-this-exact-digest, nothing more); (2) `Orchestrator`
  generic over that trait instead of the concrete `Dev*` types; (3) at
  least one real backend implementation (a cloud KMS client is the most
  practical first target; a hardware HSM PKCS#11 backend is the higher bar).
- **Can be done locally now:** the trait design and the `Orchestrator`
  generalization — **yes, entirely (A)**. A real backend implementation —
  **no**, needs a real KMS/HSM to develop and integration-test against (B).
- **What's needed from you:** which KMS/HSM vendor(s) to target for the
  first real backend (AWS KMS, GCP KMS, a specific HSM appliance, etc.) —
  affects which SDK/protocol the implementation targets.
- **Tests/acceptance criteria:** existing `DevVaultSigner`/
  `DevAttestationSigner` tests continue to pass unchanged against the new
  trait (proving the abstraction didn't change behavior); a new test
  double implementing the trait with an artificial signing delay/failure
  mode proves the orchestrator's existing retry/timeout handling still
  works generically; the real backend, once built, must pass the same
  "single signer alone cannot authorize" and "independent re-derivation"
  tests the dev signers already pass, run against the real service.

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

### P0-4. No external security audit has been scoped or performed

- **Exact problem:** the on-chain program, the attestation-verification
  logic, the Goldcoin multisig mechanics, and now the fee-computation
  path have never been reviewed by anyone outside this codebase's own
  authorship and test suite.
- **Why it matters:** this system will hold and move real funds across
  two chains under a threshold-custody model with one direction's replay
  guard structurally weaker than the other's (item 20 above) — exactly
  the kind of design where an independent reviewer catches what repeated
  self-review cannot, by construction.
- **Current implementation status:** 0%. Not scoped, not scheduled.
- **What must be implemented:** nothing code-side to *start* this — an
  audit-scoping document (what's in scope, what's explicitly flagged as a
  known asymmetry for the auditor's independent judgment, what test
  evidence already exists so the audit isn't starting from zero) can be
  written now, locally.
- **Can be done locally now:** the scoping document — yes (A). The audit
  itself — no (E), must be an external firm.
- **What's needed from you:** budget/timeline for engaging an audit firm,
  and sign-off on the scoping document once drafted.
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

### P1-1. Rebalancing is entirely unimplemented

- **Exact problem:** `rebalance_deposit`/`rebalance_withdraw` on-chain
  instructions, the `rebalance_events` ledger table, and any operator
  tooling to trigger/track a rebalance do not exist anywhere in the
  codebase.
- **Why it matters:** every settlement moves liquidity from one reserve
  to the other; over any sustained real usage period, one direction
  drains and eventually pauses (fails closed, so not unsafe — but it does
  mean the bridge cannot sustain two-way operation without a way to
  top up).
- **Current implementation status:** 0%, fully specified on paper
  (docs/03, docs/05, docs/06).
- **What must be implemented:** the two on-chain instructions
  (structurally separate from user settlements — never touch
  `reserved_liquidity`/`pending_obligations`/`bridge_requests`), the
  ledger table, and `glc-admin` subcommands for staging/approving/
  executing a rebalance with the same mandatory-note audit discipline
  every other admin action already has.
- **Can be done locally now:** yes (A) — no new information needed, the
  design is already fully specified.
- **What's needed from you:** nothing to start; eventually, the
  authorization policy for who can approve a rebalance (likely the same
  custody-domain approval pattern as other governance actions).
- **Tests/acceptance criteria:** a rebalance never appears in
  `bridge_requests`/settlement accounting or `settled_liquidity`; a
  reconciliation cycle correctly attributes the balance change to the
  rebalance event, not an unexplained drop; real-node test moving real
  (regtest/localnet) funds between reserves via the new path.

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

### P1-4. Dedicated post-finality-reorg detection is missing

- **Exact problem:** the threat model document claims a post-finality
  Goldcoin reorg triggers automatic global pause — no dedicated code path
  exists for this; only the generic unexplained-balance-drop check in
  `reconciliation::reconcile` would incidentally catch it, untested for
  this specific scenario.
- **Why it matters:** a documented safety claim that isn't backed by
  dedicated, tested code is a real gap between what an operator (or an
  auditor) would reasonably believe is protected and what actually is.
- **Current implementation status:** 0% dedicated; generic reconciliation
  provides incidental, unverified coverage.
- **What must be implemented:** explicit detection (compare the
  Goldcoin indexer's own finality-depth-reached blocks against a later
  observed reorg past that depth) wired to a dedicated, always-global
  pause with a specific, distinguishable reason string and log/event.
- **Can be done locally now:** yes (A).
- **What's needed from you:** nothing.
- **Tests/acceptance criteria:** a real-node test using
  `invalidateblock`/mine-a-competing-chain past the configured finality
  depth on regtest, confirming the dedicated path fires (not just the
  generic balance-drop path) and pauses globally, with a distinguishable
  audit-log entry.

### P1-5. Staged multi-operator attestation-key-rotation and vault-sweep procedures don't exist

- **Exact problem:** the on-chain timelocked governance instruction for
  attestation-key rotation exists, but no `glc-admin` command stages the
  required multi-operator approval; the Goldcoin vault
  sweep-to-fresh-vault compromise-response procedure has no code or
  command at all.
- **Why it matters:** these are exactly the procedures a real compromise
  incident would need, under time pressure — "we'll figure it out during
  the incident" is not an acceptable posture for a threshold-custody
  system holding real funds.
- **Current implementation status:** 0% for both, explicitly named as
  such in docs/09-runbook.md.
- **What must be implemented:** a staged-approval CLI flow (the old
  bridge's equivalent depended on a P2P transport this bridge correctly
  doesn't have; a simpler out-of-band-signature-collection design is the
  right replacement per docs/09) for key rotation; a sweep-plan/execute
  command pair for the Goldcoin vault, reusing the old bridge's
  independent-commitment-re-derivation discipline.
- **Can be done locally now:** yes (A) — fully specified, no new
  information needed.
- **What's needed from you:** nothing to start.
- **Tests/acceptance criteria:** a rehearsal test that rotates
  attestation keys end-to-end against a real program deployment,
  confirms old keys stop working post-rotation and no in-flight
  settlement is lost (docs/11 rehearsal suite item 2, currently
  unimplemented); a sweep rehearsal confirming a stale-view signer
  refuses to approve a superseded sweep commitment (docs/11 item 3).

### P1-6. UI/API gap: the bridge has no way for an external user to interact with it beyond 6 endpoints

- **Exact problem:** see item 9 above — the connected frontend expects a
  materially larger API surface and still carries federation-shaped
  client code that doesn't map onto this bridge's actual model.
- **Why it matters:** a bridge with no usable UI is not launched in any
  meaningful sense, regardless of how correct the backend is.
- **Current implementation status:** 6 endpoints exist; the frontend
  needs roughly double that, plus a product decision on the
  federation-shaped ones.
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
| Core bridge software | **~90%** | Both directions' full transactional logic, state machine, replay protection, reconciliation (both directions), and the now-fixed reserve-capacity/fee accounting are real, real-node-verified, and internally consistent. The gap to higher: rebalancing (0%), the late-deposit-after-expiry auto-recreate behavior (undocumented-as-missing), and the dedicated post-finality-reorg path. |
| Token-2022 compatibility | **~95%** | Technically complete and real-node/real-mint verified end to end. The remaining 5% is procedural (re-verify the live mint's extension set immediately before any actual mainnet deploy — state could theoretically change), not a code gap. |
| Accounting/fee system | **~95%** | Comprehensive: canonical units, fee formula, capacity fix, accrued-fee tracking/surfacing, fail-closed tamper detection, full test matrix, documentation. Deliberately deferred (not counted against this number since explicitly out of scope): fee-withdrawal/treasury path, business-minimum-transfer policy. |
| Test/rehearsal completeness | **~76%** | Real-node happy paths (both directions), double-release, crash/restart, reconciliation, and the full fee/accounting matrix all pass. Missing: multi-node/testnet rehearsal, load/soak testing, signer-loss and `record_goldcoin_completion` real-node coverage, dedicated post-finality-reorg testing, key-rotation/vault-sweep rehearsal suites (docs/11 items 2-3, never built). |
| Production operational readiness | **~57%** | Daemon, config loading, both-direction reconciliation, CI, dependency hygiene, a written (unverified-build) Dockerfile, basic webhook alerting, and tested backup/restore all exist. Missing: HSM/KMS, a verified container build, a dashboard, rebalancing, broader-network rehearsal. |
| Security/custody readiness | **~25%** | The cryptographic/protocol design (threshold signatures, independent re-derivation, on-chain replay guard, fail-closed everywhere) is genuinely sound and real-node-tested — that is real security work, reflected here. What's essentially untouched: HSM/KMS (0%), custody-domain decision (0%), program upgrade-authority resolution (0%), external audit (0%), key-rotation/vault-sweep procedures (0%). |
| API/backend readiness | **~35%** | 6 working, tested endpoints including a server-authoritative fee quote; correctly excludes any custody/admin surface from the public API. Missing roughly half of the connected frontend's expected contract, and the federation-shaped-endpoint question is unresolved. |
| UI readiness | **~15%** | Unchanged since docs/17 — a real frontend exists but runs entirely on mocks; confirmed untouched this session via file timestamps. |
| **Overall mainnet readiness** | **~60%** | Weighted toward the still-open custody/audit/ops gaps: the core transactional and accounting design is sound, real-node-verified, and now includes correct fee/decimal/capacity handling — genuinely strong progress since docs/15's ~25% baseline — but mainnet additionally requires real custody infrastructure, a resolved upgrade-authority posture, an external audit, rebalancing, and a real UI, none of which exist yet. |

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

# 2. REMAINING P0

1. HSM/KMS-backed signer implementation (no production custody without
   it).
2. Custody-domain composition decision + real key-generation ceremony.
3. Program upgrade-authority posture resolved and implemented.
4. External security audit — scope it now, perform it before any
   production-funds decision.
5. Real production parameter values (confirmation depths, reserve sizing,
   rate limits, reservation TTL) — mechanism exists, values don't.

# 3. REMAINING P1

1. Rebalancing (`rebalance_deposit`/`rebalance_withdraw`) — 0% built.
2. Broader-network (testnet, multi-node) rehearsal.
3. Load/soak testing.
4. Dedicated post-finality-reorg detection/auto-pause path.
5. Staged multi-operator attestation-key-rotation and vault-sweep-to-
   fresh-vault procedures.
6. UI/API gap — missing roughly half the frontend's expected endpoints,
   plus the federation-shaped-endpoint product decision.

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
multisig vault/payout-construction mechanics, and the 1% fee computation
and its fail-closed tamper-detection design — with the SOL→GLC
direction's non-cryptographic replay guard explicitly flagged as a known
structural asymmetry for the auditor's own independent judgment, not
asserted as safe by this repository's own authors. Never performed;
0% scoped as of this review. This review's own audit-scoping notes
(item 25 above) are a usable starting draft for that engagement, not a
substitute for it.

# 8. WHAT CAN BE IMPLEMENTED NEXT WITHOUT MY INPUT

- The HSM/KMS signer trait abstraction (not the real backend, but the
  interface + `Orchestrator` generalization).
- A timelock wrapper for the program upgrade authority (mechanism only;
  which posture to finally use still needs your decision).
- Rebalancing: both on-chain instructions, the ledger table, and
  `glc-admin` tooling — fully specified already.
- Dedicated post-finality-reorg detection and global auto-pause.
- Staged multi-operator attestation-key-rotation CLI flow and Goldcoin
  vault sweep-to-fresh-vault procedure/tooling.
- The late-deposit-after-`Expired` auto-recreate behavior
  docs/04-state-machines.md describes but nothing implements yet.
- A Grafana dashboard definition on top of the existing `/metrics`
  endpoint, and a Slack/PagerDuty-specific webhook formatter.
- The missing read-projection API endpoints (`/stats`,
  `/reserves/history`, `/explorer/events`).
- The audit-scoping document itself (item 25).
- The runbook's cold-start-sequencing documentation fix (item 7 above).

# 9. RECOMMENDED NEXT DEVELOPMENT STEP

Build the HSM/KMS signer trait abstraction and generalize `Orchestrator`
to it (P0-1's local-only half), in parallel with drafting the audit-
scoping document (P0-4's local-only half) and implementing rebalancing
(P1-1, fully local, fully specified). These three are independently
completable right now, don't require any decision from you to *start*,
and each removes a real blocker on its own critical path — the signer
trait is the seam every future HSM/KMS decision needs to exist already,
the audit scoping document shortens the calendar time to actually booking
an audit once you're ready, and rebalancing closes the last 0%-implemented
piece of core bridge software. Everything else in P0 (custody-domain
composition, upgrade-authority posture, production parameter values,
actually engaging an audit firm) is blocked on a decision or real
infrastructure this session doesn't have.

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
