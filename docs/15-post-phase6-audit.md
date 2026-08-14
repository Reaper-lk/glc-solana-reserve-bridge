# Post-Phase-6 production-readiness audit

Full-repository, read-only audit performed after the Phase 6 real-node
acceptance rehearsal (docs/14-phase6-checkpoint.md). Scope: everything in
`programs/`, `service/`, `shared/`, `docs/`, `tests/`, plus the connected
frontend repository at `/home/reaper/glc-solana-bridge-ui` for the UI/API
question only (read-only, not modified).

Standing architecture invariant re-confirmed throughout this audit: this
is a **reserve-backed 1:1 bridge**. It must never mint, burn, wrap, or
create tokens. The Solana side moves already-existing SPL tokens between a
pre-funded reserve and a recipient; the Goldcoin side moves already-
existing native GLC between a pre-funded multisig vault and a recipient.
The Solana GLC mint address is only ever a token-identity/lookup value.

## 1. Implementation phases — current status

| Phase | Scope | Status |
|---|---|---|
| 0/1 | Goldcoin/Solana chain plumbing, reserve ledger | Complete |
| 2 | On-chain program core (`initialize`, `initialize_reserve_vault`, `set_paused`, `set_limit`, admin transfer, attestation-key rotation governance, `release_from_reserve`, `deposit_to_reserve`, `record_goldcoin_completion`) | Complete for the instructions that exist. **`rebalance_deposit`/`rebalance_withdraw` do not exist anywhere in the program** — 0% implemented despite being described as part of this phase's design in 5+ docs. |
| 3/4 | Vault/payout construction, attestation + vault signer groups, orchestrator tick loop | Complete as library code, real threshold signing, all tick phases present |
| 5 | Ops tooling | Partial. `glc-admin`/`glc-audit` CLIs work. `ops::health::serve` (a real Prometheus `/metrics` + `/health` HTTP server) is fully implemented but **never called from any binary**. |
| 6 | Real-node acceptance rehearsal | Complete — verdict REHEARSAL READY (docs/14) |
| — | **Long-running service daemon** | **Does not exist.** `service/src/lib.rs` states outright there is no process entrypoint driving `Orchestrator::tick()` on an interval. Only `service/src/bin/glc-admin.rs` and `glc-audit.rs` exist — both one-shot operator CLIs, not the bridge itself. Every piece of orchestrator/health/metrics logic Phase 6 rehearsed has only ever been driven by tests; nothing in this repository can currently be deployed and left running. This is the single largest gap and predates and outlives every phase above — it isn't phase-scoped, it's just missing. |

## 2. Every TODO/stub/mock/placeholder/deferred item found

- **Zero** `TODO`/`FIXME`/`XXX`/`HACK` comments anywhere in the repo.
- **Zero** `todo!()`/`unimplemented!()`/`unreachable!()` reachable from production code — all confined to `*/tests.rs` or `tests/`.
- **`DevVaultSigner`/`DevAttestationSigner` are not test-only** — they are the *only* signer types that exist, and `Orchestrator` is hard-typed to them concretely (`vault_signers: Vec<DevVaultSigner>`, `attestation_signers: Vec<DevAttestationSigner>`, not a trait). No HSM/KMS abstraction exists to swap in a real implementation against. Both modules carry explicit "dev/test key posture only" banners.
- **`create_throwaway_mint`/`mint_to` in `service/tests/support/mod.rs`** construct real `InitializeMint`/`MintTo` instructions — but this is Cargo integration-test-only code (`tests/` directory, never compiled into the library or any binary), used solely to fabricate a stand-in "existing GLC mint" for isolated local-validator rehearsal. No production code path reaches this.
- **No config module exists.** `find service/src -iname "*config*"` returns nothing. `OrchestratorConfig` is `#[derive(Debug, Clone)]` only, no `Deserialize`, hand-constructed only in test code.
- **No network-facing API exists** beyond `/health` and `/metrics` (see §18).
- **Deferred management decisions** (`docs/12-management-decisions.md`), all still open as of this audit:
  1. Trust model — **resolved** (2-of-3 internal threshold custody, approved).
  2. Custody-domain composition / HSM vendor selection — open.
  3. Program upgrade-authority final posture (threshold custody / revoke / timelock) — open, currently a single `Keypair`.
  4. Production confirmation/finality depths — open, no defaults asserted.
  5. Reserve sizing — open.
  6. Rate-limit final values — open.
  7. Reservation TTL / rebalance cadence policy — open.
  8. Refund/compensation process for `Failed` requests — open.
  9. External security audit — not scoped or scheduled.
  10. Verification that the live Solana GLC mint is a standard SPL Token Program mint, not Token-2022 — **never independently verified against the actual live mint address.** This is a live correctness blocker, not documentation hygiene (see §16).
- **Runbook self-documented gaps** (`docs/09-runbook.md`): no procedure exists yet for staged multi-operator approval of attestation-key rotation, no procedure for vault sweep-to-fresh-vault compromise response, and both rebalance directions are described as "the intended shape once that work lands, not a procedure an operator can run today."

## 3. Anything preventing production deployment

In order of severity — see the prioritized roadmap (§ Roadmap) for sequencing:

1. No long-running daemon binary (§1).
2. No HSM/KMS signer implementation or abstraction — production signing keys currently could not be anything other than in-process plaintext (§12).
3. No network-facing API for request submission/status — nothing external can talk to this bridge (§18).
4. Goldcoin-side reserve has **zero automatic reconciliation/pause** — `tick_reconciliation` only covers `SolanaReserve` (§11, new finding this audit).
5. No config-loading mechanism — nothing to point at production endpoints/keys without code changes.
6. Rebalancing is entirely unimplemented (§14).
7. Program upgrade authority is a single, unprotected `Keypair` — undermines every on-chain control if left as-is (per the threat model's own assessment).
8. The live Solana GLC mint's token-program type has never been verified — if it's Token-2022, the program's hardcoded legacy-SPL account constraints are wrong.
9. No deployment manifest of any kind (`docker/` is empty), no alerting, no dashboard, no backup/restore tooling (§19).
10. No external security audit has been scoped, let alone performed (§17).

## 4. Token issuance safety — **CONFIRMED: no path can mint, burn, wrap, or create tokens**

Exhaustive check of the single on-chain program (`programs/glc-reserve-bridge/`) and the entire off-chain service:

- Only two token CPIs exist anywhere in the program, both `anchor_spl::token::transfer_checked`: `deposit_to_reserve.rs:127` (user ATA → reserve ATA, user-signed) and `release_from_reserve.rs:199` (reserve ATA → recipient ATA, PDA-signed via `invoke_signed`, gated on 2-of-3 attestation + solvency check).
- Zero `mint_to`/`MintTo`/`burn`/`Burn`/`InitializeMint` anywhere in `programs/glc-reserve-bridge/src/`.
- `reserve_mint` is read-only everywhere (address-constrained `Account<'info, Mint>`) — never a signer/authority, only supplies the mint field `transfer_checked` needs for decimal verification. `initialize_reserve_vault` only ever `init`s a token *account* (the reserve's ATA), never a `Mint`.
- Every "mint"/"burn"/"wrap" text hit elsewhere is a comment explicitly documenting the deliberate absence (e.g. `lib.rs:6`: "No mint, no burn: this program...").
- Off-chain `service/src/` and `shared/`: zero hits of any kind.
- The only place `InitializeMint`/`MintTo` are constructed anywhere in the repository is `service/tests/support/mod.rs`, gated behind Cargo's integration-test convention, unreachable from any production binary.

**Verdict: no code path anywhere, on-chain or off-chain, can mint, burn, wrap, or create a token. Invariant #4 is structurally satisfied, not merely tested-to-hold.**

## 5. Do both directions enforce the intended reserve-backed accounting model?

Yes, with one asymmetry that is architecturally accepted rather than a defect: GLC→SOL releases are guarded by a true on-chain replay mechanism (the `DepositClaim` PDA); SOL→GLC payouts have no on-chain equivalent (Goldcoin has no program layer) and rely on off-chain SQLite constraints (`goldcoin_payouts.request_id` primary key, `goldcoin_payout_inputs UNIQUE(txid, vout)`) as "the actual boundary against double-pay," per the schema's own comment. This is real protection but structurally weaker than the Solana side's — a second service instance against a divergent/restored DB is not caught by anything on-chain before a duplicate real Goldcoin transaction broadcasts. Both instruction handlers enforce `1:1`-consistent transfer amounts, protected-minimum checks, and rolling-volume limits on-chain (Solana side); see §6-7 for full detail.

## 6. Reserve solvency/invariant enforcement — PARTIALLY ENFORCED (on-chain for release; off-chain-only for admission/Goldcoin)

On-chain, unconditionally enforced (cannot be bypassed even by a compromised/buggy service): zero/min/per-transfer-limit checks, protected-minimum check (before attestation verification, so a release that could never be fulfilled costs nothing to reject), rolling-volume-limit tracking (a fixed-bucket, not sliding, window — a burst spanning a bucket boundary can transiently exceed the configured limit), and pause-flag checks in every instruction.

Off-chain only: the decision to *admit a new request* at all (`Ledger::create_request`/`check_invariant`/`available_capacity`) depends entirely on the service's own SQLite bookkeeping — there is no on-chain equivalent gate on new reservations. The Goldcoin reserve side has **no on-chain invariant layer at all** (no program), so its solvency depends entirely on off-chain vault-UTXO bookkeeping and reconciliation — which, per §11, currently doesn't run for that direction at all.

## 7. Replay and duplicate-release protection — asymmetric, as described in §5

GLC→SOL: fully on-chain (`DepositClaim` PDA, `init`-once), verified against a real node this phase. SOL→GLC: off-chain-only (SQLite uniqueness constraints); `complete_goldcoin_payout.rs`'s own comments explicitly document this asymmetry as accepted, not accidental. Recommend this be stated plainly as a residual trust boundary in any external security review.

## 8. Confirmation/finality handling — configurable, but no production values exist anywhere

Goldcoin: `IndexerConfig::confirmation_depth` is a real, non-hardcoded config field. Solana: every RPC read is hardcoded to `CommitmentLevel::Finalized` (deliberate and correct, not a gap). Gap: there is no config file, env-loader, or example production config anywhere — the only places these values are ever set are test fixtures chosen for fast iteration (e.g. `required_goldcoin_confirmations: 3`), explicitly not production-appropriate, and nothing documents or enforces minimum safe production depths.

## 9. Reorg handling — mostly solid, one real gap found

Pre-finality Goldcoin reorg handling is correct and tested (`mark_glc_reorged`, returns to `AwaitingDeposit`, preserves the reservation). Reorgs deeper than `max_reorg_depth` hard-halt rather than guess. Solana needs no reorg logic given the finalized-only read discipline (correct by construction). **Gap**: the threat model document (`docs/10-threat-model.md`) claims post-finality Goldcoin reorgs should be an automatic global-pause trigger — but `service/src/reconciliation/mod.rs` has no reorg-specific detection at all; it would only surface, incidentally and untested, via the generic unexplained-balance-drop path. No test simulates this scenario. The only actual post-finality-reorg code is a defensive panic in `mark_glc_reorged` that refuses to silently auto-revert — correct as a last line of defense, but not the dedicated detect-and-page mechanism the threat model describes.

## 10. Crash/restart/idempotency — FULLY ENFORCED, both directions, every stage

Every stateful transition follows the same pattern: no-op if already at-or-past target state, hard assert if the state is genuinely unexpected (caller bug, not a runtime condition). Backed by unique indexes (`(source_txid, source_vout)`, `source_obligation_index`), the `DepositClaim` PDA, and `WithdrawalObligation.status` terminal guards. Exercised end-to-end against real nodes in Phase 6 (orchestrator dropped mid-settlement, rebuilt against the same on-disk DB and same real nodes, settled exactly once).

## 11. Reconciliation and automatic pause — PARTIALLY ENFORCED, one significant gap beyond the known one

Trigger/never-auto-clear/operator-unpause path (`glc-admin unpause`/`onchain-unpause`) all confirmed correctly implemented for the direction it covers. **New finding this audit, not previously documented**: `Orchestrator::tick_reconciliation` is hardcoded to `ReserveDirection::SolanaReserve` only — its own doc comment says Goldcoin reconciliation needs a live vault UTXO scan and is "deferred." That comment is now stale in one respect (Phase 6 *did* add `list_unspent`/`tick_vault_utxos` wiring), but `tick_reconciliation` itself was never updated to also cover `GoldcoinReserve`. **Net effect: the Goldcoin reserve direction has zero automatic breach detection and zero automatic pause today.** A discrepancy between the vault's real Goldcoin balance and the ledger's bookkeeping — theft, bug, operator error — would never trigger a pause. This is materially significant and is added to the gap list here since it postdates docs/14. The previously-documented SolanaReserve cold-start reconciliation margin issue (docs/14) remains accurately described and unchanged.

## 12. Key-management / HSM / KMS work required

**0% implemented.** No KMS/HSM integration exists anywhere. `DevVaultSigner`/`DevAttestationSigner` are the only signer implementations, held as concrete struct types (not behind a trait) directly inside `Orchestrator`. A real HSM/KMS backend requires **introducing a signer trait abstraction first**, then a real implementation against it — this is new architecture work, not a config swap. No key-generation ceremony procedure exists anywhere in the docs either.

## 13. 2-of-3 attestation/custody configuration required

**0% operator-facing.** The 2-of-3 threshold *design* is approved, but which three cloud accounts/HSM vendors/personnel constitute each custody domain is an open organizational decision (docs/12 item 2), and there is no code path anywhere that constructs production signer sets — only test helpers (`three_vault_signers()`/`three_attestation_signers()`) generating fresh dev keys. This needs both an organizational decision and the code from §12 to exist before it can be configured at all.

## 14. Rebalancing functionality required

**0% implemented**, fully scoped on paper. `rebalance_deposit`/`rebalance_withdraw` on-chain instructions, a `rebalance_events` ledger table, and `glc-admin` subcommands for triggering/tracking rebalances all need to be built from scratch. The only code trace today is the string `'rebalance'` permitted in an audit-log CHECK constraint.

## 15/16. Goldcoin and Solana production configuration required (with exact fields)

No config-loading mechanism exists at all (§2). Every field below needs both (a) a real decided value and (b) a loading mechanism (env/file) built, since neither currently exists:

| Field | Current state |
|---|---|
| Goldcoin RPC endpoint + credentials | No production value; RPC URL is a plain string parameter, mechanically easy to point elsewhere once a config loader exists |
| Solana RPC endpoint | Same — plain string, no devnet/mainnet hardcoding found, but no loader |
| Existing production Solana GLC mint address | **Never appears anywhere in code or config** — placeholder everywhere it's referenced; its token-program type (legacy SPL vs Token-2022) has never been verified against the real address |
| Reserve token account derivation inputs | Code exists (`reserve_authority` PDA + ATA derivation) — just needs the real mint address above |
| Goldcoin vault redeem script / multisig address | No production value; construction code exists and is tested |
| Confirmation depths (`confirmation_depth`, `vault_min_confirmations`) | Fields exist, only test-appropriate values ever set, no production values decided (docs/12 item 4) |
| Per-transfer / rolling-volume limits, `protected_minimum` | Fields exist on-chain and off-chain, no production values decided (docs/12 items 5-6) |
| Reservation TTL | Field exists, no production value decided (docs/12 item 7) |
| Attestation threshold / vault threshold | Fields exist and are exercised (2-of-3), but see §13 for who actually holds the keys |
| Governance timelock | Field exists on-chain (`governance_timelock_seconds`), no production value decided |
| Operator/admin identities | No config surface exists; `admin` is whichever key called `initialize` |

Switching RPC endpoints is mechanically just a different string (good — no code changes needed there specifically), but essentially everything else in this table is either an undecided value, a missing config-loading mechanism, or both.

## 17. Security review / audit work required before production

- **External third-party audit**: not scoped or scheduled (docs/12 item 9 says only "recommend scoping once Phase 2-4 code exists" — that code now exists, nothing further has happened).
- **No CI, no `cargo-audit`/`cargo-deny`, no fuzzing** anywhere in the repository — `cargo-deny` was mentioned as an aspiration in the implementation plan and never actually added; `Cargo.lock` has never been audited for known vulnerabilities.
- **Program upgrade authority**: unresolved (§3/§12-management-decisions item 3) — the threat model itself calls this the thing that "undermines every on-chain control this design relies on" if left open.
- **Live GLC mint verification**: unresolved, live correctness blocker (§16).
- **No key-generation ceremony procedure** documented anywhere.
- **Threat-model claim not actually backed by code**: the doc's post-finality-reorg auto-pause claim (§9) is not implemented as described.

## 18. UI/API work required to connect to the existing bridge frontend

**Essentially 0% built on the bridge side.** The only network-facing surface anywhere in `service/src` is `/health` and `/metrics` (explicitly no-auth/no-TLS, "bind privately"). There is currently no way for any external frontend or user to create or query a bridge request over the network — the only path to `Ledger::create_request` is a direct in-process Rust call or raw SQL.

A real frontend exists at `/home/reaper/glc-solana-bridge-ui` (Next.js), currently running entirely on mock fixtures (a `mock`/`http` mode switch). Its expected REST contract is substantial: `GET /status`, `/limits`, `/stats`, `/transfers/:id`, `/transfers`, `/explorer/events`, `/federation`, `/federation/rounds`, `/incidents`, `/reserves`, `/reserves/history`, `/verify`, and `POST /transfers`. None of this exists on the bridge side today.

Worth flagging explicitly: the frontend's client interface still carries **federation-shaped surface** — `getFederation`/`listSigningRounds` calls, and a `createTransfer` code comment literally referencing a `glc-to-wglc` (wrapped-GLC) direction. Connecting this reserve bridge to that frontend will require either reinterpreting those endpoints as "the 3 internal custody domains" (with UI copy changes) or dropping them outright — they don't map cleanly onto this repo's internal-threshold-custody, non-wrapped-token model. This is itself a small instance of the same stale-federation-assumption problem the rest of this audit was asked to check for, just living in the *other* repository rather than this one.

## 19. Monitoring, alerting, deployment, operational work required

What exists: `/metrics` in real Prometheus text-exposition format, `/health` JSON endpoint, `glc-audit` (a real offline auditor re-verifying attestation commitments + SQLite integrity, with exit codes designed for a cron job to page on).

What's absent: the daemon to actually serve any of this continuously (§1); any alerting integration at all (no PagerDuty/Slack/webhook code anywhere — the runbook's "Alert fired" language is aspirational prose with nothing behind it); any deployment manifest (`docker/` directory exists but is completely empty — no Dockerfile, no compose file, no systemd unit); any dashboard (no Grafana config); any backup/restore tooling for the SQLite ledger (`glc-audit`'s own comment assumes backups exist without anything producing one); a cron job actually running `glc-audit` on a schedule.

## 20. Old federated/mint-burn/wrapped-token assumptions remaining

**None found in this repository as genuine leftover assumptions.** The ~50 hits for "federated"/"mint"/"burn"/"wrap" across code and docs are all intentional, explicit anti-assumption comments contrasting this design with the old bridge (e.g. "Adapted from the old (federated, mint/burn) bridge's `glc_bridge`" as a disclaimer). The one place a real stale assumption does live is the **connected frontend repository**, not this one — see §18.

---

# Prioritized roadmap

**P0 — required before any public testing** (i.e. before any external party, even on a valueless test network, can run against a live instance):

1. Build the long-running daemon binary: wire config loading + `Orchestrator::tick()` on an interval + `ops::health::serve` + graceful shutdown into a real deployable process. Nothing else matters until something can run continuously.
2. Add a config-loading module (env/file) for `OrchestratorConfig` and both chains' RPC endpoints — required for the daemon above to be pointed anywhere without code edits.
3. Fix the Goldcoin-side reconciliation gap (§11): extend `tick_reconciliation` to cover `GoldcoinReserve`, not just `SolanaReserve`. This is a core safety mechanism with a real hole in it right now.
4. Add a minimal network-facing API (even a bare-bones one) for request submission and status query — without this, nothing external, including a test UI, can interact with the bridge at all.
5. Verify the real Solana GLC mint's token-program type (legacy SPL vs Token-2022) against the actual live address. Cheap to check now; expensive if discovered later after code depending on the wrong assumption ships.

**P1 — required before a testnet/regtest release candidate:**

6. Wire basic alerting (webhook/Slack at minimum) on pause events and reconciliation breaches.
7. Add a deployment manifest — at minimum a Dockerfile; `docker/` is currently empty.
8. Broader-network rehearsal: multi-node, not just a single local validator/single regtest node; real testnet (Solana devnet + Goldcoin testnet if available) integration pass, not just localnet.
9. Load/soak testing — everything to date is single- or dual-request.
10. Backup/restore tooling for the SQLite ledger, plus a scheduled job actually running `glc-audit`.
11. Close the remaining Phase 6 real-node gaps: signer-loss and `record_goldcoin_completion` verified against real nodes, not just mocks.
12. Add the dedicated post-finality-reorg detection/auto-pause path the threat model claims exists (§9) — currently only incidental via the generic balance-drop check.
13. Add `cargo-audit`/`cargo-deny` and basic CI so dependency and lint regressions are caught automatically.

**P2 — required before mainnet:**

14. Design and build a real HSM/KMS signer abstraction (trait) and a production implementation, replacing the hardwired `DevVaultSigner`/`DevAttestationSigner` — the single largest piece of remaining work.
15. Resolve the custody-domain/HSM-vendor organizational decision (docs/12 item 2) and run an actual key-generation ceremony.
16. Resolve the program upgrade-authority posture (threshold custody / revoke / timelock — docs/12 item 3) and implement it.
17. Implement `rebalance_deposit`/`rebalance_withdraw`, the `rebalance_events` table, and operator tooling for both.
18. Scope and complete an external third-party security audit.
19. Finalize real production values for confirmation depths, reserve sizing, rate limits, and reservation TTL (docs/12 items 4-7), and build the staged multi-operator attestation-key-rotation approval flow plus the vault sweep-to-fresh-vault compromise-response procedure.
20. Build the real REST API surface and connect it to the frontend, resolving the frontend's remaining federation-shaped endpoints (§18) one way or the other.
21. Stand up a real dashboard (Grafana or equivalent) on top of the existing `/metrics` endpoint.
22. Formal final verification of the live GLC mint assumption (§16) as a production sign-off gate, not just the early P0 check.

**P3 — post-launch improvements:**

23. UI feature polish beyond the minimum viable API surface built for P2.
24. Further ops-procedure automation (e.g. automated rebalance triggers, automated backup rotation).
25. Performance/scale optimization beyond what load testing in P1 required.
26. Any additional monitoring/analytics beyond the core dashboard.

---

# Completeness estimates

| Area | Estimate | Basis |
|---|---|---|
| Core bridge software (on-chain program + ledger + orchestrator + signing logic, excluding rebalancing/daemon/HSM) | **~70%** | Both directions' transactional logic, invariants, and replay protection are real and real-node-verified; missing pieces are the daemon wrapper, Goldcoin reconciliation coverage, and rebalancing (0%) |
| Test / rehearsal completeness | **~55%** | Real-node happy paths, double-release, crash/restart, and reconciliation-within-tolerance all pass; missing multi-node/testnet rehearsal, load/soak testing, signer-loss and `record_goldcoin_completion` real-node coverage, and anything touching HSM/KMS or rebalancing (none of which exist to test) |
| Production operational readiness | **~10%** | No daemon, no config loader, no deployment manifest, no alerting, no dashboard, no backup tooling, no HSM — only the CLI ops tools and the (unwired) health/metrics server exist |
| UI completeness (as it relates to this bridge) | **~5%** | A frontend exists but runs entirely on mocks; zero API surface exists on the bridge side for it to connect to; the frontend's own client shape still needs federation-related rework |
| **Overall mainnet readiness** | **~25%** | Weighted toward the operational, custody, and integration gaps above — the core transactional design is sound and well-tested, but mainnet requires real keys/custody, a real deployable process, a real API, and an external audit, none of which exist yet |
