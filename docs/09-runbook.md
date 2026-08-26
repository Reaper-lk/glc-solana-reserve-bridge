# Operational Runbook (Draft)

Structured after the old bridge's `docs/runbooks.md` discipline: every procedure here should eventually be backed by an executable `glc-admin`/`glc-audit` command, asserted by CI to actually exist and behave as documented (reused practice — the old bridge's `runbook_commands.rs` caught real drift between docs and binaries repeatedly; ported to `service/tests/runbook_commands.rs`). [07-implementation-plan.md](07-implementation-plan.md) Phase 5 landed a first, deliberately partial set of real commands — see "Executable commands" below for exactly what exists today and what is explicitly still a paper procedure.

## Executable commands (Phase 5)

What actually exists, so this document never claims more than the binaries do:

- `glc-admin status --db PATH` — reserve snapshots (both directions, including cumulative accrued bridge-fee revenue — docs/20-bridge-fee.md) and the `ManualReview` backlog count.
- `glc-admin pause --db PATH --direction <goldcoin|solana> --note TEXT` / `glc-admin unpause ...` — this service's own local ledger admission gate (independent of the on-chain pause below).
- `glc-admin show-config --rpc-url URL` — decodes and prints the on-chain `BridgeConfig`.
- `glc-admin onchain-pause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT` / `glc-admin onchain-unpause ...` — submits the admin-gated-immediate `set_paused` instruction (docs/12-management-decisions.md's Phase 2 scoping decision: pause is admin-gated-immediate, not threshold-gated).
- `glc-admin set-limit --rpc-url URL --keypair PATH --field <min-transfer|per-transfer|protected-minimum|rolling-volume> --value N --note TEXT` — submits the admin-gated-immediate `set_limit` instruction (same posture as `onchain-pause` above). `--value` is atomic units of the Solana-side mint; `set_limit`'s on-chain check is against the NET release amount (`release_from_reserve`'s `limits.rs::enforce_transfer_amount`), so a `min-transfer` value must already account for the 6% bridge fee being deducted before comparison.
- `glc-admin retry-goldcoin-payout --config PATH --request-id N --note TEXT` — recovers a Solana->Goldcoin payout stuck in `goldcoin_payouts.state = 'Signed'` after its broadcast was rejected (e.g. request #8, Goldcoin RPC `-26: 64: non-mandatory-script-verify-flag (Non-canonical signature: S value is unnecessarily high)` — see the low-S signing fix). Never invoked automatically: `Orchestrator::tick_goldcoin_payouts` always skips a request that already has a `goldcoin_payouts` row, by design, so a stuck payout needs this explicit command, and this command alone. It never rebroadcasts the previously stored `signed_tx_hex` as-is, never selects a new UTXO, and never builds a second payout row — it independently reconstructs the exact same plan from the already-persisted `goldcoin_payouts`/`goldcoin_payout_inputs` rows (refusing on any mismatch against freshly recomputed request data, or if the reconstructed unsigned transaction does not byte-for-byte match what was originally built), re-runs the real independent multi-signer signing path (`signing::goldcoin_vault::independently_sign_all_inputs`, the same function a normal payout build uses), and only calls `Ledger::record_goldcoin_payout_broadcast` after the Goldcoin RPC actually accepts the resulting transaction (or reports it already known). If the broadcast fails again, the payout stays exactly in `Signed` and `bridge_requests` stays in `SettlementAuthorized` — nothing is marked done on a failed attempt. Safe to re-run: a payout already `Broadcast`/`Confirmed`/`Completed` is reported and left untouched. Unlike every other command above, this one needs `--config` (the same config file `glc-bridge-daemon` uses), not `--db` — recovery signs and broadcasts a real transaction, so it needs the configured vault signers and Goldcoin RPC, not just ledger access. The command prints whether the re-signed transaction differs from what was previously stored; if it does **not** differ, that's a strong signal the original rejection has a cause other than signature canonicalization, and needs separate investigation before assuming a retry will succeed.
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

## Goldcoin indexer initial checkpoint (added 2026-08-22)

**The problem this solves**: a brand-new `service.db_path` ledger has no
`goldcoin_indexed_blocks` rows at all, and `goldcoin::indexer::Indexer`
has always started a ledger in that state at height 0
(`Ledger::goldcoin_chain_tip() == None => start at 0`) — correct and
harmless against a regtest/testnet chain a few hundred blocks tall, but a
real launch blocker against the live production chain (~2.58M blocks at
time of writing): at this indexer's current per-block RPC rate, a full
resync from 0 would take many hours before the bridge could accept its
first deposit against the new reserve vault
(`ML79m57inAWBeqWfrXxXpi7ncA74k49GJa`). Goldcoin 0.15 does not support
`scantxoutset`, so there is no way to shortcut this by having the node
itself scan history for us.

**What the checkpoint means, precisely**: configuring one asserts "every
Goldcoin deposit *before* this height is intentionally outside the
bridge's supported history" — deposits *at* the checkpoint height itself
are indexed completely normally, exactly like any other block. This is
not a performance shortcut that might miss something; it is a stated,
operator-verified policy boundary. It is also consulted **only once**: the
moment the ledger has any indexed block at all (including right after a
checkpoint is first accepted), the checkpoint config is never looked at
again — the normal persisted cursor and reorg-detection logic always wins
from then on, exactly as it always has (`service/src/goldcoin/
indexer.rs::Indexer::tick`, `bootstrap_from_checkpoint_or_genesis`'s own
docs).

**Safety guard — do not skip this step.** Because Goldcoin 0.15 cannot
`scantxoutset` its own history, this service has no way to independently
verify that the configured vault never received a bridge deposit before
the checkpoint height — that is a claim only an operator can make, from
knowing the vault's real provenance (e.g. it is a freshly generated
address that has never appeared in any bridge configuration before this
launch). `initial_checkpoint_operator_acknowledged_no_prior_deposits`
exists specifically to make that claim explicit and machine-checked
(`false`, including simply leaving it unset, fails the whole checkpoint
closed — see the malformed-config behavior below); it is never inferred,
guessed, or defaulted to `true`.

### Exact operator procedure

1. **Get the live tip height** — run this against the SAME node the
   `[goldcoin].rpc_url` in the config file being commissioned will
   actually point at, not just any node claiming to be Goldcoin mainnet:
   ```
   goldcoin-cli getblockcount
   ```
2. **Pick a checkpoint height** at or below that tip. Using the tip
   itself is fine but leaves zero reorg buffer against the last few
   blocks; subtracting a modest safety margin (e.g. a few hundred blocks,
   comfortably above `max_reorg_depth`) is more conservative and is what
   was actually done for this vault's launch.
3. **Get that height's block hash**:
   ```
   goldcoin-cli getblockhash <HEIGHT>
   ```
4. **Verify the block independently** before trusting it — do not simply
   copy the hash from step 3 straight into config without looking at it:
   ```
   goldcoin-cli getblock <HASH>
   ```
   Confirm the returned `height` field matches what was requested, and
   that `confirmations` is comfortably above `max_reorg_depth` (a
   too-recent block is a poor checkpoint choice — see step 2).
5. **Confirm the vault has no prior bridge history.** This is the one
   step this service cannot do for you (no `scantxoutset` on Goldcoin
   0.15) — confirm from the vault's own provenance (e.g. it was generated
   fresh for this launch and has never been configured as a bridge vault
   before) that it received no bridge deposit before the chosen height.
6. **Configure height, hash, and the explicit acknowledgement together**
   in the service config file (`service/config.pilot-template.toml`'s
   commented-out `[goldcoin]` block shows the exact field names):
   ```toml
   initial_checkpoint_height = <HEIGHT from step 2>
   initial_checkpoint_hash = "<HASH from step 3/4>"
   initial_checkpoint_operator_acknowledged_no_prior_deposits = true
   ```
   All three must be set together — a partial pair (e.g. height without
   hash) is rejected at config-load time, before the daemon ever starts,
   never silently ignored or treated as "no checkpoint".
7. **Start the daemon.** Its first tick re-verifies the configured hash
   live (`getblockhash(height)`, exact byte-for-byte comparison — never
   trusting the config file alone) before indexing anything; a
   mismatch, an above-tip height, a malformed hash, or a missing
   acknowledgement all refuse the tick outright rather than silently
   falling back to height 0. Watch the first tick's log for the
   `"Goldcoin indexer verified an operator-configured initial
   checkpoint"` line confirming acceptance.

**This is a one-time procedure per ledger.** Once step 7's first tick has
indexed anything, the ledger is no longer "brand new" and this whole
config block is permanently irrelevant to it, even if left in the config
file — restarting the daemon, or ever re-running this procedure's steps
against the same `service.db_path`, has no effect once past that point.

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

**Update 2026-08-21: `service/src/config.rs`'s `reserve.{solana,goldcoin}.{target_reserve,warning_reserve,critical_reserve}` — pilot placeholder values set, resolving the previously-open item and the daemon-startup blocker it caused.** These are the off-chain reserve-ledger monitoring bands (docs/05-reserve-accounting.md's Normal/Warning/Critical/Floor-breach table), distinct from — but required to be consistent with — the on-chain `protected_minimum`. Since no real observed pilot volume exists yet, these are conservative, simply-reasoned placeholders sized directly off the approved 92,600 GLC reserve, not the `expected_peak_volume + safety_margin` formula above (that formula's inputs remain genuinely open, per docs/12 item 5, until real volume data exists — these placeholders are what let the daemon start in the meantime, not a claim that real volume analysis has been done).

**Critical unit-conversion note, checked directly against the code, not assumed:** the two `reserve.*` sections are **not** in the same raw-unit convention. `reserve.solana.*` amounts are raw SPL-token units, 6 decimals (`amount × 1,000,000` — same convention as the on-chain `protected_minimum`). `reserve.goldcoin.*` amounts are raw native-Goldcoin atomic units, **8 decimals** (`amount × 100,000,000` — confirmed against `service/src/goldcoin/deposit.rs::glc_to_atomic`, the same conversion the indexer itself uses for every real deposit it observes). The same GLC quantity is therefore a *different* raw integer on each side — using the 6-decimal conversion for the Goldcoin side (or vice versa) would silently misconfigure the reserve bands by two orders of magnitude without any error, since both are just `u64` fields to the parser.

| GLC amount (both sides) | `reserve.solana.*` raw (×1,000,000) | `reserve.goldcoin.*` raw (×100,000,000) | Reasoning |
|---|---|---|---|
| `protected_minimum` = 20,000 GLC | `20000000000` | `2000000000000` | Mirrors the approved on-chain floor exactly (P0-6) — the off-chain band must agree with the hard on-chain floor, not invent a different number. |
| `critical_reserve` = 30,000 GLC | `30000000000` | `3000000000000` | 10,000 GLC (one full max-transfer) of buffer above the hard floor before the auto-pause band engages — small but non-zero headroom, appropriate for a reserve this size. |
| `warning_reserve` = 100,000 GLC | `100000000000` | `10000000000000` | Set equal to the approved rolling 24h volume cap: if the reserve drops to the size of one full day's *legitimate maximum* volume, that's a reasonable, easy-to-explain point to start planning a rebalance. |
| `target_reserve` = 92,600 GLC | `92600000000` | `9260000000000` | The full initial funded amount — with no real volume history yet, "rebalance back to what we started with" is the simplest defensible target, not an invented number. |

`reconciliation_tolerance`: **0** (raw units) — no tolerance for unexplained drift at pilot scale; any discrepancy at all should surface, not be silently absorbed, matching the reconciliation design's own fail-closed intent (docs/05-reserve-accounting.md, docs/10-threat-model.md).

A checked-in template reflecting these exact values is at
[`service/config.pilot-template.toml`](../service/config.pilot-template.toml)
— every reserve-bounds/network/confirmation-depth field is a real
pilot value; every identity/endpoint field (RPC credentials, admin/
attestation/vault pubkeys, key paths) is an explicit
`<REPLACE_WITH_...>` placeholder, never a real or invented key. See
that file's own header comment for exactly what must be supplied
before it can be used for a real deployment, and "Attestation
signer provenance" below for the attestation-pubkey placeholders
specifically.

**One additional field this exercise surfaced, not previously
addressed in any confirmation-depth approval:**
`goldcoin.required_payout_confirmations` (consumed as
`required_goldcoin_confirmations` in `glc-bridge-daemon.rs`) — how
many confirmations *our own outgoing* Goldcoin payout needs before
being treated as settled, the outgoing-leg sibling of the incoming
`confirmation_depth`. This was never set previously (test fixtures
only ever used `3`, explicitly non-production). **Proposed pilot
value: 200 — the same conservative depth as `confirmation_depth`**,
for the same reason (no real Goldcoin hashrate/reorg data reviewed
yet) applied symmetrically to the outgoing leg. This is a new
value, not one of the three previously-approved confirmation
settings (`confirmation_depth`/`max_reorg_depth`/
`vault_min_confirmations`) — flagged here for explicit sign-off
rather than silently folded into "unchanged."

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

**Update 2026-08-22: `rolling_volume_limit` raised to the approved
pilot value of 100,000 GLC/24h (raw `100000000000`), GLOBAL and PER
DIRECTION — the same single `rolling_volume_limit` field bounds both
Goldcoin→Solana and Solana→Goldcoin volume, each tracked in its own
`RollingVolumeWindow` (see `programs/glc-reserve-bridge/src/limits.rs`);
there is no separate per-direction field to configure.** This is a
volume cap, not a transaction-count cap — a user may still bridge any
valid amount up to `per_transfer_limit` (10,000 GLC, unchanged) per
transfer, and successive transfers accumulate toward the 100,000 GLC
rolling-24h ceiling per direction. `protected_minimum` (20,000 GLC),
`per_transfer_limit` (10,000 GLC), `min_transfer_amount` (100 GLC), and
`rolling_window_seconds` (86,400) are all unchanged. `warning_reserve`
(table above) is raised to 100,000 GLC alongside it, per this section's
own "set equal to the approved rolling 24h volume cap" rule — a reserve
monitoring band, not a change to `target_reserve`/`protected_minimum`/
`critical_reserve` or to the actual funded reserve amount, none of
which moved.

Worth stating plainly rather than silently omitting: resulting
usable/releasable liquidity per side is still 92,600 − 20,000 =
**72,600 GLC** (unchanged, since neither the reserve plan nor
`protected_minimum` moved) — so, exactly as the 2026-08-21 update above
noted about the *original* 100,000 GLC value, this cap again exceeds
this reserve's own usable liquidity per side and is unlikely to ever
actually bind before usable liquidity itself becomes the limiting
factor at the current 92,600-GLC-per-side reserve size. Recorded here
as the approved policy value regardless, per explicit pilot-policy
sign-off — not a claim that it is the binding constraint at today's
reserve size.

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

## Attestation signer provenance (checked 2026-08-21)

**Status: UNCONFIRMED — human decision required before deployment.**
Three attestation pubkeys appear in this codebase
(`6b27qC3fxrReuU4hL6u8iZ9AwkdngnjDxXUPwicR8WLe`,
`G7dJ2HiEkcfJqtPGa8gQrErLaQfdZ7hcbnA173A8Y4yL`,
`4uYKxwpWrPDyoaxjmdmJoWYLxmq2AziNMctSjTDFmynT`), but only ever inside
one illustrative example bootstrap command (duplicated between
`docs/22-production-readiness-review.md` and
`service/src/bin/glc-mainnet-bootstrap.rs`'s own module doc comment).
No separate provenance/custody record anywhere in the repository
confirms these as real, intended production attestation signers rather
than an illustrative placeholder set. Both example commands now use an
explicit `<REPLACE_WITH_ATTESTATION_PUBKEY_N>` placeholder instead of
these three literals, so nothing is accidentally copied as if real.

**Before the real `glc-mainnet-bootstrap` invocation, supply:**
- 3 real production attestation pubkeys (2-of-3 threshold, per the
  approved pilot policy) — the signers authorizing GLC⇄SOL settlement.
- 3 real production Goldcoin vault pubkeys (2-of-3 threshold) — the
  payout-side custody signers.
- The real production admin pubkey.
- The real production submitter/fee-payer keypair (not a custody
  authority — see `Config::load_submitter`).

None of these were invented, guessed, or filled with placeholder/test
values anywhere production code or documentation reads from. Private
key material for any of the above is never generated or held by this
repository — only public keys are ever configuration inputs.

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

## Vault UTXO splitting (added 2026-08-24)

### Why this exists

`goldcoin::coin::select` already prefers a bounded combination of smaller mature vault UTXOs over one oversized one when a smaller combination exists (the fix for the incident where a ~9,900 GLC payout consumed the vault's one ~100,000 GLC UTXO — see the "Reserve sizing" section above and `docs/22-production-readiness-review.md`). That fix cannot manufacture liquidity that doesn't exist: if the vault's mature UTXOs are still concentrated in one very large one, a payout still has no choice but to consume it, producing a large immature change output and temporarily starving spendable reserve below `protected_minimum` — exactly the scenario that recurred in production even after the selection fix (request #18: a ~90,100 GLC UTXO consumed for a ~9,900 GLC payout, mature reserve dropping below the 20,000 GLC floor and auto-pausing again).

`glc-admin split-vault-utxo` answers this proactively: an operator, having noticed (via `glc-admin status`'s `immature_vault_utxo_total`/vault UTXO inspection, or after an incident like the one above) that the root vault's mature liquidity is concentrated in one disproportionately large UTXO, can fragment it into several smaller ones ahead of the next large-vs-small payout collision — all before any payout is even attempted.

### What it does, and does not, change

- Uses the exact same 2-of-3 vault signer path (`VaultSigner`, `crate::goldcoin::multisig::assemble`) every real payout uses — `DevVaultSigner` in dev/pilot mode, `RemoteVaultSigner` in production mode (`operators.mode` in the service config). Signer secrets/private keys never enter this command's process; production signing happens entirely on the remote signer's own side, identical to `retry-goldcoin-payout` and the orchestrator's own payout path.
- Every output of a split pays the vault's own script — never a derived per-request address, never an external destination. Splitting is scoped to root-vault UTXOs only; a per-request derived deposit-address UTXO is refused (it's already narrowly scoped to one funding request).
- Does not touch `vault_min_confirmations`, the hard reserve invariant `reconcile()` enforces, or `coin::select` itself. It only ever adds more, smaller mature UTXOs for that unchanged selector to choose from later.
- Idempotent and auditable: a dedicated `vault_utxo_splits` table (`UNIQUE(source_txid, source_vout)`, `Built -> Signed -> Broadcast` state machine, mirroring `goldcoin_payouts`) means a given outpoint can be split at most once, structurally — re-running the command against an already-split outpoint is a safe no-op, reported and left alone.
- Every one of the (2 of 3) signers independently re-derives the entire plan — amount, chunk count, chunk sizes, and the reserve-safety check below — from its own ledger view before contributing a signature (`crate::signing::goldcoin_split::LedgerSplitSource`), the same "never trust a handed-in plan" discipline every other fund-moving operation in this service already has.

### The reserve-safety check (unconditional, no override)

Splitting spends the source UTXO's full value; until every resulting output re-matures (`vault_min_confirmations`), that value briefly leaves spendable/mature reserve — the exact mechanism that caused the incident above. Before ever contacting a signer, and again independently by each signer:

```
mature_reserve_after = current_mature_reserve_balance - source_utxo_amount
refuse unless mature_reserve_after >= protected_minimum + pending_obligations
```

This is the same formula `reconciliation::reconcile` enforces reactively (see "Reserve sizing" above), checked here proactively. There is no `--force` or other override — if liquidity is too tight to safely split right now, the answer is to wait or replenish reserve first, not to bypass the check.

### Exact operator procedure

1. Identify the oversized UTXO — `glc-admin status --db PATH` for a quick reserve overview, or direct `vault_utxos` inspection for the specific `txid`/`vout` and amount. There is no auto-pick; the operator must name the exact outpoint.
2. Dry run: `glc-admin split-vault-utxo --config PATH --txid TXID --vout N --note TEXT` (no `--execute`). Prints the full plan — source UTXO, output count, each output's amount, total fee, and the reserve-safety check (current mature reserve, protected minimum, pending obligations, mature reserve after the split, and PASS/FAIL) — without contacting any signer or broadcasting anything.
3. Review the printed plan. A `FAIL` safety check refuses regardless of `--execute`; wait for reserve to recover (a rebalance deposit, or a prior split's outputs maturing) before retrying.
4. Execute: re-run the identical command with `--execute` appended. Prints the same plan first, then contacts the configured vault signers, assembles and broadcasts the transaction, and prints the resulting txid.
5. Re-running the same command again (with or without `--execute`) after a successful split is a safe no-op — the source outpoint is already recorded in `vault_utxo_splits` and is reported, not re-processed.

`--chunk-target-atomic` defaults to 1,250,000,000,000 (12,500 GLC, 8 decimals) — chosen with headroom over the current 10,000 GLC per-transfer limit's ~9,400 GLC maximum net payout, so a single resulting chunk can always individually cover the largest possible payout via `coin::select`'s cheap single-UTXO paths without needing a multi-input combination. **Revisit this default if `per_transfer_limit` (on-chain, `glc-admin set-limit --field per-transfer`) ever changes materially.** Every output is required to be at least 1,000 GLC (`goldcoin::split::MIN_CHUNK_FLOOR_ATOMIC`) — a UTXO too small to produce at least 2 useful chunks at the requested target is refused outright (`SplitError::NotWorthSplitting`/`ChunkBelowFloor`), rather than producing a fragment too small to matter.

### Split outputs and the indexer (fixed 2026-08-25)

A split transaction's outputs all pay the vault's own script with no OP_RETURN, by construction — indistinguishable, to `goldcoin::indexer`'s legacy request-binding check (built for GlcToSol deposit attribution, unrelated to splits), from an unexplained vault payment. Before this fix, every split output was recorded in `unmatched_goldcoin_deposits` with `reason = 'no_request_binding'`, a false alarm: `vault_utxos`/reserve capacity were never actually wrong, since those are populated separately by `Orchestrator::tick_vault_utxos` (`list_unspent`-based), independent of this per-block scan.

The indexer now checks, for any vault-owned output with no usable request binding, whether `(txid, vout, amount)` exactly matches an expected output of a known `Broadcast` `vault_utxo_splits` transaction (`goldcoin::split::matches_expected_split_output`, reproducing the exact deterministic output distribution from the split's own persisted `source_amount_atomic`/`fee_atomic`/`chunk_count` — never re-derived from a possibly-since-changed `fee_rate_per_kb`). An exact match is logged and skipped, never recorded unmatched; anything else — a genuinely unexplained payment, or even a single mismatched output on an otherwise-real split — is recorded exactly as before.

A row recorded before this fix shipped stays recorded (never auto-cleaned by a rescan): `glc-admin reconcile-unmatched-deposit --db PATH --txid TXID --vout N --note TEXT` marks it reconciled, using the identical exact-match check, refusing (no override) if it doesn't match a known split output. Never deletes the row — reconciliation is additive (`reconciled_at`/`reconciliation_note` columns), preserving full audit history either way.

## UTXO liquidity (permanent fix, added 2026-08-26)

### The incident this closes

Vault UTXO splitting (above) reduces the odds of a large-UTXO/small-payout collision, but does not change what happens to the *change* a payout itself produces. Production still hit this: a ~95,000 GLC vault UTXO was manually split into 20 chunks of ~4,770 GLC. Traffic then consumed more than 20 of those mature chunks — one per Solana->Goldcoin payout — faster than their single, large change outputs could clear `vault_min_confirmations` (6). Funds were never at risk (every atomic unit was accounted for, either spent to the destination or sitting in this service's own broadcast change), but the *mature, spendable* UTXO pool collapsed toward zero, `reconcile()`'s hard invariant tripped, and Solana->Goldcoin auto-paused. Manually pre-splitting more chunks only raises the number of payouts it takes to reproduce the same collapse — it does not change the shape of the problem: a payout was still building **one** oversized change output per transaction, so mature liquidity was always one confirmation-depth away from starving under sustained traffic.

### What changed

1. **Deterministic change fan-out** (`goldcoin::coin::finalize_fanout`, replacing `finalize` for real payout construction — `finalize` itself is untouched and still used by `glc-rebalance-withdraw`'s manual single-change flow). Instead of one large change output, a payout with meaningful leftover value splits it into multiple vault-owned outputs sized around `goldcoin.change_fanout_target_atomic` (production default: sized off the *current* 1,880 GLC maximum net payout, not the stale 10,000 GLC historical limit), capped at `goldcoin.change_fanout_max_outputs`. It reuses `goldcoin::split::distribute_evenly` — the same near-equal-integer-division formula `split-vault-utxo` already uses — rather than a second, inconsistent splitting implementation. The candidate output count is reduced (never increased) until every output clears `dust_threshold`, exactly generalizing `finalize`'s existing single-change dust behavior; the real fee for that exact output count is recomputed at each step, never assumed. Value is conserved exactly; every output pays the vault's own script; the algorithm is a pure function of already-independently-derived inputs, so all 2-of-3 signers reproduce a byte-identical transaction without coordinating.
2. **`PayoutPlan.change_atomic: u64` -> `change_outputs: Vec<u64>`** (with `total_change_atomic()` for the aggregate). `goldcoin_payout_change_outputs` (new table, schema v12) persists each output's amount and order; `goldcoin_payouts.change_atomic` is kept as the SUM for backward compatibility with existing queries.
3. **UTXO pool health accounting** (`Ledger::utxo_pool_health`) distinguishes, at read time, real spendable liquidity from value merely *waiting*:
   - **Reserve value** — `total_reserve_balance` (unchanged meaning).
   - **Mature spendable capacity** — `mature_available_atomic` / `available_utxo_count`: the exact pool `coin::select` draws from right now.
   - **Temporarily immature internal change** — `own_unconfirmed_change_atomic` / `unconfirmed_change_utxo_count`: value this service already knows is its own broadcast-but-immature payout change (any `vault_utxos` row whose `txid` matches a known `goldcoin_payouts` broadcast — the external destination output is never a watched address, so this match is unambiguous). Never counted as spendable capacity; also folded additively into `pending_destination_settlement_amount` so reconciliation's "unexplained drop" check stops seeing it as unexplained the moment a payout leaves `Broadcast` state, without weakening the hard invariant itself.
   - **UTXO liquidity** — the count-based figures above, a faster-reacting leading indicator than either value figure: the accounting can look healthy while the pool itself is down to one oversized UTXO.
   `glc-admin status` prints all four; `/health`'s Prometheus output exports each as its own `glc_goldcoin_utxo_pool_*` gauge plus a `glc_goldcoin_utxo_pool_warning` gauge — deliberately a gauge, never an `Invariant`, so a thin-but-self-recovering pool never flips `/health` to 503 or reads as "Goldcoin reserves disappeared."
4. **Admission backpressure before exhaustion** (`goldcoin.utxo_pool_min_available_count`, `reserve_ledger.utxo_pool_min_available_count`/`utxo_pool_warning_count`, set via `Ledger::set_utxo_pool_thresholds` at every daemon startup). `fold_sol_deposit` now also refuses to admit a new SolToGlc obligation — routing it to `ManualReview` with reason `utxo_liquidity_low_at_fold` (added to the resumable-reason allowlist, same as the three pre-existing fold-time reasons) — once live `available_utxo_count` would fall to or below the configured floor. `0` disables it (default off; no behavior change on upgrade). Recovers automatically once a payout's change matures back past `vault_min_confirmations` and `available_utxo_count` rises again — no operator action needed, mirroring the existing `admission_closed`/`paused` distinction: this is a *third*, independent, physical-liquidity-aware admission gate, not a replacement for either.

### Tuning `utxo_pool_min_available_count` — this is vault-shape-specific, not a universal constant

There is no single safe default: the floor must engage *before* `reconcile()`'s own hard invariant would trip, and that break point depends on the relationship between actual vault chunk size, per-payout net amount, and the protected minimum. Worked from the incident's own numbers (4,770 GLC chunks, 1,880 GLC maximum net payout, 20,000 GLC protected minimum, ~75,400 GLC initial slack above the floor): each single-UTXO payout removes one ~4,770 GLC chunk from the mature pool *and* commits ~1,880 GLC of `pending_obligations` against it, so the hard invariant's own survival limit is `floor(75,400 / (4,770 + 1,880)) = 11` payouts fully admitted — count-based backpressure at the shipped default of 8 free chunks remaining would only start blocking at payout 12, one *past* that break point.

**This was empirically verified, and has since been fixed at its root cause** — see "PR #35 maintainer-review fixes" below. `service/tests/utxo_liquidity_production_tuning.rs::test_prod_defaults_floor_8_no_longer_breaches_thanks_to_the_sticky_pause_fix` (originally named `..._floor_8_breaches_before_backpressure_engages`, before the fix) still runs the literal historical shipped default (`utxo_pool_min_available_count = 8`, `vault_min_confirmations = 6`, `fee_rate_per_kb = 100_000` — the real pilot-template fee rate, not a toy value) against this exact vault shape, but now proves the opposite of what it originally found: obligations 0-11 (12 total) are admitted and finalized, and reconciling before obligation 12 no longer finds a breach — the shortfall (observed mature balance 38,160 GLC vs. protected_minimum 20,000 + pending_obligations 22,560) is fully explained by known internal change, so `reconciliation::reconcile`'s hard invariant holds and the direction never pauses. Count-based backpressure now gets to be the thing that actually engages at index 12, correctly reported as `utxo_liquidity_low_at_fold`, never masked by a spurious `reserve_paused_at_fold`.

`service/tests/utxo_liquidity_production_tuning.rs::test_prod_defaults_recovery_after_maturity_diagnostics` confirms the flip side: against a correctly-sized pool (9 UTXOs, same 20,000 GLC protected minimum), `floor = 8` engages exactly at the floor with the correct `utxo_liquidity_low_at_fold` reason, well clear of the hard invariant, and admission recovers automatically the moment change matures — so `8` is not universally wrong, only for a vault carrying enough *total* balance relative to its protected minimum to let many admissions through before the count-based floor would matter. **Recompute the floor for the vault's actual current total mature balance and chunk size, not just its chunk size in isolation**, before trusting the shipped default; `goldcoin.utxo_pool_warning_count` should sit comfortably above `utxo_pool_min_available_count` so an operator sees the warning gauge well before backpressure itself engages.

**Final recommendation (2026-08-26):** for a vault currently shaped like the incident (many ~4,770 GLC chunks against a 20,000 GLC protected minimum), configure `utxo_pool_min_available_count = 10` — not the shipped default of `8`, and higher than the `9` initially considered — deployed in `service/config.pilot-template.toml` and validated by `service/tests/utxo_liquidity_incident.rs`'s Tests A-D and by `service/tests/utxo_liquidity_production_tuning.rs::test_prod_recommended_floor_10_survives_the_25_burst_with_margin` (the same 25-obligation burst, at the real production fee rate): backpressure engages at obligation 10, leaving a full payout of margin before the hard invariant's own 11-payout survival limit, and the hard invariant never breaches. `utxo_pool_warning_count = 15` is kept as-is (it already sits comfortably above 10). The `2,500 GLC` change-fanout target and `change_fanout_max_outputs = 10` also stay as shipped: `test_change_outputs_for_a_very_large_97000_glc_input` shows the `change_fanout_max_outputs = 10` cap (not the target) is what determines output size once a UTXO is very large (~9,512 GLC per output on a ~97,000 GLC input, close to the original incident's own manually-split chunk size) — raising `change_fanout_max_outputs` would shrink those outputs further at the cost of a larger transaction; the target size itself is already correctly production-aware for the common case (see the same test file's `test_change_outputs_for_a_typical_4770_glc_input`, which produces exactly 2 change outputs of ~1,445 GLC each from a 4,770.8999317 GLC input).

**Separately:** `vault_min_confirmations = 6` was used in these tuning tests to match the incident's own stated assumption, but the deployed pilot template currently sets `vault_min_confirmations = 20` — an explicitly approved, reasoned pilot-interim value (see "Confirmation-depth values" above), not a placeholder. Changing it to `6` is a distinct decision from anything in this section and has not been made here; it needs the same explicit sign-off process that value's own guard comment calls for, not a silent edit alongside an unrelated liquidity-tuning change.

### PR #35 maintainer-review fixes (2026-08-26)

A maintainer review of this fix's own PR surfaced four findings, all fixed on the same branch before merge:

1. **Safe default.** `default_utxo_pool_min_available_count` (`service/src/config.rs`) shipped as `8` — the exact value this PR's own tests proved insufficient. Fixed to `10`, matching the "Final recommendation" above; `missing_utxo_liquidity_config_defaults_to_the_verified_safe_floor` (`service/src/config/tests.rs`) proves a config file predating this fix (none of the 4 new fields present) now loads with the safe value.
2. **Manual-review resume must respect UTXO liquidity.** `Ledger::resume_manual_review_sol_to_glc` used to check only the value-based reserve invariant before moving a parked `SolToGlc` request back to `SourceFinalized` — never the count-based `utxo_pool_min_available_count` gate `fold_sol_deposit` applies to a brand-new obligation. An operator resuming a `utxo_liquidity_low_at_fold` request (or `glc-admin open-admission`, which has the identical gap) the moment value accounting looked sufficient — while the mature UTXO count was still at or below the floor — could re-admit exactly the demand backpressure exists to hold back. Fixed: resume now re-runs the identical count-based check first, refusing with a dedicated `LedgerError::UtxoLiquidityLow` (leaving the request untouched, reserving nothing) while liquidity is still low, and succeeding normally the instant it recovers — see `service/tests/manual_review_resume_liquidity.rs` (Tests A-E). `glc-admin open-admission`'s equivalent gap is tracked as a known follow-up, not yet fixed on this branch.
3. **The sticky-pause path for explained internal change.** `reconciliation::reconcile`'s hard invariant (`observed_balance >= protected_minimum + pending_obligations`) only ever looked at the raw mature balance — so the exact chunk consumed to cover a much smaller payout could show up as an unexplained shortfall the instant reconciliation ran, even though every atomic unit was known, ledger-tracked, unconfirmed payout change, auto-pausing a reserve that was never actually short. Fixed: the hard invariant now adds `Ledger::own_unconfirmed_change_atomic` (GoldcoinReserve only; always `0` for SolanaReserve) to `observed_balance` before comparing against `protected_minimum + pending_obligations` — grounded entirely in independently-observed chain state matched against this service's own already-broadcast payouts, so it can never paper over genuine, unexplained loss. See `service/tests/utxo_liquidity_incident.rs`'s Test G.
4. **Signer/config-mismatch diagnostic.** `glc-bridge-daemon` now logs the effective `utxo_pool_min_available_count`/`utxo_pool_warning_count`/`change_fanout_target_atomic`/`change_fanout_max_outputs`/`vault_min_confirmations` at startup (`tracing::info!`), so an operator can directly diff what each independent signer instance actually loaded rather than only seeing an opaque stuck-payout/signing failure if two signers' configs silently drift. This is a visibility improvement only — the existing cryptographic signature-verification-at-assembly behavior that already fails closed on a real mismatch is unchanged.

### Batching — audited, deferred (not implemented in this pass)

Paying several finalized SolToGlc obligations in one Goldcoin transaction (one recipient output per obligation, fragmented vault change) would reduce UTXO churn further and was explicitly considered. Not implemented here: `goldcoin_payouts.request_id INTEGER PRIMARY KEY` structurally enforces exactly one payout per request throughout persistence, verification, and independent re-derivation — batching would need a genuine schema/protocol change (a payout-to-requests join, multi-request signer re-derivation, and multi-request replay/accounting semantics), not an incremental change alongside fan-out. Change fan-out + admission backpressure close the production incident on their own (see the regression tests below); batching remains a well-scoped, separately-reviewable follow-up rather than something to fold in here.

### Regression coverage

`service/tests/utxo_liquidity_incident.rs` reproduces the production incident directly, using `utxo_pool_min_available_count = 10` (the final recommended production value for this vault shape, not the shipped default): a burst of 25 consecutive 2,000 GLC gross obligations against a freshly-split pool (proving the service neither misclassifies reserve as unexplained-zero nor exhausts liquidity, applying backpressure before the hard invariant could ever trip instead); automatic admission recovery once change matures; several full maturity cycles with exact conservation and no permanent pause; randomized payout sizes with no double-spend and an always-preserved protected floor; two independent signers re-deriving a byte-identical multi-change transaction; and a daemon restart mid-fan-out reconstructing state correctly from Goldcoin RPC plus the ledger alone.

`service/tests/utxo_liquidity_production_tuning.rs` runs the same vault shape and fee-rate-realistic numbers against the HISTORICAL shipped config default (`utxo_pool_min_available_count = 8`, now superseded by `10` — `fee_rate_per_kb = 100_000`) to prove it no longer breaches or pauses post-fix (see "PR #35 maintainer-review fixes" above), confirms the mechanism works correctly on an appropriately-sized pool, validates the final `10` recommendation itself against the real production fee rate, verifies the real change-fan-out output shapes for both a typical ~4,770 GLC input and a very large ~97,000 GLC input, and verifies the production fee calculation at the real fee rate.

`service/tests/manual_review_resume_liquidity.rs` covers the resume-must-respect-UTXO-liquidity fix directly: a resume attempt refused while the pool sits at the floor; no duplicate obligation or payout created across repeated refused attempts; the triggering payout's change maturing and the pool recovering; resume succeeding normally once it does; and the protected reserve invariant never breaching throughout.

## Admission control (Solana->Goldcoin) (added 2026-08-24)

### Why this exists

The local ledger pause (`glc-admin pause`/`unpause`, above) and payout processing were never actually the same thing: `Orchestrator::tick_goldcoin_payouts` has never checked `paused` — it always continues building/signing/broadcasting for any request already `SourceFinalized`, regardless of pause state. The ONLY thing `paused` gates is `Ledger::fold_sol_deposit`'s decision to admit a newly observed on-chain SolToGlc obligation (`SourceFinalized`) versus park it (`ManualReview`). Because that's the single lever, an operator recovering from an incident (e.g. the vault-UTXO-splitting scenario above) who calls `unpause` to let the reserve return to normal simultaneously reopens admission for brand-new deposits — right when reserve headroom is thinnest, racing the still-draining backlog and risking an immediate re-pause.

`admission_closed` (`reserve_ledger`, separate from `paused`) fixes this by giving admission its own, independent, operator-only switch. **Scoped to Solana->Goldcoin only** — `glc-admin close-admission`/`open-admission --direction goldcoin`, since that's the direction `fold_sol_deposit` actually checks; `--direction solana` is refused with a clear "not implemented in this version" error rather than silently doing nothing.

### What it does, and does not, change

- Only `fold_sol_deposit`'s admission decision reads `admission_closed`. Both gates (`paused` and `admission_closed`) must be clear for a new obligation to be admitted — closing either one alone is enough to route a new fold to `ManualReview`; the pre-existing `paused` behavior is completely unchanged.
- Payout processing, confirmation tracking, the 2-of-3 signer path, reconciliation's breach formula, the rolling-volume quota, and the on-chain program are all untouched. An already-`SourceFinalized`/`SettlementAuthorized`/`DestinationSubmitted` request is never affected by `admission_closed` in any way — it keeps processing exactly as it always has.
- **No automatic reopen, and nothing automatically closes it either**: reconciliation and the rolling-volume quota continue to only ever touch `paused`, exactly as before. `admission_closed` changes ONLY via an explicit operator command.
- **No manual DB editing** — both directions go through `Ledger::set_admission`, never a raw `UPDATE`.

### Exact operator procedure

1. `glc-admin close-admission --db PATH --direction goldcoin --note TEXT` — always allowed. New SolToGlc deposits now fold into `ManualReview` instead of `SourceFinalized`; nothing about already-accepted requests changes.
2. Let already-accepted obligations continue draining normally (no action needed — payout processing was never gated by admission or pause in the first place).
3. When ready to accept new transfers again: `glc-admin open-admission --db PATH --direction goldcoin --note TEXT`. Refuses unconditionally (no override) unless `GoldcoinReserve`'s hard invariant currently holds (`balance >= protected_minimum + reserved_liquidity`, the same check `reconciliation::reconcile` enforces).
4. `glc-admin status --db PATH` reports `admission_closed=<bool>` per direction alongside the existing `paused=<bool>`. The public `/status` endpoint exposes the Solana->Goldcoin side as `sol_to_glc_admission_open` — a UI should read `false` there as "not accepting new transfers right now" (maintenance), distinct from `sol_to_glc_available` being `false` for reserve-health/quota reasons.

### Resuming an individual request parked in ManualReview

`fold_sol_deposit` routes a new SolToGlc obligation to `ManualReview` (never dropped — the Solana-side deposit is already real and irreversible) whenever `admission_closed`, `paused`, or insufficient capacity was true at the exact moment it was observed. Once the underlying condition clears, that specific request does not automatically resume — `glc-admin resume-manual-review --db PATH --request-id N --note TEXT` moves it back to `SourceFinalized` (reserving its capacity, exactly as a successful fold would have) so normal processing picks it up.

Scoped narrowly and refuses (no override) unless ALL of: the request is `SolToGlc` and currently `ManualReview`; its `manual_review_note` is one of the four known fold-time reasons (`admission_closed_at_fold`/`reserve_paused_at_fold`/`insufficient_capacity_at_fold`/`utxo_liquidity_low_at_fold` — never some other `ManualReview` cause); its source deposit is already finalized; it has no `goldcoin_payouts` row or `destination_txid` yet; the mature Goldcoin UTXO count is still above `utxo_pool_min_available_count` (the identical count-based gate `fold_sol_deposit` applies to a brand-new obligation — refuses with `LedgerError::UtxoLiquidityLow` otherwise, added by the PR #35 maintainer-review fix above); and reserving its capacity now would not breach the `GoldcoinReserve` invariant (the same `available_capacity` check `create_request`/`fold_sol_deposit` use to admit anything new). Deliberately does NOT check `admission_closed`/`paused` — admission may stay closed while this resumes something already accepted, since it never admits anything new. Idempotent: re-running it on an already-resumed request, or retrying while UTXO liquidity is still low, is a safe no-op either way. Preserves the request's id and `source_obligation_index` — it transitions the existing row in place, never creates a new one, so a duplicate obligation is impossible by construction.

## Auto-pause triggers (directional, unless noted global)

| Trigger | Scope | Rationale |
|---|---|---|
| `balance < critical_reserve` | Directional | Sizing/liquidity protection |
| Reconciliation `BREACH` classification (unexplained delta beyond itemized in-flight tolerance) | Directional (or global if the discrepancy implicates shared infrastructure) | Unexpected mismatch must fail safe, never continue silently — see [05](05-reserve-accounting.md), [10-threat-model.md](10-threat-model.md) |
| Rolling volume limit exceeded | Directional | Anomaly/attack containment — enforced by `crate::quota::enforce_rolling_volume_quota`, run every orchestrator tick for both directions (see "Rolling-24h-volume quota exhaustion — full operator workflow" below for the exact mapping, commands, and states) |
| Repeated `DestinationSubmissionFailed` beyond retry budget, same direction | Directional | Likely systemic (RPC outage, fee-market issue) rather than one-off |
| Attestation/vault signer quorum unreachable for a configured duration | Directional | Liveness failure in the authorization layer shouldn't silently degrade to fewer required signers |
| Any `ManualReview` classified as a security incident (see [10-threat-model.md](10-threat-model.md)) | Global | Default to maximum caution until scoped |
| Operator-invoked emergency stop | Global | Always available (`glc-admin onchain-pause --scope global --note ...` and/or `glc-admin pause --direction <goldcoin\|solana>` for the local admission gate), highest priority gate |

**Un-pausing** is always operator-controlled and requires a note, regardless of what triggered the pause — no automatic un-pause path exists for any trigger, to avoid a flapping balance or a transient reconciliation blip silently resuming settlement. This is a deliberate asymmetry (fast/automatic to pause, slow/manual to resume), consistent with the old bridge's asymmetric pause-authority pattern (ADR-0014 §7) applied at the operational layer instead of the governance layer.

## Rolling-24h-volume quota exhaustion — full operator workflow (added 2026-08-22)

The rolling-24h-volume cap (100,000 GLC, GLOBAL and PER DIRECTION — see the reserve-sizing update above and P0-6) is enforced twice, at two different layers, and an operator dealing with an exhausted direction needs to know which is which:

- **On-chain, always, for real**: `programs/glc-reserve-bridge/src/limits.rs::enforce_and_record_rolling_volume`, checked inside `release_from_reserve`/`deposit_to_reserve` on every actual attempt. This is the protocol-level enforcement nothing can bypass — not a pause, a per-transaction quota check against a fixed-bucket window that resets entirely, on its own, once `rolling_window_seconds` (86,400s = 24h) has elapsed since the bucket started. **This reset is real and automatic — but it is a quota reset, never an un-pause**, and never claims to be a "midnight reset" (it resets 24h after the bucket started, not at a fixed wall-clock time).
- **Off-chain, as a consequence, this service's own admission gate**: `crate::quota::enforce_rolling_volume_quota`, run every orchestrator tick for both directions. When it observes a direction's on-chain window exhausted (`remaining < min_transfer_amount`), it engages this service's own LOCAL pause for that direction (`Ledger::set_paused`) — the exact same local gate `reconciliation::reconcile` already uses for a balance breach. **This local pause never lifts itself, even after the on-chain window resets** — only an explicit operator `unpause` clears it, exactly like every other auto-pause trigger in the table above.

### 1. Exact direction <-> pause-scope mapping

| Settlement direction | On-chain `PauseScope` (real, protocol-level) | Local ledger `ReserveDirection` (this service's own admission gate) |
|---|---|---|
| Goldcoin L1 -> Solana (`GlcToSol`) | `Release` (`instructions::admin::PauseScope::Release`) — direction byte `0` | `SolanaReserve` (`GlcToSol`'s destination reserve) |
| Solana -> Goldcoin L1 (`SolToGlc`) | `Deposit` (`instructions::admin::PauseScope::Deposit`) — direction byte `1` | `GoldcoinReserve` (`SolToGlc`'s destination reserve) |

(`PauseScope::Global` pauses both directions at once; there is no `PauseScope` covering both individually in one call.)

### 2. Exact operator commands

```
# Pause only Goldcoin -> Solana (on-chain, protocol-level — blocks release_from_reserve for everyone)
glc-admin onchain-pause   --rpc-url URL --keypair ADMIN_KEY --scope release --note "TEXT"

# Unpause only Goldcoin -> Solana
glc-admin onchain-unpause --rpc-url URL --keypair ADMIN_KEY --scope release --note "TEXT"

# Pause only Solana -> Goldcoin (on-chain, protocol-level — blocks deposit_to_reserve for everyone)
glc-admin onchain-pause   --rpc-url URL --keypair ADMIN_KEY --scope deposit --note "TEXT"

# Unpause only Solana -> Goldcoin
glc-admin onchain-unpause --rpc-url URL --keypair ADMIN_KEY --scope deposit --note "TEXT"
```

These are the real, enforceable, protocol-level circuit breakers — the ones to use if a direction genuinely must stop accepting new settlement, for anyone, immediately. This service's own local admission gate (`glc-admin pause/unpause --db PATH --direction <goldcoin|solana> --note TEXT`, see the LOCAL LEDGER PAUSE section above) only gates what THIS service's own API/orchestrator will do — it does not, and cannot, stop a third party from calling the on-chain program directly. `crate::quota`'s auto-pause (previous section) always engages the LOCAL gate, never the on-chain one — an operator who wants the real, protocol-level circuit breaker engaged too must run `onchain-pause` explicitly.

### 3. Confirmed behavior

- **Quota exhaustion blocks only the affected direction.** Each direction's rolling volume is tracked in its own `RollingVolumeWindow` PDA and checked independently, on-chain and off-chain — confirmed by `quota::tests::auto_pauses_the_affected_direction_only_when_quota_is_exhausted` and `api::tests::status_reports_quota_exhausted_independently_per_direction`.
- **The opposite direction can remain operational.** Same tests as above; there is no shared state between directions that a check on one could accidentally affect on the other.
- **Rolling capacity becoming available does NOT automatically unpause.** The on-chain window's own bucket reset is real and automatic, but `crate::quota` never calls `set_paused(direction, false, ...)` — confirmed by `quota::tests::never_auto_unpauses_across_repeated_ticks_of_continued_exhaustion`. Only an explicit `glc-admin unpause`/`onchain-unpause` clears either pause layer.
- **An operator must explicitly unpause after refill/reconciliation.** Exactly the mechanism above — there is no code path anywhere in this service that calls `set_paused(direction, false, ...)` other than the explicit `glc-admin pause`/`onchain-unpause` commands themselves.
- **Unpausing while the rolling quota is still exhausted does not bypass quota enforcement.** The on-chain pause flag and the on-chain rolling-volume window are two completely independent `require!` checks inside `release_from_reserve`/`deposit_to_reserve` — flipping one never touches the other. Confirmed directly by `pausing_and_unpausing_the_release_leg_does_not_reset_or_bypass_the_rolling_volume_quota` (`programs/glc-reserve-bridge/tests/release_from_reserve.rs`): pause, then unpause, while the window is still genuinely exhausted, and the exact same claim still fails with `ExceedsRollingVolumeLimit`, never succeeds and never fails with a stale pause error.

### 4. API/UI states

`GET /status` and `GET /stats` (`service/src/api.rs`) expose, per direction:

- **active** — `glc_to_sol_available`/`sol_to_glc_available` = `true` (neither paused, quota not exhausted, reserve capacity above zero).
- **quota exhausted** — `glc_to_sol_quota_exhausted`/`sol_to_glc_quota_exhausted` = `true`, with the exact remaining headroom in `glc_to_sol_rolling_volume_remaining`/`sol_to_glc_rolling_volume_remaining` (raw atomic units, `0` when fully exhausted).
- **operator paused** — `goldcoin_paused`/`solana_paused` = `true` (this service's own local gate; the on-chain `release_paused`/`deposit_paused` circuit breakers are visible via `glc-admin show-config`, not this public API, per this module's existing scope of "available capacity", never raw infrastructure/config detail).
- **quota exhausted + operator paused/waiting for refill** — both of the above `true` simultaneously; this is exactly the state `crate::quota`'s auto-pause produces once its tick observes an exhausted window, and it persists (the pause bit) even after the quota itself later clears on its own.
- **reserve/protected-minimum constraint** — a *separate*, pre-existing signal: `available_capacity <= 0` (`GET /reserve`, and `ReserveStats.available_capacity` in `GET /stats`) even with nothing paused and quota not exhausted — this is the `enforce_protected_minimum`-equivalent off-chain check, orthogonal to both pause and quota.

Any one of paused / quota-exhausted / capacity-insufficient alone is enough to make `*_available` report `false` for that direction — a UI wanting the *specific* cause reads these fields directly rather than inferring it from `POST /transfers`' error message (see next section).

### 5. User-facing message

The exact, approved copy for a direction that cannot currently accept a new transfer — for ANY of the causes above — is `service::api::DIRECTION_UNAVAILABLE_MESSAGE`:

> Bridge capacity reached for this direction.
> Transfers are temporarily paused while reserves are replenished.
> Please check the official Telegram for reopening updates.

This is deliberately the ONLY text `POST /transfers` returns for `ApiError::Paused`/`ApiError::QuotaExhausted`/`ApiError::InsufficientLiquidity` — never a technical reason code, never the raw remaining/available numbers, and **never a claim about automatic reopening**: no midnight reset, no automatic unpause, is stated or implied anywhere in this copy. Pinned directly by `api::tests::create_transfer_reports_quota_exhausted_with_the_exact_message_never_creates_a_row`, which additionally asserts the string contains neither "midnight" nor "automatic".

### 6. Test coverage

- `programs/glc-reserve-bridge/tests/release_from_reserve.rs::pausing_and_unpausing_the_release_leg_does_not_reset_or_bypass_the_rolling_volume_quota` — on-chain, item 3's core invariant.
- `service/src/quota.rs`'s own `tests` module — `does_not_pause_while_headroom_remains`, `auto_pauses_the_affected_direction_only_when_quota_is_exhausted`, `a_fresh_bucket_reset_reports_no_exhaustion_even_with_a_high_prior_total`, `never_auto_unpauses_across_repeated_ticks_of_continued_exhaustion`.
- `service/src/solana/accounts.rs`'s `rolling_volume_remaining_*` tests — the pure remaining-capacity projection, including the exact fixed-bucket boundary condition and saturating-subtraction safety.
- `service/src/api/tests.rs` — `status_reports_quota_exhausted_independently_per_direction`, `status_does_not_report_quota_exhausted_while_headroom_remains`, `create_transfer_reports_quota_exhausted_with_the_exact_message_never_creates_a_row`, `create_transfer_succeeds_when_amount_fits_within_remaining_quota`.

### 7. On-chain program (.so) impact

**None.** Every piece of this workflow is either (a) the on-chain quota/pause enforcement that already existed before this update (`limits.rs`, `instructions::admin::set_paused`, both unmodified — `programs/glc-reserve-bridge/src/` has zero diff for this change), or (b) new off-chain code reading that existing on-chain state (`service/src/solana/accounts.rs`'s new `RollingVolumeWindow` decoder and `rolling_volume_remaining` projection, `service/src/quota.rs`'s new local auto-pause consequence, and new `service/src/api.rs` fields/messages). The one on-chain change in this update is a NEW TEST (`release_from_reserve.rs`), not new program source — the deployed `.so` is unaffected.

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

The 6% bridge fee (docs/20-bridge-fee.md) accrues on the SOURCE reserve's
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
