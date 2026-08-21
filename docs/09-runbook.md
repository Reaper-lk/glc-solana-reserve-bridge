# Operational Runbook (Draft)

Structured after the old bridge's `docs/runbooks.md` discipline: every procedure here should eventually be backed by an executable `glc-admin`/`glc-audit` command, asserted by CI to actually exist and behave as documented (reused practice — the old bridge's `runbook_commands.rs` caught real drift between docs and binaries repeatedly; ported to `service/tests/runbook_commands.rs`). [07-implementation-plan.md](07-implementation-plan.md) Phase 5 landed a first, deliberately partial set of real commands — see "Executable commands" below for exactly what exists today and what is explicitly still a paper procedure.

## Executable commands (Phase 5)

What actually exists, so this document never claims more than the binaries do:

- `glc-admin status --db PATH` — reserve snapshots (both directions, including cumulative accrued bridge-fee revenue — docs/20-bridge-fee.md) and the `ManualReview` backlog count.
- `glc-admin pause --db PATH --direction <goldcoin|solana> --note TEXT` / `glc-admin unpause ...` — this service's own local ledger admission gate (independent of the on-chain pause below).
- `glc-admin show-config --rpc-url URL` — decodes and prints the on-chain `BridgeConfig`.
- `glc-admin onchain-pause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT` / `glc-admin onchain-unpause ...` — submits the admin-gated-immediate `set_paused` instruction (docs/12-management-decisions.md's Phase 2 scoping decision: pause is admin-gated-immediate, not threshold-gated).
- `glc-audit --db PATH [--quiet]` — offline integrity auditor: re-verifies every frozen attestation-claim commitment plus `PRAGMA integrity_check`. Exit 0 = clean, 1 = findings, 2 = could not run.
- `scripts/backup-ledger.sh <db path> <backup dir>` — safe online SQLite backup (`sqlite3 .backup`, never a plain file copy) of the ledger, timestamped. Prints the backup's path on success.
- `scripts/restore-ledger.sh <backup file> <destination>` — restores a backup produced by `backup-ledger.sh`, after verifying `PRAGMA integrity_check` on it. Refuses to overwrite an existing destination.
- `scripts/run-audit-cron.sh <db path> <backup dir> [glc-audit path]` — the cron/systemd-timer entry point: takes a fresh backup, then runs `glc-audit` against it (not the live database — see the script's own comments). Exit code is `glc-audit`'s own; wire it directly into your scheduler's failure notification.
- `glc-admin rebalance-status --db PATH` — read-only imbalance assessment for both reserves against their own configured target/warning/critical thresholds (docs/22-production-readiness-review.md P1 "rebalancing").
- `glc-admin rebalance-list --db PATH [--direction <goldcoin|solana>] [--open-only]` — lists rebalance requests.
- `glc-admin rebalance-propose --db PATH --direction <goldcoin|solana> --kind <deposit|withdraw> --amount N --by IDENTITY --required-approvals N --note TEXT` — creates a request in `Proposed`.
- `glc-admin rebalance-approve --db PATH --id N --by IDENTITY` — records an approval; idempotent per identity.
- `glc-admin rebalance-reject --db PATH --id N --by IDENTITY --note TEXT` / `glc-admin rebalance-cancel ...` — terminal off-ramps before execution.
- `glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` — records evidence of a real transfer already authorized and executed through real custody tooling **outside this system** — this command (and this entire service) never constructs, signs, or broadcasts a fund-moving transaction itself. `tx_reference` is a Goldcoin txid or a Solana signature, as plain text, and is unique across every rebalance request ever recorded (structural replay guard).
- `glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N` — records the independently-observed real effect of the executed transfer and updates the cached reserve balance in the same step, so the next reconciliation tick does not misclassify it as an unexplained breach.
- `glc-admin rebalance-fail --db PATH --id N --by IDENTITY --note TEXT` — routes an executed-but-unconfirmed rebalance to manual resolution.
- `glc-admin custody-list --db PATH [--kind <attestation-rotation|vault-sweep>] [--open-only]` — lists custody transitions (docs/22-production-readiness-review.md P1 "key rotation / vault sweep tooling").
- `glc-admin custody-propose --db PATH --kind <attestation-rotation|vault-sweep> --old-identities CSV --new-identities CSV [--new-threshold N] --by IDENTITY --required-approvals N --note TEXT` — creates a transition in `Proposed`. `--new-threshold` only applies to `vault-sweep`.
- `glc-admin custody-verify-identity --db PATH --id N --by IDENTITY` — records that the claimed new signer identity/vault descriptor was independently verified. Required before any approval: `custody-approve` rejects anything still in `Proposed`.
- `glc-admin custody-approve --db PATH --id N --by IDENTITY` — records an approval; idempotent per identity.
- `glc-admin custody-reject --db PATH --id N --by IDENTITY --note TEXT` / `glc-admin custody-cancel ...` — terminal off-ramps before execution.
- `glc-admin custody-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` — records evidence of a real rotation/sweep already authorized and executed through real custody tooling **outside this system** — this command (and this entire service) never generates keys, signs, or performs a rotation/sweep itself. Enforces the "pause requirements" invariant as a precondition, not documentation: fails unless `GoldcoinReserve` is already paused (`vault-sweep`) or both reserves are already paused (`attestation-rotation`, since attestation authorizes both bridge directions). `tx_reference` is unique across every custody transition ever recorded (structural replay guard).
- `glc-admin custody-confirm --db PATH --id N --by IDENTITY` — records independent confirmation that the new custody identity is active and correct post-transition.
- `glc-admin custody-fail --db PATH --id N --by IDENTITY --note TEXT` — routes an executed-but-unconfirmed transition to manual resolution.
- `glc-admin custody-rollback --db PATH --id N --by IDENTITY --note TEXT` — records that a `Failed` transition's effect was reverted back to the old identity out of band; only ever an audit marker, never performs the rollback itself.

**Rebalancing's and key-rotation/vault-sweep's off-chain engineering layers are now both built** (the commands above: imbalance detection, proposal/approval/execution-evidence/confirmation state machines, identity-verification and pause-requirement gates for custody transitions, structural separation from settlement accounting, replay protection, reconciliation interaction, restart recovery, and audit trail). **No procedure exists yet** for staging the actual out-of-band collection of multi-operator signatures/approvals outside this CLI (the old bridge's equivalent depended on a P2P federation transport this bridge does not have and does not need — see IMPLEMENTATION_LOG.md's Phase 5 entry) — `custody-approve`/`custody-verify-identity` record decisions made elsewhere, they do not collect them. The dedicated on-chain `rebalance_deposit`/`rebalance_withdraw` instructions docs/03-architecture.md originally envisioned for the Solana leg specifically (an atomic, on-chain-enforced structural separation between a rebalance transfer and an arbitrary one) are also not yet built; until those exist, the real fund movement a rebalance or custody transition evidences is an ordinary SPL/Goldcoin transfer or key/vault change executed through whatever wallet/custody tooling already holds the relevant keys, not a bespoke program instruction. These gaps are named explicitly below wherever the procedure that needs them is described, so they stay visible rather than silently assumed away.

## Startup/commissioning sequencing (cold start)

**Required order when bringing up the daemon against a newly-funded or freshly-restarted reserve — do not start the daemon before this sequence completes:**

1. Fund the reserve (send GLC to the Goldcoin vault address; transfer the reserve mint's tokens into the Solana reserve token account).
2. Wait for that funding transaction to reach the same confirmation/finality depth reconciliation itself requires before it is trusted — Goldcoin: `vault_min_confirmations` confirmations on the funding UTXO, read the same way reconciliation reads it (`listunspent` against the vault address, filtered to `solvable` entries — importing a watch-only address triggers a node-side wallet rescan that is not guaranteed to be instantaneous, so confirming the block is mined is not sufficient by itself); Solana: `finalized` commitment on the reserve token account's balance.
3. Only then start `glc-bridge-daemon`.

**Why this matters, concretely**: reconciliation's fail-closed design (see [05-reserve-accounting.md](05-reserve-accounting.md)) has no exception for "just started, haven't observed reality yet" — the ledger's configured starting balance is treated as the baseline from the very first reconciliation tick. If the daemon starts before step 2 has genuinely completed, the very first reconciliation tick can read a real chain balance that has not yet caught up to the full funded amount, classify the entire un-caught-up portion as an unexplained drop, and auto-pause the reserve before the bridge ever processes a single request — a real, once-observed failure mode during this service's own load/soak testing (docs/22-production-readiness-review.md item 7, docs/24-load-soak-harness.md), not a hypothetical one. Because auto-pause is deliberately never automatic to clear (see "Auto-pause triggers" below), a cold-start breach like this requires a manual operator unpause even though nothing was ever actually wrong with the funds.

**Verifying step 2 without running the daemon**: `goldcoin-cli listunspent <vault_min_confirmations> 9999999 '["<vault address>"]'` and confirm the `solvable` entries sum to the funded amount; for Solana, poll the reserve token account at `finalized` commitment until its balance matches what was transferred.

## Reserve sizing

Per management's stated principle: **reserve levels should cover the largest expected net outflow between operational rebalances.** Concretely:

```
target_reserve(direction) =
    expected_peak_directional_volume_per_rebalance_interval
  + safety_margin
  + protected_minimum
```

`expected_peak_directional_volume_per_rebalance_interval` and `safety_margin` are operational judgment calls informed by observed volume once the bridge is live; no value is asserted here. `rebalance_interval` itself is a policy choice (fixed schedule vs. threshold-triggered) — see [12-management-decisions.md](12-management-decisions.md).

**`protected_minimum` for the pilot launch is approved: 20,000 GLC** (raw `20000000000`, 6 decimals) — see [22-production-readiness-review.md](22-production-readiness-review.md) P0-6's "Approved pilot bridge-policy parameters" for the full pilot policy table and where each value is consumed (this is the same value passed to `initialize` via `glc-mainnet-bootstrap --protected-minimum`). This is the on-chain floor releases are refused below — it does **not** by itself resolve the formula above: `target_reserve`/`warning_reserve`/`critical_reserve` (the off-chain service's own `reserve.{solana,goldcoin}` config) still need real expected-volume data before `expected_peak_directional_volume_per_rebalance_interval`/`safety_margin` can be set, and remain open (docs/12 item 5).

**Update 2026-08-21: exact pilot initial-funding plan set.** Planning
reference price: 1 GLC = $0.002160. Reserves split ~equally, ~$400 total
intended exposure:

| Reserve | Planned initial funding | Approx. value |
|---|---|---|
| Goldcoin L1 reserve | **92,600 GLC** | ~$200.016 |
| Solana GLC reserve | **92,600 GLC** | ~$200.016 |
| **Total** | **185,200 GLC** | **~$400.032** |

($200 / $0.002160 = 92,592.592593 GLC; rounded up to 92,600 GLC per side
for operational simplicity.) **This replaces the previous 200,000-GLC-
per-side pilot planning figure.** Still a plan, not funding that has
happened — nothing has been transferred yet.

**Update 2026-08-21: recalculated and approved.** `protected_minimum`
and `rolling_volume_limit` were sized against the old 200,000-GLC-
per-side plan and have been replaced with values recalculated against
the new 92,600-GLC-per-side plan:

- **`protected_minimum`: 50,000 GLC → 20,000 GLC** — roughly the same
  proportion of the reserve as before (~21.6% vs. ~25%), rounded down
  slightly to leave real usable liquidity for a reserve this small.
- **`rolling_volume_limit`: 100,000 GLC/24h → 50,000 GLC/24h** — the
  old value exceeded an entire single-side reserve outright and could
  never actually bind; the new value sits under the resulting usable
  liquidity while still being a real constraint.

Resulting usable/releasable liquidity per side: 92,600 − 20,000 =
**72,600 GLC**. Resulting max full-size (10,000 GLC) transfers per
rolling 24h: 50,000 / 10,000 = **5**. `min_transfer_amount` and
`per_transfer_limit` are unchanged. See
[22-production-readiness-review.md](22-production-readiness-review.md)
item 28 and P0-6 for the full reasoning.

## Confirmation-depth values (pilot, approved 2026-08-21)

**These are the actual values to put in the pilot's Goldcoin config
section — not a placeholder, not "TBD."** They are a deliberately
conservative, hand-picked interim choice for the bounded pilot
specifically, made **without** the real Goldcoin hashrate/historical
reorg-depth data docs/12 item 4 calls for — that data collection remains
open and is now explicitly a scale gate (see
[22-production-readiness-review.md](22-production-readiness-review.md),
"Pilot Launch Policy"). The reasoning for picking a number now rather
than waiting: the pilot's settlement speed does not matter at this
volume, so there is no cost to erring far on the side of caution, and an
explicit conservative number closes the one real up-to-the-reserve
attack mechanism (an under-confirmed deposit reorged out after the
Solana-side release already happened) that the earlier "no default"
stance otherwise left open indefinitely.

| Field | Pilot value | Reasoning |
|---|---|---|
| `confirmation_depth` (Goldcoin deposit finality — the security-critical one) | **200** | Chosen with a large margin over what a well-hashrate-secured chain would need, specifically because this repository has not reviewed Goldcoin's actual real-world hashrate/reorg history. Trades settlement latency for safety margin; acceptable because pilot volume/urgency is low. |
| `max_reorg_depth` (reorg-walk safety valve — halts rather than silently reconciling past this) | **250** | Set above `confirmation_depth` so the indexer can actually walk back and resolve an ordinary reorg approaching the finality depth, rather than hard-halting on anything close to 200; still bounded, so a reorg deeper than 250 correctly halts the indexer and pages an operator instead of being silently absorbed. |
| `vault_min_confirmations` (payout-side vault UTXO spendability) | **20** | Governs the bridge's *own* change/reserve outputs, not an external depositor's — a reorg here is an operational hiccup (resubmit), not a loss-cap issue, so it does not need `confirmation_depth`'s margin; still well above the `1`–`3` values used only in test fixtures. |

**These are pilot-interim values, not the final production numbers.**
Replacing them with values backed by real Goldcoin hashrate/historical
reorg data (docs/12 item 4) is required before reserves, limits, or
usage are increased past the pilot — it is a scale gate, not a pilot
launch blocker; see "Pilot Launch Policy" in
[22-production-readiness-review.md](22-production-readiness-review.md)
for the full reasoning. Update this table (and the deployed config) when
that data-driven pass happens — do not silently carry these numbers
forward into a scaled deployment.

## Threshold bands and responses

| Band | Condition | Automatic response | Operator action |
|---|---|---|---|
| Normal | `balance ≥ warning_reserve` | None | None |
| Warning | `critical_reserve ≤ balance < warning_reserve` | Alert fired | Plan a rebalance; no urgency |
| Critical | `balance < critical_reserve` (but `≥ protected_minimum`) | **Automatic directional pause** (new requests rejected on that direction; in-flight requests continue to settle) | Execute rebalance before unpausing |
| Floor breach | `balance` would drop below `protected_minimum` for a specific request | Request rejected at capacity-check time (never accepted in the first place — see [05-reserve-accounting.md](05-reserve-accounting.md)) | None routine; investigate if this triggers unexpectedly, since it implies capacity accounting drifted from the live balance |

## Rebalancing procedure

The off-chain engineering layer (state machine, approvals, execution-evidence recording, confirmation, structural separation from settlement accounting) is built and executable today via the `glc-admin rebalance-status`/`rebalance-list`/`rebalance-propose`/`rebalance-approve`/`rebalance-reject`/`rebalance-cancel`/`rebalance-record-executed`/`rebalance-confirm`/`rebalance-fail` commands above. **What's still a manual, out-of-system step is the real fund transfer itself** — this service never constructs, signs, or broadcasts one; step 4 below is performed entirely through whatever real Goldcoin/Solana wallet or custody tooling already holds the relevant keys, same as any other operator-initiated transfer, and only its evidence is recorded here.

1. Operator determines direction and amount needing rebalance — `glc-admin rebalance-status --db PATH` gives a read-only severity assessment (Normal/Warning/Critical) and a suggested deposit size against the operator's own configured `target_reserve`, computed from already-configured values only, never an invented one.
2. Stage the rebalance: `glc-admin rebalance-propose --db PATH --direction ... --kind <deposit|withdraw> --amount N --by IDENTITY --required-approvals N --note TEXT`. Creates a `Proposed` request — moves no funds, touches no settlement accounting.
3. Required custody-domain approvals collected: `glc-admin rebalance-approve --db PATH --id N --by IDENTITY`, once per approving identity, until `required_approvals` is reached (per the ratified trust model — e.g. 2-of-3 for whichever reserve is being topped up). The request moves to `Approved`.
4. **Execute the real transfer entirely outside this system**, through the real custody tooling for the relevant reserve (Goldcoin vault multisig / Solana reserve-authority-adjacent wallet), then record the resulting evidence: `glc-admin rebalance-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` (a Goldcoin txid or Solana signature). `tx_reference` is unique across every rebalance ever recorded — the same real transfer can never be recorded twice.
5. Once the real transfer is independently confirmed (on-chain/on-Goldcoin), record it: `glc-admin rebalance-confirm --db PATH --id N --by IDENTITY --observed-amount N`. This updates the cached `total_reserve_balance` in the same step, so the very next reconciliation tick sees an already-explained balance rather than misclassifying the confirmed, operator-authorized change as a breach. If the rebalance clears `critical_reserve`, any reserve-triggered pause on that direction still requires an explicit `glc-admin unpause`/`glc-admin onchain-unpause` — reconciliation and rebalancing never auto-clear a pause (see below), regardless of how healthy the balance now looks.
6. If the recorded transfer's effect is never confirmed, or is confirmed wrong: `glc-admin rebalance-fail --db PATH --id N --by IDENTITY --note TEXT` routes it to manual resolution rather than leaving it `Executed` indefinitely.

## Auto-pause triggers (directional, unless noted global)

| Trigger | Scope | Rationale |
|---|---|---|
| `balance < critical_reserve` | Directional | Sizing/liquidity protection |
| Reconciliation `BREACH` classification (unexplained delta beyond itemized in-flight tolerance) | Directional (or global if the discrepancy implicates shared infrastructure) | Unexpected mismatch must fail safe, never continue silently — see [05](05-reserve-accounting.md), [10-threat-model.md](10-threat-model.md) |
| Rolling volume limit exceeded | Directional | Anomaly/attack containment |
| Repeated `DestinationSubmissionFailed` beyond retry budget, same direction | Directional | Likely systemic (RPC outage, fee-market issue) rather than one-off |
| Attestation/vault signer quorum unreachable for a configured duration | Directional | Liveness failure in the authorization layer shouldn't silently degrade to fewer required signers |
| Any `ManualReview` classified as a security incident (see [10-threat-model.md](10-threat-model.md)) | Global | Default to maximum caution until scoped |
| Operator-invoked emergency stop | Global | Always available (`glc-admin onchain-pause --scope global --note ...` and/or `glc-admin pause --direction <goldcoin\|solana>` for the local admission gate), highest priority gate |

**Un-pausing** is always operator-controlled and requires a note, regardless of what triggered the pause — no automatic un-pause path exists for any trigger, to avoid a flapping balance or a transient reconciliation blip silently resuming settlement. This is a deliberate asymmetry (fast/automatic to pause, slow/manual to resume), consistent with the old bridge's asymmetric pause-authority pattern (ADR-0014 §7) applied at the operational layer instead of the governance layer.

## Key compromise response (draft, depends on ratified trust model)

Structure reused from old bridge's rehearsed compromise runbook, repointed at internal custody domains rather than federation members:

1. **Detect**: anomalous signing activity, reconciliation breach, or external report.
2. **Contain**: global emergency pause immediately — `glc-admin onchain-pause --scope global --keypair ADMIN_KEY --rpc-url URL --note "compromise response: containing"`.
3. **Assess**: determine which custody domain(s) are implicated; do not assume "one domain compromised" without checking whether threshold-worth of domains are affected.
4. **Rotate**: stage the transition off-chain first — `glc-admin custody-propose --db PATH --kind attestation-rotation --old-identities CSV --new-identities CSV --by IDENTITY --required-approvals N --note "compromise response"` (or `--kind vault-sweep --new-threshold N` for the Goldcoin leg), then `glc-admin custody-verify-identity` once the clean replacement key/vault in a fresh custody domain is independently checked, then collect `custody-approve`s. Once `Approved`: for the Solana leg, execute attestation-key rotation via the timelocked governance instruction (already built on-chain — `propose/execute/cancel_attestation_key_rotation`, Phase 2); for the Goldcoin leg, execute the sweep-to-fresh-vault transfer through real custody tooling (the old bridge's `sweep.rs` and its independent-commitment-re-derivation discipline, docs/01-reuse-inventory.md, is the intended shape for that transfer itself — not yet ported). Either way, `glc-admin custody-record-executed --db PATH --id N --by IDENTITY --tx-reference TEXT` requires the relevant reserve(s) already paused (enforced, not just documented) and only ever records evidence — this service never generates the new keys/vault or performs the rotation/sweep itself.
5. **Verify**: independent confirmation (a domain not implicated in the compromise) that the new keys/vault are correctly configured before un-pausing.
6. **Resume**: operator-controlled, with note, one direction at a time — `glc-admin onchain-unpause --scope <release|deposit> --keypair ADMIN_KEY --rpc-url URL --note "compromise response: resuming <direction>"`.
7. **Post-mortem**: written, includes whether the reconciliation/monitoring layer detected the compromise before or after external report — a gap here is itself a finding.

## Accrued bridge fees (no withdrawal procedure yet)

The 1% bridge fee (docs/20-bridge-fee.md) accrues on the SOURCE reserve's
row (`reserve_ledger.accrued_fees_atomic`, canonical units, visible via
`glc-admin status` and the `/metrics` endpoint) and stays there — this
phase has **no treasury wallet/address and no fee-withdrawal path**, by
design. Accrued fees are never automatically moved anywhere and are never
counted toward `available_capacity`/the reserve invariant; they are purely
an audit-visible running total. Standing up a withdrawal procedure (who
authorizes it, where funds go, how it's distinguished from a rebalance in
the ledger) is future work, not yet scoped here.

## Explicitly deferred to real operational experience

Exact reserve thresholds, rebalance cadence, rolling-volume window size, per-transfer limits: all configuration, none defaulted in this document, per the old bridge's precedent of refusing to assert production security parameters without operational data (`docs/custody.md`'s open items #7/#8 were left open for the same reason — better an explicit open decision than a silently wrong default). See [12-management-decisions.md](12-management-decisions.md).

**Update 2026-08-21: confirmation depths are no longer on this deferred list for the pilot specifically** — see "Confirmation-depth values (pilot, approved 2026-08-21)" above for the actual interim numbers now in effect. The *final*, historical-data-backed values remain deferred, per docs/12 item 4, and are a scale gate rather than a pilot concern.
