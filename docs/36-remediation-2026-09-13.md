# Remediation program — 2026-09-13

Four production issues, one program. Every item below ends either
FIXED + VERIFIED or BLOCKED with the exact external dependency named.
Companion runbooks: docs/09 (operator commands), docs/30 (the program
upgrade procedure this document instantiates), docs/29 (F-8).

| # | Issue | Root cause (proven) | Code | Deploy | Status |
|---|-------|---------------------|------|--------|--------|
| 1 | "Robinhood contract has RhnToSol/SolToRhn enabled while the service has no settlement machinery" | **Premise false for V2.** Phase H (PR #85, merged 2026-09-11) gave both routes machinery; production settled `RhnToSol` 4227 end-to-end 2026-09-12 22:10–22:11 and refunded 4106 on 2026-09-13 10:53. **The contract accepting deposits nothing settles is V1** (`0x1753dDA0…`, retired at the 2026-09-11 cutover): routes 1–4 still `true`, `depositsPaused=false`, 1.56M GLC in custody, 6 `Pending` obligations, no indexer. | Capability reporting (#102) so a route can never read available without a settlement path | V1 governance pause + route close (§1 below; 2-of-3 at the V1-bound signer instances) | **Prepared; awaiting operator execution** (sudo + signer quorum) |
| 2 | Requests 4037/4038 vs obligations 29/30 "Pending → Settled with no ledger operation" | **Cross-contract confusion.** 4037/4038 are V1 #29/#30 (`RhnToSol`, 300 + 135 GLC, depositor `0x5376d8ff…`, still **Pending** on V1). The "Settled" #29/#30 are V2's — two other users' 20,000 GLC `RhnToGlc` deposits, settled by this service (txs `0x4e3292…`, `0x9a14b3…`). Nothing bound a request's `source_contract` to the settler's deployment. | Contract binding on every obligation action + parked `foreign_contract` + chain/ledger obligation audit, CLI and periodic (#101, schema v31) | Refund through V1's own `executeRefund` from `config-v1.toml` (§2) | **Code done; recovery prepared; awaiting operator decision** |
| 3 | Abuse policy not enforceable (rapid-burst, 72h minimum, fee-bearing refund, void/cancel, UI) | v30 (#100, merged, not yet deployed) already implements the classifier, indefinite hold, `review_after` enforcement on process/refund, PROCESS/REFUND decisions. Missing: a terminal disposition; a fee-bearing refund. | Terminal closure with recorded disposition (#103, schema v32). **Fee-bearing refund BLOCKED** (§3) | v30 → v32 daemon deploy; admin UI | **v30 ready to deploy; v32 in review; fee-bearing refund BLOCKED** |
| 4 | Solana `refund-manual-review` fails: `InstructionFallbackNotFound` (Anchor 101) | Deployed program (slot 442,649,805 = 2026-08-29, sha256 `76c0b517…`) predates the 2026-09-02 hardening: no `refund_withdraw`/`treasury_withdraw`/rebalance-policy instructions; `rebalance_withdraw` still live. Client built from newer source since 2026-09-02. Nothing compared them. | Compat probe + `solana_refund_supported` everywhere + refund refuses before preparing (#102); program marks obligations `Refunded` (F-8) | Program upgrade (§4) | **Detection FIXED (on deploy of #102); upgrade BLOCKED on SOL funding + ProgramData extension** |

## 0. What was preserved

`00-phase0-snapshot.sh` records main SHA, binary hashes, schema, admissions,
local pauses, V1+V2 contract flags, Solana `BridgeConfig`, the 56 held
ids, reserve invariants, and takes the SQLite/config/binary backups. The
56 operator holds are untouched by every step below; nothing here
resumes, settles, refunds or releases a held request except §2, which
names 4037/4038 explicitly and requires operator sign-off.

## 1. Phase 1 — close the door that is actually open (V1)

Evidence (read 2026-09-13 13:0x UTC, chain 4663 block 61,971,554):

```
V2 0xbaEdFFdA… migrated=false depositsPaused=false payoutsPaused=false routeEnabled(1..4)=true obligationCount=70
V1 0x1753dDA0… migrated=false depositsPaused=false payoutsPaused=false routeEnabled(1..4)=true obligationCount=31
V1 GLC balance 1,560,150 GLC; V1 obligations: 25 Settled (route 2), 4 Pending route 2 (#0,#1,#3,#28 — 20,000 GLC each), 2 Pending route 4 (#29,#30)
V1 ObligationSettled events: 25 (blocks 59,633,046 … 59,757,984); none for #29/#30
```

Merged main serves both cross routes (`routes.rs` Phase H; `orchestrator::tick_fold_rhn_to_sol_observations`, `tick_release_settlements`, `settlement::authorize_settlement` for `RhnToSol`; `solana::indexer::SolToRhnFold`). Disabling them on V2 would take a working route offline on a false premise; the dry run is provided for the record only.

Steps (scripts in the session scratchpad `remediation/`):

1. `10-v1-config-candidate.sh` — `config-v1.toml` = live config with exactly the four V1 values from the 2026-09-11 pre-cutover config (both `bridge_contract`, `start_block`, `max_log_block_range`, the three signer paths `/robinhood`). glc-admin ONLY; never the daemon.
2. `sudo 11-v1-config-install.sh` — installs it and runs the read-only V1 preflight with the signer tokens loaded the way `launch.sh` loads them. Expected: `signer_quorum_available` PASS, `contract_route_*` FAIL (open — the thing being closed). If the quorum FAILS, the V1-bound signer instances are gone and this phase is BLOCKED on the custody-domain operators.
3. `sudo 20-v1-governance-dryrun.sh` — quorum, digest, before/after for: deposits pause, payouts pause, four route flags.
4. `sudo 21-v1-governance-execute.sh` — executes in that order, re-reads the contract after each, stops on any mismatch. `executeRefund`/`executeSettlement`/`executeAbandonment` stay available while paused (contract docs), so §2 is unaffected.
5. Align the public UI's contract address if it still names V1 (out of this repository).

## 2. Phase 2 — 4037 / 4038, proof-backed timeline

| | 4037 | 4038 |
|---|---|---|
| deposit tx | `0x15e8112d…809a97` block 60,705,212, 2026-09-12 01:10:23 UTC | `0x85dfdd39…2184c9` block 60,706,447, 01:12:29 UTC |
| contract | **V1** `0x1753dDA0256A2cB10B44497ACeA9650A1422f440` | V1 |
| depositor | `0x5376d8ff23B3eaEeef9Fbe7857Ebe4486191f53b` | same |
| amount | 300 GLC (3e20 wei; ledger gross 30,000,000,000 @8dp; fee 300 bps; net 291 GLC) | 135 GLC (net 130.95) |
| route | 4 = `RhnToSol`, destination `5730acf5…3d47` (Solana pubkey) | same |
| obligation | V1 #29, `DepositCreated` in the deposit tx | V1 #30 |
| events touching it since | **none** (no `ObligationSettled`/`RefundExecuted`/`DepositAbandoned` on V1 for #29/#30) | none |
| on-chain status now | **Pending (1)** | Pending (1) |
| ledger | folded 2026-09-12 01:54:57 (`fold_robinhood_deposit` → `ManualReview`, PR #93 `robinhood-recover-deposit` from the V1 config, four minutes after #93 merged), `auto_resume_hold` 2026-09-13 01:07:16 (one of the 56 operator holds), no destination txid, no `robinhood_transactions` row | same |
| "Pending → Settled" | is **V2** #29: `DepositCreated` `0x8dc4acfe…` (depositor `0x6b63d342…`, route 2, 20,000 GLC), `ObligationSettled` `0x4e329224…` block 61,243,083 (requestId `0x64cdfe3a…`) — this service's own RhnToGlc settlement | V2 #30: `0x5af13521…` / `0x9a14b3ee…` block 61,243,086 |
| predecessor/migration | V1 was replaced by V2 at 2026-09-11 21:17:51 UTC (V2 first log, block 60,568,283); V1 was **never** `commitMigration`'d — it still holds its reserve and accepts deposits (§1) | same |
| why no ledger operation | the ledger has none because none happened: V1 #29/#30 are untouched. The V2 #29/#30 operations belong to other requests | same |

**Classification: D → resolved to "C-adjacent: obligation NOT consumed, user received nothing, funds in V1 custody, recoverable."** No double-payment has occurred and none is possible: V1's `executeRefund` binds recipient = its own stored depositor and amount = its own stored principal, and refuses unless `Pending`.

Recovery (narrowest): `30-4037-4038-recovery-dryrun.sh` then, per request and after sign-off, `31-4037-4038-recovery-execute.sh <id>` = `manual-review-refund --config config-v1.toml --request-id N --execute` (records the REFUND decision, audited; `robinhood-refund` against the V1 deployment; contract-side `(ACTION_REFUND, requestId)` replay guard; ledger `robinhood_transactions` row; a rerun re-broadcasts identical bytes, never a second transfer). PROCESS (Solana release + V1 `executeSettlement`) would need a V1-bound daemon and is not offered.

Detection going forward: `glc-admin robinhood-obligation-audit` (+ `--contract` for V1), the daemon's 5-minute sweep, `/health robinhood_obligations_reconciled`, and the contract binding that makes this class of confusion a refusal rather than a payment (#101).

## 3. Phase 3 — abuse policy

Already in v30 (#100, merged): deterministic `rapid_burst_hold` (rolling window, per-source / per-destination / per-pair counts, config-driven, no addresses, no IP), `held_at`/`review_after = held_at + minimum_review_hold_secs`, never auto-resumed, survives restart, `review_after` enforced on PROCESS and on the REFUND decision (`--emergency` refund is the one early exit, audited as such); existing 56 holds migrate to `operator_hold` exactly (rehearsed on a copy of the pre-v30 backup: 56 → 56).

Added: the terminal closure (#103): `ManualReview → Closed` with `refunded_out_of_band` / `reconciled_to_chain` + mandatory reference; never a void. **No retention disposition**: the published Terms (2026-09-12, §7–§8) do not authorize closing an order without payout or refund with the principal kept — §8 caps the abuse charge at USD $25 and refunds the remainder. Required clause before any such disposition ships: *"Where an order is determined to be abusive under §5, Goldcoin may close the order permanently: no destination payout will be made, no refund will be issued, and the deposited amount will be retained by the Bridge reserve. Closure is an authorized-operator decision recorded in the Bridge's audit log and is not made before the minimum review period has elapsed."*

**Fee-bearing refund — BLOCKED, by design, not implemented.** Two hard dependencies do not exist and must not be invented:

1. **Neither chain accepts a partial refund.** `GlcRobinhoodBridge.executeRefund` requires `req.amount == ob.amount` ("No partial refunds. No on-chain refund fee."); the Solana program's `refund_withdraw` requires `amount == obligation.amount` ("a partial refund is not a refund"). A fee-bearing refund needs a NEW instruction/function on each chain (`refund_withdraw_with_fee` retaining `fee` in the reserve and recording it on the obligation; an EIP-712 `RefundWithFee` struct with the fee bound into the digest), audited and upgraded, before any off-chain arithmetic matters.
2. **No USD→GLC valuation source exists** in this repository or its configuration (no oracle, no price feed, no `[pricing]` section). Proposed shape, for approval: a `[abuse_fee]` section `{ usd_cents = 2500, source = "pyth"|"chainlink"|"operator_fixture", pyth_price_account | chainlink_aggregator, max_age_secs, max_confidence_bps, rounding = "ceil_fee" }`; the fee in GLC atomic = `ceil(usd_cents / price)`, capped at the principal, the price read at a named slot/round that is recorded in the refund row and printed by the dry run; a stale/unconfident read refuses. Operator-fixed prices are refused in production mode.

Until both exist, `fee_bearing_refund_supported = false` on `/status` and every route's capabilities, and the only refunds are full-principal.

## 4. Phase 4 — the Solana program

Proof: `solana program show 6tmLSP2j…` → ProgramData `268AcDD4…`, authority `9LdtdQsy…` (the deployer key, timelock NOT armed — direct deploy path per docs/30 §0), last deployed slot 442,649,805 (2026-08-29 16:16 UTC), 658,864 bytes, sha256 `76c0b517da69b5d8d1126d58051b8e385f5f53c04f319b772242a053ffe93ad2`. Discriminators in the dump (lddw-encoded): `release_from_reserve`, `deposit_to_reserve`, `record_goldcoin_completion`, `set_paused`, `set_limit`, `reset_rolling_volume_window`, `rebalance_withdraw`, `propose_upgrade` PRESENT; `refund_withdraw`, `treasury_withdraw`, `initialize_rebalance_policy`, `propose_rebalance_policy` ABSENT. The client's `refund_withdraw` discriminator is `3d18b2d16950a5c5`; the source implements it (`programs/…/instructions/refund_withdraw.rs`).

Review of `refund_withdraw` against the checklist: admin signature required and `bridge_config.admin` pinned; 2-of-3 threshold attestation over `(nonce, amount, destination, mint, reserve ATA, obligation_index, requester)` via the adjacent ed25519 instruction; protected minimum enforced; per-nonce PDA replay guard in the refund namespace (`NONCE_DOMAIN_REFUND`, deterministic from the request id); destination DERIVED as the requester's ATA (Anchor `associated_token::authority = requester`, `requester` address-pinned to the obligation's stored field) — not operator-supplied; amount must equal the obligation's — not operator-supplied; obligation must be `Pending`; bridge must be globally paused; **and, new, the obligation is marked `Refunded` (F-8)** so a second refund and a later completion are refused on chain. Tests: 16 refund + 6 completion (including second-refund-refused, refunded-cannot-complete), 72 unit, all program suites green.

Build (this branch, `anchor build`, Anchor 0.31.1 / Agave 2.1.21 / rustc 1.85): `target/deploy/glc_reserve_bridge.so` 819,032 bytes, sha256 `9563a955b343fc03878bce28a815a75d34b928b13a15c1d086ee7df2d6dfc9cb`; dispatches every client instruction (`the_built_program_dispatches_every_client_instruction`). Reproduce on the deploying host and compare the hash before deploying.

**Blocked on two external facts, verified 2026-09-13:**

- ProgramData has **658,909 bytes of space** — exactly the current ELF plus header. The new program needs 819,077. `solana program extend 6tmLSP2j… 200000 --url … -k <payer>` (≈1.4 SOL rent) must precede the deploy.
- The upgrade authority `9LdtdQsy…` holds **0.028 SOL**. The deploy buffer for 819 KB needs ≈5.7 SOL (returned when the buffer closes) plus fees. A funded payer (any key, `-k`) and the authority (`--upgrade-authority /etc/glc-bridge/keys/deployer.json`) are both required.

Runbook (docs/30 §2–§9, instantiated; scripts `6x-program-*.sh`): snapshot (dump + sha + `program show` + `show-config` pause flags) → global pause per docs/30 §3 (record prior `paused/release_paused/deposit_paused` — restore exactly those afterwards) → `program extend` → `program deploy` → `program dump` + sha compare → `glc-admin solana-program-compat` must show every instruction PRESENT → §5 `RebalancePolicy` init (required: the upgrade retires `rebalance_withdraw`, so treasury withdrawals need the policy; 2-of-3 attestation) → restore the recorded pause flags → `refund-manual-review --request-id <a refundable SolToGlc park>` dry run must reach a successful simulation → only then any real refund, each under its own pause window (`refund_withdraw` requires `paused`). Rollback: redeploy the snapshot `.so` (the `Refunded` tag is additive; a rolled-back program simply never writes it).

## 5. Deployment order

1. `sudo 00-phase0-snapshot.sh`
2. v30 (#98+#99+#100): `sudo v30/00-precheck.sh` → `sudo v30/40-migration-rehearsal.sh` → `sudo v30/50-deploy.sh` → `sudo v30/60-verify.sh` (rollback `70-rollback.sh --with-ledger`)
3. Phase 1 on V1: `10` → `sudo 11` → `sudo 20` → `sudo 21`
4. Phase 2 decision: `sudo 30` → per request `sudo 31 <id>`
5. v32 (#101 → #102 → #103 merged): build from main, `sudo 4x-deploy-v32.sh`, verify schema 32, `/status.solana_refund_supported=false` (truthful until §4), `/health` breaches `solana_refund_instruction_supported` (expected), `robinhood_obligations_reconciled` clean on V2, `--contract V1` audit shows #29/#30 consistent-pending
6. Admin UI (holds: reason/held_at/review_after/decision/availability; critical banner on `solana_program.refund_supported=false`; close dialog; no preselected action)
7. Program upgrade per §4 once funded
8. Re-probe: `/status.solana_refund_supported=true`; one dry-run refund reaching simulation
9. Only then reconsider reopening any admission.

## 6. Operator policy update (2026-09-13, evening) — ManualReview frozen by default

Supersedes the interim "refund support required before a route opens" rule.
Refund capability is REPORTED (`capabilities.refund_supported` beside
`settlement_supported`), never an availability gate: a route whose normal
settlement works stays open, and a deposit that cannot settle parks in
`ManualReview`, where — from schema v33 — it is FROZEN until an operator
acts.

- `bridge_settings.auto_resume_manual_review` (default `false`, persisted,
  audited on every flip, read fresh every tick): the only thing that lets
  the automatic recovery pass run at all. `glc-admin
  manual-review-auto-resume`, `GET/PUT /settings/manual-review-auto-resume`,
  the Admin UI toggle.
- Never auto-resumed whatever the switch says (candidate-filter invariant):
  `rapid_burst_hold`, `operator_hold`, any hold marker, `foreign_contract`.
- Allowlist when the switch is on (unchanged from v29, not broadened):
  `utxo_liquidity_low_at_fold`, `liquidity_buffer_low_at_fold` (gate open),
  `wallet_source_24h_limit`, `wallet_destination_24h_limit`.
- Per-row `manual_review_class` / `auto_resume_eligible` /
  `auto_resume_block_reason` on the admin listing; `/status` reports
  `manual_review_auto_resume_enabled`, `abuse_hold_enabled`.
- Cap-sized burst rule (`repeated_cap_sized_amount`): repeated max /
  near-max deposits on a route inside the window, from any wallets — the
  rotating-wallet pattern of 2026-09-12 (50 × exactly the per-transfer
  limit, up to 17 in one 15-minute window).
- CANCEL with the principal retained: state-machine support behind
  `[manual_review] retained_cancel_enabled = false` (abuse-only, after the
  minimum review, written approval required). **Not enabled**: the clause
  in §3 must be published first.

Route posture under the new rule: every route with a working settlement
path may operate once the frozen behaviour is deployed and proven —
SolToGlc and SolToRhn included (their Solana refund path stays a reported
`refund_supported=false` until the program upgrade). Reopening them is a
separate, later operator act (`route-admission-open`), not part of the
deploy.
