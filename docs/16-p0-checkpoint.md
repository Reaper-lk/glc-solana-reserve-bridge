# P0 implementation checkpoint

Scope: every P0 item from the post-Phase-6 audit's roadmap
(docs/15-post-phase6-audit.md), implemented autonomously in dependency
order (config loading before the daemon that needs it; both before the
API that needs both). No mainnet, production funds, production keys, or
production custody key generation used at any point.

## Commits, in order

- `a2a87be` — GoldcoinReserve gets the same automatic reconciliation as Solana
- `4186cc3` — production-style configuration loading (`config.rs`)
- `236ef7a` — the long-running `glc-bridge-daemon` process
- `460032f` — structural token-program verification for the reserve mint
- `211977f` — minimal bridge API for the future web UI
- `36a2a01` — remaining test coverage (concurrency, wrong-token-program real-node test)

## Functionality implemented

**1. The long-running daemon (`service/src/daemon.rs` + `service/src/bin/glc-bridge-daemon.rs`).**
The single largest gap from the audit — this repository had no process
that could be deployed and left running. Now: `daemon::run` drives
`Orchestrator::tick()` on a configurable interval; a tick, once started,
always completes before a shutdown check runs, so no settlement step is
ever interrupted mid-flight. Consecutive ticks where *both* chains are
unreachable widen the inter-tick delay (capped, exponential); a partial
outage (one chain down) never triggers backoff, so the healthy chain
keeps making progress. `glc-bridge-daemon` wires real config, real
signer-key loading (with cross-validation against the config-declared
pubkeys), real RPC clients, the orchestrator, `ops::health::serve`
(`/health`+`/metrics`), the new bridge API, and `SIGINT`/`SIGTERM`
handling together. Every startup step fails closed (exits 2) with a
clear, logged reason rather than starting partially configured.

**2. GoldcoinReserve reconciliation (`service/src/orchestrator.rs`).**
`tick_reconciliation` covered `SolanaReserve` only — the Goldcoin
reserve direction had zero automatic breach detection. Split into
`tick_solana_reconciliation` (unchanged) and a new
`tick_goldcoin_reconciliation`, both run every tick, both fail closed
(pause, never auto-clear) on an unexplained drop, exactly matching the
existing Solana-side design.

**3. Configuration loading (`service/src/config.rs`).**
A single TOML file plus environment-variable overrides for RPC
endpoints/credentials. Covers everything requested: the existing Solana
GLC mint address, Solana RPC endpoint and commitment, Goldcoin RPC
endpoint/credentials/confirmation depth, operator/attestor identities,
reserve minimums/limits, reconciliation tolerance, service bind
addresses. Fails closed on threshold/pubkey-count mismatches,
`critical_reserve <= protected_minimum`, and a `goldcoin.network` field
that only accepts `"regtest"` today (see "New blocker found" below). No
production secrets are embedded — signing keys are loaded from local
files whose *paths* are named in config, each cross-checked against the
config-declared pubkey and refused on mismatch; this is explicitly
DEV/TEST posture pending the HSM/KMS work in P2.

**4. Minimal bridge API (`service/src/api.rs`).**
The first network-facing way for anything external to interact with the
bridge — previously the only path to `Ledger::create_request`/
`get_request` was a direct in-process call or raw SQL. Five endpoints:
`GET /status`, `/limits`, `/reserve`, `POST /transfers` (GLC->Solana
only — Solana->GLC has no off-chain "create" step by design, the user
calls `deposit_to_reserve` directly on-chain and the indexer picks it
up), `GET /transfers/:id`. Exposes no custody keys, signing material,
admin operations, or infrastructure detail; reserve figures are limited
to derived *available capacity*, not the raw balance breakdown
`ops::health` reports for an operator audience. Deliberately no
`/federation`-shaped endpoint.

**5. Structural token-program verification (`accounts::verify_reserve_mint_token_program`).**
Reads the configured `reserve_token_mint` and confirms it's owned by the
legacy SPL Token program before the daemon does anything else. The
on-chain program's own Anchor constraints already reject a Token-2022
mint at the instruction level; this makes the fact explicit, checkable
off-chain, and gives a clear startup-time error instead of a generic
on-chain failure discovered only once a real transfer is attempted.
Never creates, mints, burns, or wraps anything.

**6. Federation/wrapped-token-era language.** Swept every file added or
modified this phase (`grep -rniE "federat|wrapped|wrap_token|mint_to|burn"`)
— zero genuine hits; every match is either an explicit anti-assumption
comment (matching the established codebase pattern) or a legitimate
"mint" as in "reserve_token_mint" (identity, never issuance). The one
place federation-era assumptions do live — the *frontend* repository's
client code (`getFederation`, a `glc-to-wglc` comment) — was flagged in
the post-Phase-6 audit and is out of scope here: a separate repository
this session was not authorized to modify.

**7. Test coverage**, against the explicit P0 list:

| Requirement | Coverage |
|---|---|
| daemon restart | `daemon::tests::daemon_restart_resumes_ticking_against_the_same_ledger`; real-node crash/restart already covered in Phase 6 |
| RPC outage/recovery | `daemon::tests::{partial_outage_never_counts_as_a_full_failure, total_outage_is_detected_as_a_full_failure, run_backs_off_during_a_total_outage_and_recovers_promptly}` |
| reconciliation breach | `orchestrator::tests::reconciliation_breach_pauses_the_{solana,goldcoin}_reserve_without_aborting_the_tick`, `goldcoin_reconciliation_pause_survives_a_simulated_crash_and_restart` |
| reserve depletion | `api::tests::create_transfer_reports_insufficient_liquidity_never_creates_a_row`, plus pre-existing `adversarial.rs` coverage |
| malformed configuration | 13 tests in `config::tests` (missing file, bad TOML, wrong commitment, wrong network, threshold/pubkey mismatches, bad bounds, key-file mismatches, env overrides) |
| wrong token program | `accounts::tests::{accepts/rejects}_a_mint_owned_by_*`, and **real-node**: `daemon_refuses_to_start_with_a_reserve_mint_owned_by_the_wrong_program` |
| duplicate/replayed request | Phase 6's real-node `double_release_crash_restart_and_reconciliation_on_real_nodes`, plus existing ledger-level coverage |
| API request lifecycle | 22 tests in `api::tests` (status/limits/reserve/create/get, error mapping, routing) |
| concurrent requests | `api::tests::concurrent_post_transfers_never_oversubscribe_capacity` (20 concurrent HTTP requests against a real spawned server), plus pre-existing ledger-level `adversarial.rs` coverage |
| graceful shutdown | `daemon::tests::run_ticks_until_shutdown_and_then_returns`, plus **real-node**: `daemon_starts_ticks_serves_health_and_shuts_down_cleanly_on_sigterm` (real `SIGTERM`, exit 0, log line asserted) |
| fail-closed behavior | Pervasive: every startup step, every config field, the token-program check, both reconciliation directions |

## Test results

- `cargo +nightly test --lib`: **242 passed, 0 failed** (up from 195 at the Phase 6 checkpoint)
- `cargo +nightly fmt -- --check`: clean
- `cargo +nightly clippy --all-targets -- -D warnings`: clean
- Real-node suite (`GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN` set), single-threaded, run repeatedly for stability: **5 passed, 0 failed**
  - `glc_to_sol_release_settles_end_to_end_on_real_nodes`
  - `sol_to_glc_payout_settles_end_to_end_on_real_nodes`
  - `double_release_crash_restart_and_reconciliation_on_real_nodes`
  - `daemon_starts_ticks_serves_health_and_shuts_down_cleanly_on_sigterm` — spawns the actual compiled `glc-bridge-daemon` binary, confirms it starts, ticks, serves `/health` and `/status`, and exits 0 on a real `SIGTERM`
  - `daemon_refuses_to_start_with_a_reserve_mint_owned_by_the_wrong_program` — spawns the real binary, confirms it exits 2 before ever serving `/health`

## New blocker found this round (not in docs/14 or docs/15)

While wiring `config.rs`'s `goldcoin.network` validation, found that
`goldcoin::address` only defines **regtest** base58check version bytes
(`P2PKH_VERSION_REGTEST`, `P2SH_VERSION_REGTEST`) — there are no
mainnet (or testnet, if Goldcoin has one) version-byte constants
anywhere in this codebase. `MultisigVault::new` derives its address via
`base58check_encode(P2SH_VERSION_REGTEST, ...)` unconditionally. This
means **the vault-address derivation itself cannot produce a valid
production Goldcoin address today**, independent of any key-management
question. Config now fails closed on this (`network = "mainnet"` is
rejected with a clear error citing this exact gap, with a passing
regression test), so nothing will silently produce a wrong address —
but the capability itself doesn't exist yet and needs the real version
bytes to build.

## Remaining P0 blockers

**None that were independently completable this round.** All seven
items were addressed. The one sub-piece that is blocked, per the
explicit instruction to stop only that specific step and continue
everything else: **verifying the real production Solana GLC mint's
token-program ownership against reality** — the structural check
(`accounts::verify_reserve_mint_token_program`) is built, wired into the
daemon's startup, and tested against both a real legacy-SPL mint and a
real wrong-owner account on a local validator; running it against the
*actual* production mint requires that mint's real address and a
reachable RPC endpoint for wherever it lives, neither of which is
available here.

## Remaining P1 work (unchanged from docs/15 except where noted)

Alerting integration (webhook/Slack on pause events); a deployment
manifest (`docker/` is still empty — no Dockerfile); broader-network
rehearsal (multi-node, real testnet rather than single-node regtest/
localnet); load/soak testing; backup/restore tooling for the SQLite
ledger plus a scheduled `glc-audit` run; real-node verification of
signer-loss and `record_goldcoin_completion` specifically; a dedicated
post-finality-reorg detection/auto-pause path (currently only
incidental via the generic balance-drop check); `cargo-audit`/
`cargo-deny` and basic CI.

## Remaining P2 work (unchanged from docs/15, plus the new Goldcoin address-version-byte gap)

HSM/KMS signer abstraction and a real implementation (the config's
key-file loading is a seam toward this, not a substitute for it); the
custody-domain/HSM-vendor organizational decision and an actual
key-generation ceremony; resolving the program upgrade-authority
posture; implementing `rebalance_deposit`/`rebalance_withdraw`; an
external third-party security audit; finalizing real production values
for confirmation depths, reserve sizing, rate limits, and reservation
TTL (the config field for TTL now exists — `service.reservation_ttl_secs`
— the *value* is still undecided); the staged multi-operator
attestation-key-rotation approval flow and vault sweep-to-fresh-vault
compromise-response procedure; building a real UI against the new API
and resolving the existing frontend's federation-shaped endpoints one
way or the other; a dashboard on top of `/metrics`; **Goldcoin mainnet/
testnet base58check version bytes** (new this round — needed before
`goldcoin::address`/`MultisigVault` can represent anything but a
regtest address).

## Updated completeness estimates

| Area | docs/15 (post-Phase-6) | Now | Why it moved |
|---|---|---|---|
| Core bridge software | ~70% | **~78%** | Goldcoin reconciliation gap closed; token-program verification added. Rebalancing (0%) and the HSM signer-trait abstraction are still missing, capping this below 80%. |
| Test/rehearsal completeness | ~55% | **~65%** | New real-node daemon evidence (happy path + wrong-token-program negative test), API concurrency test, config/reconciliation unit coverage. Multi-node/testnet rehearsal, load testing, and signer-loss/`record_goldcoin_completion` real-node coverage remain open. |
| Production operational readiness | ~10% | **~35%** | The daemon, config loading, continuous health/metrics serving, structured logging, and startup fail-closed behavior are the biggest single jump in this checkpoint. Still missing: HSM/KMS, deployment manifest, alerting, dashboard, backup/restore, external audit. |
| UI completeness | ~5% | **~15%** | A real, tested API now exists for a frontend to call. No actual UI has been built against it yet, and the existing frontend's federation-shaped client code is untouched (out of scope, separate repository). |
| **Overall mainnet readiness** | ~25% | **~35%** | Every independently-completable P0 item is done. What's left for mainnet is dominated by custody/HSM (organizational + architectural), external audit, broader rehearsal, and the newly-found Goldcoin mainnet-address gap — none of which are code this session could safely do without production information or an architecture-changing custody decision. |

## Exact information needed next

1. **The real production Solana GLC mint address**, plus a reachable RPC
   endpoint for wherever it currently lives — to run
   `verify_reserve_mint_token_program` against reality and close the one
   blocked P0 sub-item above.
2. **Goldcoin's real mainnet (and testnet, if applicable) base58check
   P2PKH/P2SH version byte constants** — newly found this round; without
   them, no valid production Goldcoin vault address can ever be derived,
   independent of any custody decision.
3. **The custody-domain/HSM-vendor decision** (docs/12-management-decisions.md
   item 2) — organizational, required before the P2 HSM/KMS
   implementation work can start.
4. **A decision on the program upgrade-authority's final posture**
   (threshold custody / revoke / timelock — docs/12 item 3).
5. **Real production values** for confirmation depths, reserve sizing,
   rate limits, and reservation TTL — the config fields all exist now;
   the operator-chosen numbers don't yet.
6. **How to connect this new API to the existing bridge frontend** — a
   product decision: reinterpret its federation-shaped endpoints
   (`/federation`, `/federation/rounds`) as this bridge's 3 internal
   custody domains with UI copy changes, drop them, or build a fresh
   minimal UI directly against the 5 endpoints added this round.

Nothing was pushed, no PR opened, no merge, no deploy, no mainnet
interaction, no production keys used or generated, and the approved 1:1
reserve architecture was not changed.
