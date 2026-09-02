# Operational Runbook (Draft)

Structured after the old bridge's `docs/runbooks.md` discipline: every procedure here should eventually be backed by an executable `glc-admin`/`glc-audit` command, asserted by CI to actually exist and behave as documented (reused practice — the old bridge's `runbook_commands.rs` caught real drift between docs and binaries repeatedly; ported to `service/tests/runbook_commands.rs`). [07-implementation-plan.md](07-implementation-plan.md) Phase 5 landed a first, deliberately partial set of real commands — see "Executable commands" below for exactly what exists today and what is explicitly still a paper procedure.

## Executable commands (Phase 5)

What actually exists, so this document never claims more than the binaries do:

- `glc-admin status --db PATH` — reserve snapshots (both directions, including cumulative accrued bridge-fee revenue — docs/20-bridge-fee.md) and the `ManualReview` backlog count.
- `glc-admin pause --db PATH --direction <goldcoin|solana> --note TEXT` / `glc-admin unpause ...` — this service's own local ledger admission gate (independent of the on-chain pause below).
- `glc-admin show-config --rpc-url URL` — decodes and prints the on-chain `BridgeConfig`.
- `glc-admin onchain-pause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT` / `glc-admin onchain-unpause ...` — submits the admin-gated-immediate `set_paused` instruction (docs/12-management-decisions.md's Phase 2 scoping decision: pause is admin-gated-immediate, not threshold-gated).
- `glc-admin set-limit --rpc-url URL --keypair PATH --field <min-transfer|per-transfer|protected-minimum|rolling-volume> --value N --note TEXT` — submits the admin-gated-immediate `set_limit` instruction (same posture as `onchain-pause` above). `--value` is atomic units of the Solana-side mint; `set_limit`'s on-chain check is against the NET release amount (`release_from_reserve`'s `limits.rs::enforce_transfer_amount`), so a `min-transfer` value must already account for the 3% bridge fee being deducted before comparison.
- `glc-admin retry-goldcoin-payout --config PATH --request-id N --note TEXT` — recovers a Solana->Goldcoin payout stuck in `goldcoin_payouts.state = 'Signed'` after its broadcast was rejected (e.g. request #8, Goldcoin RPC `-26: 64: non-mandatory-script-verify-flag (Non-canonical signature: S value is unnecessarily high)` — see the low-S signing fix). Never invoked automatically: `Orchestrator::tick_goldcoin_payouts` always skips a request that already has a `goldcoin_payouts` row, by design, so a stuck payout needs this explicit command, and this command alone. It never rebroadcasts the previously stored `signed_tx_hex` as-is, never selects a new UTXO, and never builds a second payout row — it independently reconstructs the exact same plan from the already-persisted `goldcoin_payouts`/`goldcoin_payout_inputs` rows (refusing on any mismatch against freshly recomputed request data, or if the reconstructed unsigned transaction does not byte-for-byte match what was originally built), re-runs the real independent multi-signer signing path (`signing::goldcoin_vault::independently_sign_all_inputs`, the same function a normal payout build uses), and only calls `Ledger::record_goldcoin_payout_broadcast` after the Goldcoin RPC actually accepts the resulting transaction (or reports it already known). If the broadcast fails again, the payout stays exactly in `Signed` and `bridge_requests` stays in `SettlementAuthorized` — nothing is marked done on a failed attempt. Safe to re-run: a payout already `Broadcast`/`Confirmed`/`Completed` is reported and left untouched. Unlike every other command above, this one needs `--config` (the same config file `glc-bridge-daemon` uses), not `--db` — recovery signs and broadcasts a real transaction, so it needs the configured vault signers and Goldcoin RPC, not just ledger access. The command prints whether the re-signed transaction differs from what was previously stored; if it does **not** differ, that's a strong signal the original rejection has a cause other than signature canonicalization, and needs separate investigation before assuming a retry will succeed.
- `glc-admin refund-manual-review --config PATH --request-id N --note TEXT [--keypair ADMIN_KEYPAIR] [--execute]` — refunds a fold-parked SolToGlc deposit to its ORIGINAL Solana depositor and permanently closes the request. Without `--execute` this is a strict read-only dry run (contacts no signer, loads no keypair, writes nothing, broadcasts nothing). With `--execute` it requires the bridge to be **already globally paused on-chain**, re-verifies everything against fresh state, always simulates before broadcasting, and confirms at `finalized` commitment before marking the request `Refunded`. See "ManualReview refunds (Solana->Goldcoin)" below for the full procedure — do not run this from this list alone.
- `glc-admin refund-list --db PATH [--open-only]` — read-only listing of every refund lifecycle.
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

**Update 2026-08-29: `rolling_volume_limit` raised on-chain to 500,000
GLC/24h per direction (raw `500000000000`) in production.** Applied via
the supported `glc-admin set-limit --field rolling-volume` path; nothing
in this repository hardcodes the value. The live
`BridgeConfig.rolling_volume_limit` read (`glc-admin show-config`, the
admin control plane's `GET /onchain`, and the public `GET /limits`
projection) is ALWAYS the authoritative current value — the historical
figures in the updates above are policy history, not current
configuration, and no dashboard or document should present them as
today's limit.

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

`--chunk-target-atomic` defaults to 1,250,000,000,000 (12,500 GLC, 8 decimals) — originally chosen with headroom over an earlier per-transfer limit so a single resulting chunk could always individually cover the largest possible payout via `coin::select`'s cheap single-UTXO paths without needing a multi-input combination. **As of the 2026-08-29 limit raise (`per_transfer_limit` = 20,000 GLC gross, 19,400 GLC maximum net payout at the 3% fee), a single 12,500 GLC chunk no longer covers a maximum-size payout — two chunks do (2 x 12,500 = 25,000 > 19,400 + tx fee), which is still a cheap 2-input selection, but re-tuning this default for the new maximum is an open operational decision (same sign-off process as the incident-era tuning below), deliberately not folded into the limit raise itself. Revisit this default if `per_transfer_limit` (on-chain, `glc-admin set-limit --field per-transfer`) ever changes materially again.** Every output is required to be at least 1,000 GLC (`goldcoin::split::MIN_CHUNK_FLOOR_ATOMIC`) — a UTXO too small to produce at least 2 useful chunks at the requested target is refused outright (`SplitError::NotWorthSplitting`/`ChunkBelowFloor`), rather than producing a fragment too small to matter.

### Recovery from a split stuck in Signed (fixed 2026-08-27)

Real production incident: two splits reached `Signed` (a valid `signed_tx_hex` persisted) but their broadcast attempt never got a definitive answer from the node — `transport error contacting Goldcoin RPC: error decoding response body` — leaving `txid`/`broadcast_at` `NULL` and the source UTXO still `Available`. Before this fix, every later re-run of `split-vault-utxo --execute` for that same outpoint was a guaranteed no-op forever: the idempotency check found the existing row and reported "already split, nothing to do" without ever looking at its state.

`split-vault-utxo --execute` now recovers automatically: finding an existing `Signed` row for the requested outpoint re-submits the EXACT stored `signed_tx_hex` (`goldcoin::split_recovery::recover_stuck_vault_utxo_split`) — never rebuilt, never re-signed, no new signer round-trip. This is deliberately the opposite of `payout_recovery`'s (Goldcoin payout) recovery, which always re-signs — that module recovers from a signature-canonicalization *rejection*, where the stored bytes are the suspected cause; this one recovers from a *transport* failure that never rendered a verdict on the transaction at all, so the signed bytes are presumed fine. Without `--execute`, a `Signed`-but-unbroadcast split is reported, not acted on. `sendrawtransaction`'s "already known"/"already in mempool" (`-26`, specific known messages only, never the generic code) and "already in chain" (`-27`) responses are both treated as success, same as a fresh accept; "missing inputs" (`-25`) is still refused as a conflict needing operator investigation. The recovered txid is always computed independently from the exact submitted bytes (`goldcoin::tx::txid_of_serialized`), never trusted from the RPC's own reported string. The structural `UNIQUE(source_txid, source_vout)` protection is untouched — this path only ever reads the existing row and moves it `Signed -> Broadcast` via the same idempotent `Ledger::record_vault_utxo_split_broadcast` a fresh split already used, never a second `INSERT`.

Both the fresh-build broadcast call and the recovery resubmit call are now wrapped in a bounded 3-attempt retry (`goldcoin::rpc::call_with_retry`), so an isolated transient blip resolves within a single command invocation rather than requiring a manual re-run. `RpcClient::call`'s own diagnostics were also improved to distinguish a genuine transport failure from a response that arrived but failed to parse as JSON — the latter now includes the HTTP status and a body snippet (with any run of 40+ hex characters redacted, so a misbehaving proxy reflecting a submitted signed hex back in an error page can never leak it) instead of the old, undiagnosable "error decoding response body" with no further detail.

Recovering a split still in `Built` (never signed at all) is explicitly out of scope for this path — refused with a clear message, not silently attempted; that would require re-signing, which this module deliberately never does.

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
2. **Manual-review resume, and `open-admission`, must respect UTXO liquidity.** Both `Ledger::resume_manual_review_sol_to_glc` and `glc-admin open-admission` used to check only the value-based reserve invariant before letting more `SolToGlc` demand back in — never the count-based `utxo_pool_min_available_count` gate `fold_sol_deposit` applies to a brand-new obligation. An operator resuming a `utxo_liquidity_low_at_fold` request, or reopening admission, the moment value accounting looked sufficient — while the mature UTXO count was still at or below the floor — could re-admit exactly the demand backpressure exists to hold back. Fixed in both places: resume re-runs the identical count-based check first, refusing with a dedicated `LedgerError::UtxoLiquidityLow` (leaving the request untouched, reserving nothing); `open-admission` refuses with `LedgerError::UtxoLiquidityLowForAdmission` (also naming `own_unconfirmed_change_atomic`, so the error itself shows whether the "missing" liquidity is already known and en route to maturing) via the new `Ledger::check_utxo_liquidity_for_admission` — additive to, never a replacement for, the existing hard-invariant check. Both succeed normally the instant liquidity recovers, with no special-casing. See `service/tests/manual_review_resume_liquidity.rs` (Tests A-E) and `service/tests/open_admission_liquidity.rs` (Tests A-D). Solana admission is untouched either way — `check_utxo_liquidity_for_admission` is a no-op for `SolanaReserve`, and `cmd_admission` already refuses `--direction solana` before reaching either check.
3. **The sticky-pause path for explained internal change.** `reconciliation::reconcile`'s hard invariant (`observed_balance >= protected_minimum + pending_obligations`) only ever looked at the raw mature balance — so the exact chunk consumed to cover a much smaller payout could show up as an unexplained shortfall the instant reconciliation ran, even though every atomic unit was known, ledger-tracked, unconfirmed payout change, auto-pausing a reserve that was never actually short. Fixed: the hard invariant now adds `Ledger::own_unconfirmed_change_atomic` (GoldcoinReserve only; always `0` for SolanaReserve) to `observed_balance` before comparing against `protected_minimum + pending_obligations` — grounded entirely in independently-observed chain state matched against this service's own already-broadcast payouts, so it can never paper over genuine, unexplained loss. See `service/tests/utxo_liquidity_incident.rs`'s Test G.
4. **Signer/config-mismatch diagnostic.** `glc-bridge-daemon` now logs the effective `utxo_pool_min_available_count`/`utxo_pool_warning_count`/`change_fanout_target_atomic`/`change_fanout_max_outputs`/`vault_min_confirmations` at startup (`tracing::info!`), so an operator can directly diff what each independent signer instance actually loaded rather than only seeing an opaque stuck-payout/signing failure if two signers' configs silently drift. This is a visibility improvement only — the existing cryptographic signature-verification-at-assembly behavior that already fails closed on a real mismatch is unchanged.

### Batching — audited, deferred (not implemented in this pass)

Paying several finalized SolToGlc obligations in one Goldcoin transaction (one recipient output per obligation, fragmented vault change) would reduce UTXO churn further and was explicitly considered. Not implemented here: `goldcoin_payouts.request_id INTEGER PRIMARY KEY` structurally enforces exactly one payout per request throughout persistence, verification, and independent re-derivation — batching would need a genuine schema/protocol change (a payout-to-requests join, multi-request signer re-derivation, and multi-request replay/accounting semantics), not an incremental change alongside fan-out. Change fan-out + admission backpressure close the production incident on their own (see the regression tests below); batching remains a well-scoped, separately-reviewable follow-up rather than something to fold in here.

### Regression coverage

`service/tests/utxo_liquidity_incident.rs` reproduces the production incident directly, using `utxo_pool_min_available_count = 10` (the final recommended production value for this vault shape, not the shipped default): a burst of 25 consecutive 2,000 GLC gross obligations against a freshly-split pool (proving the service neither misclassifies reserve as unexplained-zero nor exhausts liquidity, applying backpressure before the hard invariant could ever trip instead); automatic admission recovery once change matures; several full maturity cycles with exact conservation and no permanent pause; randomized payout sizes with no double-spend and an always-preserved protected floor; two independent signers re-deriving a byte-identical multi-change transaction; and a daemon restart mid-fan-out reconstructing state correctly from Goldcoin RPC plus the ledger alone.

`service/tests/utxo_liquidity_production_tuning.rs` runs the same vault shape and fee-rate-realistic numbers against the HISTORICAL shipped config default (`utxo_pool_min_available_count = 8`, now superseded by `10` — `fee_rate_per_kb = 100_000`) to prove it no longer breaches or pauses post-fix (see "PR #35 maintainer-review fixes" above), confirms the mechanism works correctly on an appropriately-sized pool, validates the final `10` recommendation itself against the real production fee rate, verifies the real change-fan-out output shapes for both a typical ~4,770 GLC input and a very large ~97,000 GLC input, and verifies the production fee calculation at the real fee rate.

`service/tests/manual_review_resume_liquidity.rs` covers the resume-must-respect-UTXO-liquidity fix directly: a resume attempt refused while the pool sits at the floor; no duplicate obligation or payout created across repeated refused attempts; the triggering payout's change maturing and the pool recovering; resume succeeding normally once it does; and the protected reserve invariant never breaching throughout.

## Zero-conf payout change (added 2026-08-30)

Bridge-created payout CHANGE outputs are spendable for the next payout at
0 confirmations; everything else in the vault still waits
`vault_min_confirmations` (unchanged at its configured value). The
mechanics, in the order the guarantees stack:

- **Provenance is authoritative, never inferred.** A change output
  qualifies ONLY via its exact `(txid, vout)` row in
  `goldcoin_payout_change_outpoints`, written by
  `Ledger::record_goldcoin_payout_broadcast` in the same ledger
  transaction as the broadcast fact itself (change outputs are
  `outputs[1..]` of the payout; the destination is always output 0 and
  never gets a row, so a payout whose destination pays a watched script
  cannot be misclassified). Paying the vault script, or appearing in a
  vault-touching transaction, is NOT provenance. External deposits,
  vault-split outputs (`vault_utxo_splits` is a separate relation), and
  outputs broadcast before schema v14 all stay on the full-threshold
  policy — fail closed, no backfill.
- **Confirmed liquidity is always preferred.** Selection runs against
  confirmed (`Available`) UTXOs alone first; the 0-conf pool joins only
  when they cannot fund the payout (`signing::goldcoin_vault`'s
  two-phase selection, identical and deterministic across every
  independent signer).
- **Parent validation before use.** Each tick, before any payout
  building, the orchestrator re-checks every candidate's parent payout
  transaction against the live node (`getrawtransaction`). A parent the
  node no longer knows/accepts — evicted, conflicted, replaced, or an
  RPC failure, all treated identically — puts a persisted hold on its
  change (`vault_utxos.zero_conf_hold_reason`), honored by both the
  eligibility query and the reservation guard; re-acceptance clears it.
  A change output that disappears from `listunspent 0` entirely is
  marked Spent by the ordinary sync on the very next tick.
- **Chaining is capped — two modes** (`goldcoin.zero_conf_change_mode`,
  added 2026-08-30):
  - `"depth_limited"` (default): `goldcoin.zero_conf_change_max_depth`
    bounds the unconfirmed OWN-payout ancestor depth a 0-conf input may
    carry. At the shipped depth of 1, change whose only unconfirmed
    ancestor is its own parent payout is spendable, and the resulting
    payout's change records depth 2 — not spendable until a confirmation
    lands (chains stall every second generation). This mode is the
    rollback target for the recursive mode below.
  - `"bridge_owned_recursive"`: recursive reuse of VERIFIED
    bridge-created payout change — confirmed UTXO -> payout -> 0-conf
    change -> payout -> 0-conf change -> ... with no confirmations in
    between. The per-input cap becomes
    `goldcoin.zero_conf_change_recursive_chain_limit` (default 20,
    validated 1..=24), and selection additionally enforces a
    per-transaction budget: the sum of the selected still-unconfirmed
    inputs' recorded depths must stay within that same limit, so a
    constructed transaction always stays safely below the node's
    mempool chain policy (Goldcoin Core v0.17 `-limitancestorcount`:
    reject at 25 in-mempool ancestors including the new transaction,
    `too-long-mempool-chain`; the 101kB ancestor-size limit is orders
    of magnitude above these transactions). When a selection would
    exceed the budget, the deepest 0-conf candidate is dropped and
    selection re-runs (deterministically, so independent signers
    agree); if nothing within budget can fund the payout, the build
    fails closed for that tick and the request retries once a
    confirmation lands — a rejectable transaction is never constructed,
    and nothing is ever marked lost. Eligibility, provenance,
    parent-validation holds, and the reservation re-check are IDENTICAL
    to depth-limited mode: only authoritative payout change ever
    qualifies, at any depth.

  In BOTH modes `zero_conf_change_max_depth = 0` remains the kill
  switch that disables 0-conf change spending outright. Depth is
  recorded at broadcast and is an upper bound; from an output's first
  confirmation, its whole own-chain ancestry is buried and the caps no
  longer apply.

  **Deliberately unchanged by the recursive mode:** admission capacity
  (`fold_sol_deposit`) still follows the CONFIRMED reserve book only
  (reconciliation's >= `vault_min_confirmations` observed balance), and
  the `utxo_pool_min_available_count` floor still counts confirmed
  UTXOs only — under sustained demand, new SolToGlc obligations can
  still park in `ManualReview` while the payout engine is happily
  settling already-admitted ones from recursive change; they
  auto-resume as change confirms. Crediting
  `own_unconfirmed_change_atomic` into admission capacity would be the
  coherent next step if that latency matters, but it admits obligations
  against unconfirmed backing and is a separate, explicit decision —
  not bundled here.
- **Failure/recovery posture is unchanged.** A payout built on 0-conf
  change whose dependency later fails keeps every existing guarantee:
  one payout per request (the `goldcoin_payouts` PK),
  `retry-goldcoin-payout` re-derives the byte-identical transaction and
  reports `BroadcastConflict` if its inputs are genuinely gone — never a
  second, independent payout. Recovering the PARENT payout (its own
  normal recovery path) restores the child's inputs.
- **Operator visibility.** `glc-admin status` prints the 0-conf policy
  pool on its own line ("zero-conf payout change (policy candidates, not
  confirmed liquidity)"), separate from `mature_spendable_capacity` —
  0-conf change is never counted as confirmed reserve liquidity, never
  enters reconciliation's observed balance (still confirmed-only), and
  never satisfies the `utxo_pool_min_available_count` admission floor. A
  nonzero "on parent-validation hold" count deserves attention: a parent
  payout may have been evicted or conflicted.

## Automatic UTXO liquidity shaping (added 2026-08-30)

### The incident this closes

After the per-transfer maximum was raised from 2,000 to 20,000 GLC (2026-08-29), production SolToGlc payouts began repeatedly failing with `coin selection failed: selection would require more than 10 inputs`. Root cause was a **configuration mismatch the limit raise had explicitly deferred re-tuning** (see the former `default_change_fanout_target_atomic` doc comment): the mature pool was shaped into ~2,500 GLC chunks — `change_fanout_target_atomic = 250000000000` atomic units, sized for the *former* 2,000 GLC maximum — so a maximum 19,400 GLC net payout needed 9–10 of them, permanently riding the `max_inputs = 10` edge. Under sustained traffic, with each payout's own change out of the pool for the full `vault_min_confirmations` maturity window, the largest 10 *mature* candidates repeatedly summed below the target and selection correctly failed closed. This was **not a selector defect**: `goldcoin::coin::select`'s largest-first accumulation is feasibility-complete within `max_inputs` (if any `<= max_inputs` combination covers the target plus its own fee, the largest-`k` subset does — pinned exhaustively by `selection_never_reports_too_many_inputs_when_any_valid_combination_exists` in `service/src/goldcoin/coin.rs`). The failures were genuine infeasibility at 10 inputs against a mis-shaped pool, and the only remedy was manual, operator-run `glc-admin split-vault-utxo` — unacceptable for 24/7 operation.

### What changed

1. **`max_inputs` 10 -> 25** (`service/config.pilot-template.toml`) — the explicit, tested decision replacing the incident-day emergency edit. Cost quantified in `goldcoin::coin`'s test `twenty_five_input_transaction_size_and_fee_are_modest`: a worst-case 25-input 2-of-3 payout with 11 outputs is ~7.8 KB (far below relay/standardness ceilings) and costs ~0.0079 GLC at the production fee rate — noise against a 20,000 GLC payout. Shaping (below) keeps the pool chunked so this headroom is rarely needed, never routine.
2. **`change_fanout_target_atomic` 2,500 -> 5,000 GLC** (`default_change_fanout_target_atomic`, `service/src/config.rs`) — the deferred re-tune, made as its own reviewed decision: a maximum net payout now needs ~4 chunks (comfortable margin below `max_inputs`), while a typical smaller payout still finds a single covering chunk. This is also the chunk target automatic shaping splits to — one canonical payout-chunk size for the whole service.
3. **The split lifecycle** (`goldcoin::liquidity`, schema v16). Every split — automatic or CLI-initiated — moves through ONE persisted state machine: `Built -> Signed -> Broadcast -> Confirmed`, with `Abandoned` reachable from any non-terminal state. The load-bearing properties:
   - **`Built` is the CLAIM on the source outpoint**, written and validated (source must exist and be `Available`) in one transaction BEFORE any signer round-trip. From that commit, the source is excluded from payout coin selection (`available_vault_utxos`) and from the payout reservation guard (`reserve_vault_utxos` re-checks inside its own write transaction) — a payout and a split can never commit to the same UTXO, regardless of process interleaving (concurrent CLI + daemon included) or restarts.
   - **Broadcast bookkeeping is one transaction** (`Ledger::record_vault_utxo_split_broadcast`): split row -> `Broadcast`, source -> `Spent` (with `spent_by_txid` = the split's own txid), every chunk inserted as an `Unconfirmed` `vault_utxos` row — so `own_unconfirmed_change_atomic` (matching split txids in `Broadcast`/`Confirmed` as well as payout txids) explains the mature-balance dip with no crash window, and a split's fee joins payout fees in the permanent-departures term.
   - **`Broadcast` is driven, not abandoned to fate**: each tick, reaching `vault_min_confirmations` (via this service's own synced chain view) marks the split `Confirmed` (terminal) — the same depth the crate trusts outputs everywhere else, so a shallow reorg never orphans a split nothing maintains; a split the node no longer knows (mempool eviction, e.g. a node restart) is re-broadcast from its exact stored bytes. A transport-level RPC failure always defers to the next tick — an unreachable node never changes any state.
   - **Automatic abandonment only where provably safe; ambiguity defers to the operator**: a `Built` row (nothing signed yet) whose source is gone, or a fresh broadcast rejected outright, is auto-`Abandoned`. Every ambiguous case — a missing-inputs refusal (reorg races produce these transiently for transactions that later confirm), a `Signed` split the local node has forgotten, a transient floor refusal — is surfaced loudly (`lifecycle_error` in the tick report / CLI output) and DEFERRED: fully signed bytes are never walked away from automatically. Deferral is never silent masking, though: a `Broadcast` split persistently refused for missing inputs is FLAGGED (`missing_inputs_since`), and after a grace window (`Ledger::SPLIT_MISSING_INPUTS_GRACE_SECS`, 10 minutes) TWO things happen: the accounting terms stop explaining its phantom chunks, and — decisively, since the delta-based unexplained-drop detector can never re-fire for a drop it already explained at broadcast time — reconciliation raises an EXPLICIT dead-split alarm (`ReconciliationReport::dead_split_ids`): classification `Breach`, auto-pause, and a pause reason naming the split(s), regardless of how much solvency headroom the reserve has. A possible conflicting spend of vault funds always pauses and pages; it is never inferred from arithmetic that cannot see it. The flag clears itself the moment the node knows the transaction again. `glc-admin split-vault-utxo --abandon --execute` is the deliberate, per-outpoint release: for any split with signed bytes it derives the txid from the exact stored bytes (persisting it onto the row, so the re-adoption watch below covers `Signed` abandons too) and probes the node tri-state — refusing while the transaction is KNOWN, refusing (fail closed) when the node is UNREACHABLE, allowing only on a definitive "no such transaction"; `Confirmed` splits are never abandonable. Abandonment mutates NO reserve book (reconciliation's own per-tick refresh already converged the cached book when the split broadcast — a debit would double-count); the audit row is the permanent record. If the node reports the abandoned transaction within a 24-hour watch window (`liquidity::READOPT_WATCH_SECS`, a live probe — never stale ledger confirmation counts), lifecycle maintenance RE-ADOPTS the split automatically (state back to `Broadcast`, chunks re-entering the accounting and, at maturity, the pool); past the window an abandonment is terminal and chain-resurrected value is a reserve-custody runbook decision. The `Abandoned` audit row is kept forever while the partial uniqueness index (`WHERE state != 'Abandoned'`) releases the outpoint for a legitimate later split. Pending-split recovery visits EVERY `Built`/`Signed` row with per-row error isolation (never head-of-line blocking), and a deferring or erroring split never stops maintenance of other splits or new-split consideration; a never-broadcast `Signed` split re-checks the reserve floor against CURRENT state before its bytes ever reach the network, and its recorded chunk amounts are always byte-verified against the persisted unsigned transaction. No state can permanently wedge shaping, and no recovery path involves SQLite.
   - **Crash-window bookkeeping heals BEFORE reconciliation** (`goldcoin::liquidity::heal_split_bookkeeping`, run at the front of every orchestrator tick, per-row error isolation): a `Signed` split whose exact bytes the node already knows — a crash landed between broadcast acceptance and the ledger commit, in the daemon or in a concurrent `glc-admin` run — has its Broadcast bookkeeping recorded probe-only (never signing, never re-sending, so never a duplicate transaction) before any reconciliation pass could read the spent source as an unexplained loss and latch a false auto-pause. If ANY `Signed` row's status cannot be settled (node unreachable, verification refused, write failed), BOTH of the tick's Goldcoin reconciliation passes are skipped — recorded visibly as `SKIPPED`, retried next tick — rather than judged on books that may be mid-heal. Payout liveness has one more guard: a `Built` claim resumed after downtime is released (abandoned, safely — nothing was signed) when the remaining mature pool can no longer cover obligations admitted in the meantime, and a fresh split whose just-signed broadcast is refused for missing inputs stays `Signed` for resume — never auto-abandoned. All split lifecycle events (`lifecycle_error`, abandonments, re-adoptions, re-broadcasts, heals) are logged by the daemon at warn/info level every tick.
   - **Payout liveness outranks shaping, in both entry points**: a split (automatic or CLI) is deferred/refused — non-overridably — while removing its source from the mature pool would leave already-admitted obligations uncoverable; and a claimed source is excluded from admission backpressure counts and pool-health figures, never reported as spendable liquidity.
   - **One shaping tick** (`run_shaping_tick`, wired after the payout pass) = lifecycle maintenance plus at most one transaction-shaped action, in priority order: drive `Broadcast` splits; resume/abandon the oldest pending split; only then — while the payout-ready pool (mature Available UTXOs plus currently-eligible 0-conf payout change, each at half the chunk target or better) is below `utxo_shaping_target_available_count` and no previous split's chunks are still maturing — claim and execute one NEW split of the largest eligible root-vault UTXO (>= `utxo_shaping_min_source_atomic`, at most `utxo_shaping_max_outputs_per_split` chunks, never below the canonical chunk target).
   - **The CLI is the same code**: `glc-admin split-vault-utxo` calls the identical `goldcoin::liquidity` functions (`execute_fresh_split`, `resume_pending_split`, `maintain_broadcast_splits` — scoped to exactly the named outpoint's split, never an unrelated one) — it resumes a pending split instead of falsely reporting it done, and there is no CLI action that can strand lifecycle state. `--abandon --execute` is the operator-decided release valve for a not-yet-`Confirmed` split the automatic lifecycle cannot finish (e.g. the node permanently rejects its stored bytes for a policy reason): audit row kept, source outpoint released, no SQL.
   - **One problematic split never freezes shaping**: a lifecycle step that errors (an RPC surprise, an unrecognized node rejection) is surfaced as `lifecycle_error` in the tick report and the tick continues — maintenance of other splits and new-split consideration proceed. An unreachable node always DEFERS (nothing is abandoned, nothing recorded) rather than being read as "transaction unknown". A split is also deferred, never executed, while removing its source from the mature pool would leave already-admitted obligations uncoverable — payouts keep first claim on mature liquidity.
4. **Solvency-aligned split safety check**: the reserve-floor refusal is `balance - fee >= protected_minimum + pending_obligations`, replacing `balance - source_amount >= floor`. A split never removes value from the vault's custody — every chunk output pays the vault's own script and is ledger-tracked from the instant of broadcast — so only the network fee genuinely leaves. The old formula pretended the whole source had left, which deadlocked the exact bootstrap scenario shaping exists for: a vault whose dominant liquidity IS one oversized deposit could never be restructured at all. The check is pre-run before a claim is ever written, re-run by EVERY signer independently (`signing::goldcoin_split::RecoverySplitSource`, which also independently proves the plan it signs serializes byte-identically to the persisted unsigned transaction, and refuses non-root-vault sources), and re-run before a resumed never-broadcast split's bytes reach the network. Non-overridable everywhere.
5. **Mempool-safe UTXO sync** (`Ledger::sync_vault_utxos`): one missed `listunspent` snapshot can no longer permanently destroy accounting state. Chunk outputs of a split still in `Broadcast` are exempt from the absence flip (their transaction's fate is owned explicitly by the lifecycle above — the 0-conf payout-change policy's own "disappearance removes it from the selectable pools immediately" behavior is deliberately unchanged); and a row the sync itself once inferred `Spent` (`spent_by_txid` NULL — never one spent by a transaction this service signed) is resurrected when a fresh snapshot reports the outpoint unspent again (parent re-broadcast after eviction, reorg restored it). Chain truth wins in both directions; rows this service spent stay `Spent` forever.
6. **`list_unspent` scan stays at min_conf 0** (`Orchestrator::tick_vault_utxos`) — the same 0-conf-inclusive scan the "Zero-conf payout change" feature above already established, which shaping's own accounting (split chunks tracked as `Unconfirmed` from broadcast) equally depends on. **The maturity policy itself is unchanged**: classification against `vault_min_confirmations` still happens in `sync_vault_utxos`, and reconciliation's `observed_balance` keeps its own mature-only read.

### What deliberately did NOT change

- **External deposits still require `vault_min_confirmations` before becoming selectable.** Unchanged everywhere.
- **The "Zero-conf payout change" policy above is preserved exactly as shipped** (`zero_conf_change_max_depth = 1` behavior, provenance table, parent validation, holds — all untouched). Shaping composes with it, on the strict side: a split's chunk outputs get NO `goldcoin_payout_change_outpoints` row — they are not payout change — so they fail closed onto the full `vault_min_confirmations` policy, exactly as that section's provenance rules already state for vault-split outputs. Shaping never widens the 0-conf surface by a single outpoint.
- **Selection preference order is unchanged**: exact match, then the change-minimizing choice between the smallest covering single UTXO and a bounded smallest-first combination (so a huge confirmed deposit is *usable* immediately but not *wastefully consumed* while smaller mature chunks suffice), then largest-first accumulation — fewer inputs always preferred, `Reserved`/`Spent`/deposit-backing outpoints never offered. The zero-conf pool joins only via the pre-existing two-phase selection, exactly as before.
- **No manual SQLite edits**: every state change goes through typed ledger methods.

### Operator workflow (the whole point)

1. Deposit any large GLC refill directly to the reserve vault address.
2. Wait `vault_min_confirmations`.
3. Done. The daemon restructures the deposit into payout-sized chunks itself (bounded by the reserve-floor safety check), payouts select from them automatically, and every payout's own change fans back out at the canonical chunk size. `glc-admin split-vault-utxo` remains available but is no longer part of normal operation; `utxo_shaping_enabled = false` restores the operator-driven flow.

Config (all under `[goldcoin]`). **Two keys are REQUIRED, explicitly, for production** (2026-08-31 review, M2 — a binary upgrade must never silently change an existing deployment's behavior): `utxo_shaping_enabled = true` (the bare default is `false`: autonomous vault self-spends are explicit opt-in; in-flight split lifecycle maintenance runs regardless — only NEW automatic splits are gated) and `change_fanout_target_atomic = 500000000000` (5,000 GLC, the reviewed production chunk sizing; the bare default stays at the pre-existing 2,500 GLC so configs omitting the key keep their old behavior). Both are set in `service/config.pilot-template.toml` with `REQUIRED PRODUCTION KEY` markers. The remaining knobs default sensibly: `utxo_shaping_target_available_count` (15 — matches `utxo_pool_warning_count`), `utxo_shaping_min_source_atomic` (`4 * change_fanout_target_atomic`; validated `>= 2x` the chunk target), `utxo_shaping_max_outputs_per_split` (25). The daemon logs all effective values at startup for cross-signer drift diagnosis.

**Trust-model note — the signer-side split floor (2026-08-31 review, M1, documented decision, no code change).** Each vault signer's non-overridable split refusal is the solvency formula `balance - fee >= protected_minimum + pending_obligations`: it guarantees a signer can never be induced to sign a split that makes the reserve INSOLVENT (chunks stay vault-owned; only the fee leaves), but it deliberately does not encode payout LIVENESS — the guard that a split must not immobilize the mature liquidity already-admitted obligations need lives in the orchestrator/CLI (both non-overridable there). The residual accepted: a buggy or compromised orchestrating process could obtain threshold signatures for a split that stalls payouts for one `vault_min_confirmations` maturity window — a bounded liveness effect, never a solvency one, on par with the same process's existing ability to simply not build payouts at all. The alternative (restoring `balance - source >= floor` in signers) provably deadlocks the bootstrap scenario shaping exists for (test J). Any future change to this posture is a docs/02-trust-model.md amendment requiring its own sign-off, not a code tweak.

### Regression coverage

`service/tests/utxo_liquidity_autoshaping.rs` drives the real production code paths end to end (real ledger, real independent 2-of-3 split signing, real selection/fan-out, a broadcast double with real node-membership/eviction semantics): **A** — one 1,000,000 GLC deposit funds six cycles of repeated 20,000 GLC payouts with zero manual splits, no `TooManyInputs`, no outpoint reuse, and reconciliation clean throughout; **J** — the production bootstrap (the deposit IS the entire reserve, admission floor 10) parks on the count floor, shapes anyway under the solvency-aligned check, and self-recovers after one maturity window; **B** — the incident's exact fragmented pool shape selects within `max_inputs`; **D/E/F** — with the 0-conf policy disabled, unconfirmed internal change and sub-min-conf external deposits are never selectable and become selectable exactly at maturity; **G** — concurrent reservation cannot double-select an input; **H/H2** — restart resumes `Signed` splits by stored bytes and `Built` splits by verified re-signing; **I/I2** — a healthy pool produces no self-transactions, and shaping never stacks a second split while chunks mature; **K** — at the production depth-1 zero-conf setting, split chunks are never 0-conf eligible or reservable while payout change keeps its documented eligibility; **L** — an evicted `Broadcast` split keeps its accounting term through the missed snapshot, is re-broadcast byte-identically, and reaches `Confirmed`; **M** — a conflicted split is abandoned loudly, its phantom chunks cleared, and shaping continues; **N** — a claimed split whose source vanished is abandoned, and the source resurrects and re-splits when the chain restores it; **O** — a claimed source is invisible to payout selection and unreservable, and abandonment releases it. `tests/zero_conf_change_policy.rs` continues to own the 0-conf payout-change policy itself, including that vault-split outputs never receive it. The selector's feasibility-completeness and the `max_inputs = 25` cost decision are pinned in `service/src/goldcoin/coin.rs`'s own tests.

## Admission control (Solana->Goldcoin) (added 2026-08-24)

### Why this exists

The local ledger pause (`glc-admin pause`/`unpause`, above) and payout processing were never actually the same thing: `Orchestrator::tick_goldcoin_payouts` has never checked `paused` — it always continues building/signing/broadcasting for any request already `SourceFinalized`, regardless of pause state. The ONLY thing `paused` gates is `Ledger::fold_sol_deposit`'s decision to admit a newly observed on-chain SolToGlc obligation (`SourceFinalized`) versus park it (`ManualReview`). Because that's the single lever, an operator recovering from an incident (e.g. the vault-UTXO-splitting scenario above) who calls `unpause` to let the reserve return to normal simultaneously reopens admission for brand-new deposits — right when reserve headroom is thinnest, racing the still-draining backlog and risking an immediate re-pause.

`admission_closed` (`reserve_ledger`, separate from `paused`) fixes this by giving admission its own, independent, operator-only switch. **Scoped to Solana->Goldcoin only** — `glc-admin close-admission`/`open-admission --direction goldcoin`, since that's the direction `fold_sol_deposit` actually checks; `--direction solana` is refused with a clear "not implemented in this version" error rather than silently doing nothing.

### What it does, and does not, change

- Only `fold_sol_deposit`'s admission decision reads `admission_closed`. Both gates (`paused` and `admission_closed`) must be clear for a new obligation to be admitted — closing either one alone is enough to route a new fold to `ManualReview`; the pre-existing `paused` behavior is completely unchanged.
- Payout processing, confirmation tracking, the 2-of-3 signer path, reconciliation's breach formula, the rolling-volume quota, and the on-chain program are all untouched. An already-`SourceFinalized`/`SettlementAuthorized`/`DestinationSubmitted` request is never affected by `admission_closed` in any way — it keeps processing exactly as it always has.
- **No automatic reopen, and nothing automatically closes it either**: reconciliation and the rolling-volume quota continue to only ever touch `paused`, exactly as before. `admission_closed` changes ONLY via an explicit operator command. (The confirmed-liquidity safety buffer added 2026-09-02 *does* open and close automatically — but it is a SEPARATE column and a separate axis, and never reads or writes `admission_closed`. See "Confirmed-liquidity admission safety buffer" below.)
- **No manual DB editing** — both directions go through `Ledger::set_admission`, never a raw `UPDATE`.

### Exact operator procedure

1. `glc-admin close-admission --db PATH --direction goldcoin --note TEXT` — always allowed. New SolToGlc deposits now fold into `ManualReview` instead of `SourceFinalized`; nothing about already-accepted requests changes.
2. Let already-accepted obligations continue draining normally (no action needed — payout processing was never gated by admission or pause in the first place).
3. When ready to accept new transfers again: `glc-admin open-admission --db PATH --direction goldcoin --note TEXT`. Refuses unconditionally (no override) unless ALL THREE: `GoldcoinReserve`'s hard invariant currently holds (`balance >= protected_minimum + reserved_liquidity`, the same check `reconciliation::reconcile` enforces); the mature UTXO count is still above `utxo_pool_min_available_count` (`Ledger::check_utxo_liquidity_for_admission` — the same count-based gate `fold_sol_deposit` applies to a brand-new obligation, added by the "PR #35 maintainer-review fixes" section above); and the automatic confirmed-liquidity gate has already reopened (`Ledger::check_liquidity_buffer_for_admission`, added 2026-09-02 — otherwise clearing the operator flag would appear to succeed while every new fold kept parking). Each error names its own figures: the current count and configured floor plus any known unconfirmed internal change, or the current confirmed headroom and the reopen threshold.
4. `glc-admin status --db PATH` reports `admission_closed=<bool>` per direction alongside the existing `paused=<bool>`. The public `/status` endpoint exposes the Solana->Goldcoin side as `sol_to_glc_admission_open` — a UI should read `false` there as "not accepting new transfers right now" (maintenance), distinct from `sol_to_glc_available` being `false` for reserve-health/quota reasons.

### Resuming an individual request parked in ManualReview

`fold_sol_deposit` routes a new SolToGlc obligation to `ManualReview` (never dropped — the Solana-side deposit is already real and irreversible) whenever `admission_closed`, `paused`, or insufficient capacity was true at the exact moment it was observed. Once the underlying condition clears, that specific request does not automatically resume — `glc-admin resume-manual-review --db PATH --request-id N --note TEXT` moves it back to `SourceFinalized` (reserving its capacity, exactly as a successful fold would have) so normal processing picks it up.

Scoped narrowly and refuses (no override) unless ALL of: the request is `SolToGlc` and currently `ManualReview`; its `manual_review_note` is one of the seven known fold-time reasons (`admission_closed_at_fold`/`reserve_paused_at_fold`/`insufficient_capacity_at_fold`/`utxo_liquidity_low_at_fold`/`liquidity_buffer_low_at_fold`/`recipient_rate_limited`/`source_wallet_rate_limited` — never some other `ManualReview` cause); its source deposit is already finalized; it has no `goldcoin_payouts` row or `destination_txid` yet; NEITHER the recipient NOR the Solana source wallet is still inside its own rolling 24-hour window (see below and "SolToGlc source-wallet rate limit" — both checked unconditionally, independently, regardless of the request's own `manual_review_note`); the mature Goldcoin UTXO count is still above `utxo_pool_min_available_count` (the identical count-based gate `fold_sol_deposit` applies to a brand-new obligation — refuses with `LedgerError::UtxoLiquidityLow` otherwise, added by the PR #35 maintainer-review fix above); reserving its capacity now would not breach the `GoldcoinReserve` invariant (the same `available_capacity` check `create_request`/`fold_sol_deposit` use to admit anything new); and reserving it now would still leave the confirmed-liquidity admission safety buffer intact (`LedgerError::AdmissionLiquidityBufferLow` otherwise — the same per-request formula `fold_sol_deposit` applies, for the same reason the UTXO-count floor is re-applied here: a resume re-admits real demand exactly as a fresh fold would, and self-clears the moment headroom recovers). Deliberately does NOT check `admission_closed`/`paused` — admission may stay closed while this resumes something already accepted, since it never admits anything new. **Refuses permanently for any request with a refund lifecycle** — `RefundPending`/`RefundBroadcast`/`Refunded`, or any `solana_refunds` row at all (checked against the row, so an out-of-band `bridge_requests.state` edit cannot re-open a refunded request) — see "ManualReview refunds (Solana->Goldcoin)" below. Idempotent: re-running it on an already-resumed request, or retrying while UTXO liquidity is still low or either rate limit still applies, is a safe no-op either way. Preserves the request's id and `source_obligation_index` — it transitions the existing row in place, never creates a new one, so a duplicate obligation is impossible by construction.

### Automatic recovery, without an operator (added 2026-08-26/27, extended 2026-08-28)

`Orchestrator::tick_auto_resume_utxo_liquidity_backlog` runs as the last phase of every tick and automatically resumes `ManualReview` requests parked for exactly the three conditions that self-clear over time — `utxo_liquidity_low_at_fold`, `recipient_rate_limited`, and `source_wallet_rate_limited` — oldest first, reusing `resume_manual_review_sol_to_glc` verbatim (identical safety checks, no separate logic). It never touches any other `ManualReview` reason (`admission_closed_at_fold`/`reserve_paused_at_fold`/`insufficient_capacity_at_fold` still require `glc-admin resume-manual-review`), stops the whole batch immediately on a paused reserve, closed admission, `OrchestratorConfig::max_auto_resumes_per_tick` being reached, or any unexpected error — except a `recipient_rate_limited` or `source_wallet_rate_limited` refusal, each a per-recipient or per-wallet, independent condition: that one candidate is skipped (counted in `AutoResumeReport::skipped`) and the pass continues to the next, so one recipient or wallet still inside its window never stalls unrelated, eligible candidates behind it in the same tick. A request with a refund lifecycle is never a candidate at all (a refund moves it out of `ManualReview`), and is additionally refused-and-skipped by the same per-request rule if one is ever reached through an out-of-band state edit.

## Confirmed-liquidity admission safety buffer (Solana->Goldcoin) (added 2026-09-02)

### Why this exists

`protected_minimum` is a cliff, not a cushion. Until now the last SolToGlc obligation admitted before headroom ran out could take the reserve from "comfortable" to "sitting exactly on the hard floor" in one step, with the next arrival parking as `insufficient_capacity_at_fold` and an operator finding out only from the ManualReview backlog. The reserve was never insolvent at any point — the accounting was correct throughout — but there was no margin left to absorb a payout fee, a reorg, or a batch of deposits arriving inside one tick.

The safety buffer adds that margin. It closes admission for NEW obligations *while the reserve is still healthy*, keeping a deliberate reserve of confirmed liquidity above `protected_minimum` that new demand may not consume, and it reopens only after a genuine recovery — never on a single reading that happens to tick back over the line.

### The policy

- **Buffer (close threshold): 250 000 GLC.** Admission closes as soon as confirmed unreserved Goldcoin headroom drops **below** 250 000 GLC.
- **Reopen threshold: 350 000 GLC.** Admission reopens **only** once confirmed unreserved headroom reaches 350 000 GLC or more.
- Between the two the gate **holds whatever state it is in**. That 100 000 GLC band is the anti-flapping mechanism: a headroom oscillating anywhere inside it produces no state change at all, so the gate cannot toggle on deposit/payout churn. A single-threshold design would flip on every crossing of one number.

Both thresholds are configuration (`goldcoin.admission_safety_buffer_atomic`, `goldcoin.admission_reopen_headroom_atomic`, in 8-decimal Goldcoin atomic units), default to exactly the values above, and are validated `reopen >= buffer` at load time. Setting the buffer to `0` disables the mechanism entirely — the same "0 means disabled" shape `utxo_pool_min_available_count` uses.

### The admission calculation

A new SolToGlc obligation is admitted only when

```
total_reserve_balance >= protected_minimum
                       + reserved_liquidity
                       + <this obligation's net_destination_atomic>
                       + admission_safety_buffer
```

on top of every pre-existing gate (`paused`, operator `admission_closed`, the `utxo_pool_min_available_count` floor, both rolling-24h rate limits, and the plain capacity check). This is TWO checks, and both matter:

1. **Per-request** — the formula above. A single obligation large enough to eat into the buffer is held back *even while headroom is comfortably above the close threshold*, and smaller obligations keep flowing normally.
2. **Direction-wide** — the hysteresis gate on headroom alone (`total_reserve_balance - protected_minimum - reserved_liquidity`), which is what actually closes and reopens admission for everything.

### Confirmed means confirmed

Headroom is computed from `total_reserve_balance`, which is a **mature-only** figure by construction: `sync_vault_utxos` and `Orchestrator::tick_goldcoin_reconciliation` both filter by `vault_min_confirmations` before it is computed, and `Ledger::immature_vault_utxo_total`/`own_unconfirmed_change_atomic` are observational figures that are never added to it.

So **immature payout change buys no admission room** — including this service's own broadcast-but-not-yet-mature change, which is known, accounted for, and provably not missing. Value that cannot be spent yet must not read as room to take on new demand. (Reconciliation's hard solvency invariant *does* add `own_unconfirmed_change_atomic`, deliberately and separately: "is anything actually missing" is a different question from "may we take on more", and the two must not share an answer.)

### What it does, and does not, change

- **The hard invariant and `protected_minimum` are untouched.** The buffer sits on top of them and is never a term inside them. A reserve can be entirely solvent — `invariant_holds=true` — while the buffer has closed admission; that is the normal, intended state.
- **Already-accepted obligations keep processing.** Anything already `SourceFinalized` or later is completely unaffected: payout building, signing, broadcast, confirmation tracking and settlement all continue exactly as before, on liquidity that is real and confirmed. The gate governs admission only.
- **Nothing is cancelled.** A closed gate never touches an existing request. A newly observed deposit is still folded (the Solana-side tokens are already locked and irreversible) and parks in `ManualReview` with `manual_review_note = liquidity_buffer_low_at_fold` — resumable and refundable like every other fold-time park.
- **The operator flag is separate.** `admission_closed` (`glc-admin close-admission`/`open-admission`) remains operator-only: nothing automatic sets or clears it, exactly as before. The automatic gate is its own column and its own line in `glc-admin status`, so "I closed this" is always distinguishable from "liquidity closed this". Either one being closed is enough to park a new fold; neither can clear the other.
- **`open-admission` refuses while the automatic gate is closed**, alongside its existing invariant and UTXO-count refusals — otherwise clearing the operator flag would appear to succeed while every new fold kept parking.
- **No auto-resume.** `liquidity_buffer_low_at_fold` is deliberately NOT in `Orchestrator::tick_auto_resume_utxo_liquidity_backlog`'s filter (same posture as `insufficient_capacity_at_fold`): automatically resuming these the instant headroom crept over the line would re-admit exactly the demand the buffer holds back, and would defeat the hysteresis.

### What an operator sees

- `glc-admin status --db PATH` prints an `Admission liquidity:` line for GoldcoinReserve with `confirmed_headroom`, `buffer`, `reopen_at` and `liquidity_admission_closed` (omitted entirely when the buffer is disabled).
- `/metrics`: `glc_goldcoin_admission_liquidity_closed`, `glc_goldcoin_confirmed_admission_headroom_atomic`, `glc_goldcoin_admission_buffer_atomic`, `glc_goldcoin_admission_reopen_atomic`. All gauges, never invariants — a closed gate is the mechanism working on a healthy reserve and must never flip `/health` to 503. **Alert on it staying closed, not on it closing.**
- Admin API: `liquidity_admission_closed` on the direction status view, plus the headroom and both thresholds on the reserve-health view.
- Public `/status`: `sol_to_glc_admission_open` is `false` when EITHER axis is closed. The two causes are deliberately not distinguished there — the user-facing answer ("not accepting new transfers right now") is identical, and raw reserve figures stay operator-only.
- The daemon logs a `WARN` on every gate transition (and only on a transition, not on each evaluation).

### Exact operator procedure when the gate closes

1. Confirm it is the buffer and not something else: `glc-admin status --db PATH`. `liquidity_admission_closed=true` with `paused=false`, `admission_closed=false` and `invariant_holds=true` is the buffer doing its job on a healthy reserve.
2. **Do nothing to already-accepted obligations.** They are still settling; interfering is the only way to turn this into an incident.
3. Look at where the headroom went. The common case is that mature liquidity is temporarily sitting in immature payout change — `glc-admin status`'s `UTXO liquidity:` line shows `temporarily_immature_internal_change`. That recovers on its own as the change matures, and the gate reopens automatically at 350 000 GLC.
4. If it is not recovering, the reserve genuinely needs more Goldcoin: rebalance in (`glc-admin rebalance-*`). The gate reopens on the next tick after confirmed headroom reaches the reopen threshold — no command is needed, and no command can force it early.
5. Deposits that parked meanwhile: `glc-admin resume-manual-review --db PATH --request-id N --note TEXT` once headroom allows (it re-checks the buffer itself and refuses safely until then), or `glc-admin refund-manual-review` for one that will genuinely never be paid out.

## ManualReview refunds (Solana->Goldcoin) (added 2026-09-01)

Returns a fold-parked SolToGlc deposit to the **original Solana
depositor** and closes the request permanently. This is the compensating
action docs/04-state-machines.md's "open design item: late deposits after
expiry" and docs/12-management-decisions.md item 8 left unresolved, for
the specific case where the bridge is holding a real, finalized, unsettled
deposit it is not going to pay out.

**Use this only when the request will genuinely never be paid out.** The
normal answer to a fold-time park is `resume-manual-review` (or automatic
recovery) once the underlying condition clears. A refund is one-way: once
begun, the request can never be resumed and never receive a Goldcoin
payout.

### Eligibility (all required, no override, fail-closed)

Refused unless ALL of:

- direction is `SolToGlc`, and the request is currently `ManualReview`
  (or already inside its own refund lifecycle — see "Re-running" below);
- `manual_review_note` is one of the seven **fold-time** reasons:
  `admission_closed_at_fold`, `reserve_paused_at_fold`,
  `insufficient_capacity_at_fold`, `utxo_liquidity_low_at_fold`,
  `liquidity_buffer_low_at_fold`, `recipient_rate_limited`,
  `source_wallet_rate_limited`. Every one of
  these is a park that happened *instead of* reserving Goldcoin capacity,
  on an already-finalized deposit — the two premises a safe refund needs.
  Any other `ManualReview` cause (the GlcToSol-only reasons
  `late_deposit_no_capacity` / `deposit_amount_mismatch: ...` /
  `deposit_spent_before_finalized`, a `NULL` note, or any future/unknown
  string) is refused: an ambiguous reason is excluded, never broadened;
- the source deposit is finalized (`source_finalized_at` set) and its
  on-chain `WithdrawalObligation` still reads `Pending` at `finalized`
  commitment;
- the stored `source_obligation_index`, `requester`, and gross amount all
  match the on-chain obligation **exactly** (any disagreement between
  database and chain is a hard refusal, never a "pick one side");
- no `goldcoin_payouts` row, no `destination_txid`, no `settled_at`;
- the request never advanced to `SourceFinalized` or beyond at any point
  in `bridge_request_state_log` — the per-request *proof* that no
  Goldcoin-side `reserved_liquidity`/`pending_obligations` increment was
  ever applied, so the refund has nothing to release (it never subtracts
  blindly; a request that ever held a reservation is refused outright);
- no existing refund lifecycle other than this request's own;
- the reserve mint and token program match the live on-chain
  `BridgeConfig`;
- SolanaReserve capacity holds: `balance - protected_minimum -
  reserved_liquidity - other open refunds >= refund amount` (stricter
  than the on-chain floor, which only knows `protected_minimum`);
- **the bridge is already globally paused on-chain** (execute only; a dry
  run reports the pause state but does not require it).

### Amount and destination — both derived, never entered

The refund is the **exact gross deposited amount**, in the reserve mint's
own atomic units, taken from the on-chain `WithdrawalObligation.amount`.
No fee is deducted: the 3% SolToGlc bridge fee accrues only inside
`mark_goldcoin_completion_confirmed` (docs/20-bridge-fee.md), which a
refunded request never reaches, so there is no accrued fee to net off and
none is invented.

The destination is the canonical **Token-2022** ATA of
`(WithdrawalObligation.requester, reserve mint, reserve token program)` —
derived from on-chain data the bridge itself verified, which is by
construction the same account the deposit came from (the on-chain
`deposit_to_reserve` instruction constrains the source to exactly that
ATA and records `requester` from the deposit's own `Signer`). **There is
deliberately no `--destination` flag**; an operator cannot direct a refund
anywhere else. If that ATA no longer exists, the refund transaction
creates it idempotently, submitter-paid, in the same atomic transaction
(the identical pattern normal releases already use).

### Authorization and fund movement

Reuses the existing operator-withdrawal rail with nothing weakened:
`rebalance_withdraw` (see RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md) — the
admin's signature **and** a threshold (2-of-3 pilot) ed25519 attestation
over the canonical claim, the on-chain global-pause precondition, the live
`protected_minimum` check, `transfer_checked` via the reserve-authority
PDA, and a per-nonce `rebalance_withdrawal` PDA replay guard. Attestation
signatures come from the configured signer endpoints exactly as the
daemon's own settlement path collects them — in production no attestation
key ever exists on the machine running this command. **No on-chain program
change was needed or made.**

The refund nonce is `(1 << 63) | request_id` — a dedicated refund domain
that can never collide with ordinary rebalance nonces (small counters or
timestamps). One request maps to exactly one nonce forever, so its PDA is
a per-request, on-chain replay guard that holds even against a database
restored from an old backup.

### Dry run (always do this first)

```
glc-admin refund-manual-review --config PATH --request-id N --note "why this is being refunded"
```

Prints: request id and state, the manual-review reason, the original
deposit (obligation index + PDA — **the bridge stores no deposit
transaction signature anywhere; the finalized obligation account *is* the
verified deposit record**), the original sender wallet, the source token
account, the derived refund destination and whether it exists, mint, token
program, the exact refund amount and the fee interpretation, whether a
Goldcoin payout exists, whether a prior refund exists, reserve balance
before/after, the protected minimum, the pause state, the attestation
threshold, and every safety check individually as PASS/FAIL with an
overall verdict.

**Verify the destination independently** before executing: derive
`ATA(requester, reserve mint, Token-2022)` yourself — e.g.
`spl-token address --owner <REQUESTER> --token <RESERVE_MINT> --program-2022`
— and confirm it equals the printed destination, and that the printed
requester matches the depositor you expect from the original on-chain
deposit transaction.

### Execute

```
glc-admin onchain-pause --rpc-url URL --keypair ADMIN_KEY --scope global --note "manual review refunds"
glc-admin refund-manual-review --config PATH --request-id N --note TEXT --keypair ADMIN_KEY --execute
# ... repeat per request; each is individually checked and idempotent ...
glc-admin onchain-unpause --rpc-url URL --keypair ADMIN_KEY --scope global --note "refunds complete"
```

The pause is **never** engaged or lifted by the refund command itself —
that stays an explicit, separately audited operator action, so the
security boundary is visible in the audit log rather than implied.

Execution order, each database step atomic with its own audit row:
re-check everything against fresh state -> record `RefundPending` ->
collect attestations -> **re-check global pause, protected minimum, and
nonce immediately before simulating** -> simulate (a failed simulation
blocks the broadcast unconditionally, `--execute` or not) -> record the
signature and blockhash **before** sending -> broadcast -> confirm at
`finalized` -> `Refunded` + debit the cached SolanaReserve balance.

### Re-running, crash recovery, and rollback expectations

Safe to re-run at any point; it never resolves uncertainty by building a
second transfer:

- **Already `Refunded`** — reports the existing transaction and exits 0.
- **`RefundBroadcast`** — reads the on-chain state back. If the refund's
  nonce PDA exists (and matches this refund's amount/destination), the
  transfer happened: it finalizes the bookkeeping. If not, and the
  recorded blockhash is still landable, it waits for a definite outcome.
  Only once the recorded transaction is *positively* dead (blockhash can
  no longer land **and** no nonce PDA, or it landed and failed) does it
  rebuild — under the **same** nonce.
- **`RefundPending`** — resumes from attestation collection.
- A crash between recording the broadcast and the actual send is the same
  case: the recorded intent plus the nonce PDA make the outcome
  determinable.

There is no "undo": once the transfer confirms, the funds are with the
depositor and the request is terminal. If a refund is broadcast in error,
the compensating action is a new, ordinary deposit by that party — not a
database edit. A refund that has *not* yet broadcast can simply be left
alone (the request stays `RefundPending` and inert; nothing else will ever
act on it).

### Verifying the request is permanently closed

- `glc-admin refund-list --db PATH` shows the row as `Confirmed` with its
  transaction signature; `glc-admin status --db PATH` no longer counts the
  request in the `ManualReview` backlog.
- `glc-admin resume-manual-review --db PATH --request-id N --note TEXT`
  refuses with a refund-lifecycle error — from **any** surface (CLI, admin
  API, or the daemon's automatic recovery), since all three call the same
  ledger function. The refusal keys on the `solana_refunds` row itself, so
  it holds even if `bridge_requests.state` were edited out of band.
- No Goldcoin payout can be created for the request: the guard sits in
  `Ledger::record_goldcoin_payout_built`, the single point every payout row
  is born, not only in the CLI.
- The refunded amount is debited from the cached SolanaReserve balance in
  the same transaction that marks it `Refunded`, and a
  broadcast-but-unconfirmed refund is an explicit in-flight explanation
  term in reconciliation — so a refund never trips the unexplained-drop
  auto-pause, and never hides a real one.

### Accounting

A fold-time park never reserved Goldcoin liquidity, so a refund releases
nothing there — and this is *proved* per request (the state-log check
above) rather than assumed; a request that ever held a reservation is
refused instead of blindly subtracted from. `reserved_liquidity`,
`pending_obligations`, `settled_liquidity_total`, and `accrued_fees_atomic`
are all untouched by the refund path: a refund is not a settlement.

### Batch refunds

Not implemented, deliberately. Drain a backlog with `refund-list` +
per-request dry run + per-request `--execute`; each request is checked and
made idempotent on its own. There is no `refund-all`.

### NEVER do this instead

**Do not** send a manual SPL/Token-2022 transfer from the reserve and then
edit the database to match. A hand-made transfer bypasses every guard
above — the attestation threshold, the protected-minimum check, the
eligibility whitelist, the replay guard, the audit trail — and a
hand-edited row will not release/close the request correctly, will not be
recognized by reconciliation (it will surface as an unexplained balance
drop and auto-pause the reserve), and destroys the request/deposit/refund
linkage an auditor needs. If this command refuses, the refusal is the
answer: fix the named cause, or escalate — never route around it.

### Schema rollback (v17) — read before rolling back a release

The refund feature adds schema **v17** (`solana_refunds`). Migration is
automatic on first daemon start, additive only (`CREATE TABLE IF NOT
EXISTS` — no table rebuild, no column rewrite, no data movement), and
touches no existing table, so upgrading is safe and re-runnable.

**Rolling BACK to a pre-v17 binary is not supported and must not be done
casually**, for two reasons established by inspection, not assumption:

1. A pre-v17 binary does not know the `RefundPending`/`RefundBroadcast`/
   `Refunded` state strings. Any read that parses a refunded request's
   row fails with `InvalidColumnType` — specifically `Ledger::get_request`
   and `Ledger::transfers_page`, i.e. the public `GET /transfers/{id}` and
   `GET /transfers?address=...` endpoints and the admin API's
   request-detail reads, for the affected requests only. **Settlement is
   unaffected**: every daemon loop selects by an explicit state
   (`requests_by_state`) or off `goldcoin_payouts`, and reserve accounting
   uses aggregate SQL, so none of them ever parse a refund state. Verified
   empirically, not inferred.
2. A pre-v17 binary's migration ladder has no forward-compatibility guard,
   so it would silently `UPDATE schema_version SET version = 16` on a v17
   database — relabelling it as older while it still physically carries
   `solana_refunds` and its rows. No data is lost (rolling forward
   re-applies v17 idempotently), but the version marker would be wrong in
   the meantime.

From v17 onward this is prevented: `schema::open_and_migrate` refuses to
open any database whose `schema_version` exceeds the running binary's
`CURRENT_SCHEMA_VERSION` (`LedgerError::SchemaTooNew`) rather than
stamping an older version over it. That guard protects every future
rollback; it cannot retroactively protect a rollback to a binary that
predates the guard itself.

**If a rollback past v17 is genuinely required**: stop the daemon, and
restore a pre-upgrade backup with `scripts/restore-ledger.sh` rather than
pointing the old binary at the current database. Any refund executed after
the upgrade is a real, irreversible on-chain transfer — a restored older
database will not contain its record, so reconcile those refunds manually
(they are visible on-chain as `rebalance_withdrawal` PDAs under the refund
nonce domain, and in `admin_audit_log` in the un-restored database) before
resuming operation.

### Regression coverage

`ledger::tests` (eligibility, whitelist, cross-checks, capacity,
lifecycle, restart, concurrent-begin, resume/payout guards),
`solana::refund::tests` (dry-run purity, exactly-one-transaction,
Token-2022 ATA derivation, idempotent rerun, crash recovery in all three
shapes, simulation-blocks-broadcast, pause re-check, wrong mint/program,
on-chain settlement evidence, insufficient reserve, nonce-without-row),
`orchestrator::tests` (auto-resume never revives a refunded request and is
not stalled by one), and `ledger::schema::tests` (v17 migration, its
constraints, and the forward-compatibility guard refusing a newer-than-
supported database instead of downgrading it).

## SolToGlc recipient rate limit (added 2026-08-27)

### The rule

A Goldcoin L1 recipient address may receive at most one accepted SolToGlc bridge payout in a rolling 24-hour window (`Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS`, 86,400 seconds). The window starts at the first accepted request's `created_at`. Any new SolToGlc obligation to the same recipient inside that window is parked in `ManualReview` with `manual_review_note = "recipient_rate_limited"` instead of proceeding to payout — checked by `Ledger::fold_sol_deposit` before any reservation is made, so a rate-limited fold never consumes reserve capacity. Different recipient addresses are completely independent. GlcToSol is unaffected — this check only ever runs in `fold_sol_deposit` (SolToGlc's admission path).

"Accepted" is an exclude-list, not an include-list: every `SolToGlc` row created inside the window counts (`SourceFinalized`, `ManualReview` for any reason, `SettlementAuthorized`, `DestinationSubmitted`, `DestinationConfirmed`, `Settled`, ...) EXCEPT the terminal states that mean no payout resulted or ever will (`Failed`, `DestinationSubmissionFailed`, `InsufficientReserveAtSettlement`, `Cancelled`, `Expired`, `Reorged`) — a request that never created a real obligation, or that failed/was cancelled before one completed, does not count against the recipient. A request already sitting in `ManualReview` for some other reason (e.g. `utxo_liquidity_low_at_fold`) still counts, since it remains a live obligation that can still result in a payout.

### Manual resume cannot bypass the window

`Ledger::resume_manual_review_sol_to_glc` re-checks the SAME rate limit unconditionally on every resume attempt — regardless of the request's own `manual_review_note`. An operator cannot resume a request early just because it happened to be parked for a different reason; if the recipient is still inside its window (because of some OTHER request to the same address), the resume is refused with `LedgerError::RecipientRateLimited { retry_after, .. }`, leaving the request untouched. Retrying the identical command once `retry_after` passes succeeds normally — a transient, self-clearing refusal, exactly like `LedgerError::UtxoLiquidityLow`.

**Only a strict predecessor can ever block a candidate.** The blocker search is restricted to rows ordered `(created_at, id)` strictly BEFORE the candidate's own — never a later-arriving sibling, and never itself. This is what makes "oldest first" actually true for a recipient with several queued rows: candidate `C`'s eligibility can only ever depend on the request immediately before it in creation order, never on anything that showed up after `C` did. Without this restriction, a later-arriving sibling (necessarily still parked, since it too was rate-limited on arrival) could shadow-block an earlier, rightfully-next-in-line candidate — inverting the drain order, and under a steady trickle of new same-recipient arrivals, potentially starving the oldest parked request indefinitely. (This was a real HIGH-severity finding, fixed before merge — see `service/src/ledger/mod.rs`'s `resume_manual_review_sol_to_glc` doc comment.)

### Automatic resume

Once the blocking predecessor ages out of the 24-hour window, a `recipient_rate_limited` request resumes automatically, oldest first, subject to every normal safety check — see "Automatic recovery, without an operator" above. Because eligibility only ever depends on a candidate's immediate predecessor, a backlog of several queued rows to the same recipient drains strictly in creation order: the oldest becomes eligible first (once its predecessor's window clears), the next becomes eligible only once *that* one's own window clears in turn, and so on — never out of order, and never blocked by anything newer. No operator action is required in the common case; `glc-admin resume-manual-review` remains available for the same request and will simply refuse (not error out destructively) if its predecessor's window has not actually cleared yet.

### Pre-transaction eligibility read (added 2026-08-27, extended 2026-08-28)

`GET /recipients/sol-to-glc/eligibility?address=<Goldcoin p2pkh address>&wallet=<base58 Solana pubkey, optional>` answers, read-only, whether a NEW SolToGlc obligation naming that recipient — and, when `wallet` is given, deposited from that Solana wallet — would currently be admitted or parked by EITHER rate limit: `{direction, address, wallet, eligible, blocked_reason, retry_after, retry_after_seconds, window_seconds}`, where `blocked_reason` is `"source_wallet_rate_limited"` or `"recipient_rate_limited"` (`null` when eligible, checked wallet-first when both would block — see the next section), `retry_after` is the absolute unix second the blocking window reopens (`null` when eligible), and `retry_after_seconds` the same instant as remaining seconds. `wallet` is optional — omitting it means only the recipient leg is checked, same as before this dual limit existed. It is served by `Ledger::sol_to_glc_recipient_rate_limited_until` and `Ledger::sol_to_glc_source_wallet_rate_limited_until`, each running the SAME shared window query its respective admission check uses, so the answer can never disagree with what admission would actually do. The bridge UI calls it as soon as a wallet is connected AND a valid Goldcoin destination address is entered, AND again immediately before invoking the wallet, so a user is warned before signing a Solana transaction whose deposit would only be parked. Purely advisory by construction: admission re-checks both rules at fold time inside the write transaction, so a stale or bypassed answer here can never weaken either limit, and the endpoint discloses nothing beyond the boolean, which limit is blocking, and the reopen time (no blocking request id, amount, or state).

### Race-safety

The rate-limit query, the row insert (`fold_sol_deposit`) or state update (`resume_manual_review_sol_to_glc`), and the reservation increment all run inside the SAME `BEGIN IMMEDIATE` SQLite transaction — SQLite's write lock serializes every mutating ledger call DB-wide, so two concurrent obligations to the same recipient can never both observe "no blocking row yet" and both proceed; the second one always sees the first's already-committed row. The source-wallet limit below shares this same guarantee, keyed on `requester` instead of `recipient`.

### Regression coverage

`service/src/ledger/tests.rs`: same recipient inside 24h (parked), same recipient after 24h (accepted), different recipients (independent), restart/idempotency, an in-flight `ManualReview`/`DestinationSubmitted`/`Settled` obligation all counting against the recipient, a cancelled/failed obligation never counting, manual resume refusing while still inside the window, GlcToSol completely unaffected — plus, for the predecessor-only ordering fix specifically: three queued requests to the same recipient resuming strictly oldest-first, the newest of the three remaining blocked until the middle one's own window elapses, a flood of newer same-recipient arrivals never starving the oldest parked request, and that ordering surviving a simulated restart. `service/src/orchestrator/tests.rs`: automatic resume draining a `recipient_rate_limited` entry once its window clears, and a still-rate-limited candidate being skipped (not a batch stop) in a mixed-reason batch. See "SolToGlc source-wallet rate limit" below for the dual-key regression coverage.

## SolToGlc source-wallet rate limit (added 2026-08-28)

### The rule

A single Solana source wallet could bypass the recipient-only limit above by spreading deposits across many different Goldcoin recipients — the recipient limit alone never noticed, since each individual recipient was still fresh. This adds a SECOND, INDEPENDENT rolling-24-hour limit, keyed by the Solana wallet instead of the Goldcoin recipient, enforced ALONGSIDE the recipient limit (never replacing it): a Solana source wallet may make at most one qualifying SolToGlc deposit in a rolling 24-hour window (`Ledger::RECIPIENT_RATE_LIMIT_WINDOW_SECS`, the SAME 86,400-second constant, shared by both limits so they cannot drift apart on the window itself). Any new SolToGlc obligation from the same wallet inside that window is parked in `ManualReview` with `manual_review_note = "source_wallet_rate_limited"` — checked by `Ledger::fold_sol_deposit` before any reservation is made, using the identical exclude-list and matching semantics as the recipient rule (`Ledger::source_wallet_rate_limit_blocker_created_at`, the structural mirror of `recipient_rate_limit_blocker_created_at`). GlcToSol is unaffected, for the same reason as the recipient limit — this only ever runs in `fold_sol_deposit`.

**Identity, not a client-supplied string.** The wallet key is `requester`, decoded straight from the on-chain `WithdrawalObligation` account (`solana::accounts::decode_withdrawal_obligation`, offset 16, immediately after `index`/`amount`) — which the `glc-reserve-bridge` Anchor program itself sets to `ctx.accounts.user.key()`, the `Signer` that authorized the `deposit_to_reserve` instruction (`programs/glc-reserve-bridge/src/instructions/deposit_to_reserve.rs`). There is no code path by which a caller can set this to anything other than the wallet that actually signed the deposit — it is threaded verbatim from that on-chain account through `solana::indexer::tick`'s `snap.requester.to_bytes()` into `fold_sol_deposit`'s `requester: [u8; 32]` parameter, never taken from request headers, form input, or any other client-controlled source.

### Manual resume cannot bypass either window

`Ledger::resume_manual_review_sol_to_glc` re-checks BOTH rate limits unconditionally on every resume attempt, independently — an operator cannot resume a request early because it happens to be clear of one limit while the other still applies. If the source wallet is still inside its window, the resume is refused with `LedgerError::SourceWalletRateLimited { retry_after, .. }` (checked first); if only the recipient is still inside its window, `LedgerError::RecipientRateLimited` (unchanged from before). Same strict-predecessor-only blocking rule as the recipient limit (a resume candidate can only ever be blocked by an earlier row from the SAME wallet, ordered `(created_at, id)`), for the identical oldest-first-draining reason documented above.

### Automatic resume

Covered by the same `Orchestrator::tick_auto_resume_utxo_liquidity_backlog` pass as the recipient limit (see above) — a `source_wallet_rate_limited` candidate is drained automatically once its wallet's window clears, and skipped (never a batch stop) while it doesn't, exactly like `recipient_rate_limited`.

### UI enforcement (added 2026-08-28)

The bridge UI's `GET /recipients/sol-to-glc/eligibility?wallet=` leg (see "Pre-transaction eligibility read" above) surfaces this to the user BEFORE Phantom is ever invoked: "This Solana wallet has already used the bridge in the last 24 hours." — shown alone (never alongside the recipient message) when the connected wallet itself is blocked, disabling the Deposit button and re-checked fresh immediately before submission so a wallet that becomes rate-limited between form-fill and click still never reaches the wallet-open step. The backend remains the sole enforcing authority regardless: a direct `deposit_to_reserve` observation that bypasses the UI/API preflight entirely is still independently caught by `fold_sol_deposit` and safely parked in `ManualReview`, never silently accepted.

### Regression coverage

`service/src/ledger/tests.rs`: same wallet inside 24h to a DIFFERENT recipient (parked — the exact bypass this closes), a different wallet to the SAME already-paid recipient (still blocked, by the pre-existing recipient rule, proving the new limit never replaced it), a different wallet AND different recipient (unaffected), same wallet after the window ages out (accepted), manual resume refusing/self-excluding for the wallet leg, both limits refusing independently in the same ledger, a direct replay of the same obligation index never being reinterpreted as a rate-limit hit, a cancelled/failed obligation never counting against its wallet, GlcToSol unaffected, plus the read-only `sol_to_glc_source_wallet_rate_limited_until` view agreeing with `fold_sol_deposit` at every boundary. `service/src/orchestrator/tests.rs`: automatic resume draining a `source_wallet_rate_limited` entry once its window clears. `service/src/api/tests.rs`: the eligibility endpoint checking the wallet leg, reporting the correct `blocked_reason` when only one limit (or both) apply, and routing `?wallet=` end to end over real HTTP. UI: `tests/unit/bridge-card-source-wallet-rate-limit.test.tsx` (glc-solana-reserve-bridge-ui) covers the same-wallet/different-recipient block, the different-wallet/same-recipient block via the unchanged recipient rule, the pre-submit race re-check, and the auto-unblock poll — mirroring `bridge-card-recipient-rate-limit.test.tsx`, whose own tests are unchanged and still pass.

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

### 8. Manually discarding the remaining wait (`reset-rolling-window`, added 2026-08-29)

Item 1 above describes the window's own automatic reset — real, but only once the FULL `rolling_window_seconds` (24h) has elapsed since the bucket started. An operator who has already refilled/rebalanced the reserve and verified accounting has no reason to wait out the remainder of that 24h — `glc-admin reset-rolling-window` is the administrative override for exactly this: it manually reopens ONE direction's on-chain rolling-volume window immediately, without editing SQLite and without fabricating a timestamp by hand.

**This is a deliberate override of the anti-drain protection the rolling-volume window exists to provide.** Use it only after independently verifying reserve/accounting state (steps A-C below) — never as a routine substitute for letting the window age out on its own, and never before confirming the exhaustion was actually caused by legitimate volume rather than something that still needs investigating.

#### What it does, and does not, touch

Resets exactly one `RollingVolumeWindow` PDA — `window_start` becomes the current on-chain clock's `unix_timestamp` (a fresh window starting now, from the real trusted clock, never operator-supplied), `window_total` becomes `0`. Nothing else changes: reserve balances, obligations, `protected_minimum`, `per_transfer_limit`, `rolling_volume_limit`, `min_transfer_amount`, and the OTHER direction's window are all left exactly as they were (`programs/glc-reserve-bridge/src/instructions/admin.rs::reset_rolling_volume_window`'s account list contains nothing else to touch). Emits `RollingVolumeWindowReset` (admin pubkey, direction, previous and new `window_start`/`window_total`, unix timestamp, slot) for the audit trail.

#### Authorization and preconditions

Same `BridgeConfig.admin`-gated authorization as `onchain-pause`/`set-limit` (`instructions::admin::AdminConfig`'s pattern) — any other signer is rejected with `UnauthorizedAdmin`. Additionally requires `BridgeConfig.paused == true` (global pause already engaged) — refused with `BridgeNotPaused` otherwise; this is a conscious precondition the operator must satisfy first, same discipline as `rebalance_withdraw`. Does **not** require the individual direction's own `release_paused`/`deposit_paused` flag to also be set.

#### Exact operator commands

```
# Reset the GLC L1 -> Solana (release) rolling-volume window
glc-admin reset-rolling-window --rpc-url URL --keypair ADMIN_KEY --direction glc-to-sol --note "TEXT"

# Reset the Solana -> GLC L1 (deposit) rolling-volume window
glc-admin reset-rolling-window --rpc-url URL --keypair ADMIN_KEY --direction sol-to-glc --note "TEXT"
```

`glc-to-sol` maps to the RELEASE window (`Direction::GoldcoinToSolana`, on-chain direction byte `0`); `sol-to-glc` maps to the DEPOSIT window (`Direction::SolanaToGoldcoin`, byte `1`) — the same mapping table in section 1 above. `--note` is required (mandatory audit trail, same as every other on-chain `glc-admin` command) and is recorded in this command's own printed output and the transaction history itself; it is not written to a separate local audit-log file, since none exists for this class of command today.

#### Full maintenance sequence

A quota-driven maintenance window that includes a deliberate window reset should follow this order, not an ad hoc one:

```
A. glc-admin onchain-pause --scope global --keypair ADMIN_KEY --rpc-url URL --note "maintenance: starting"
B. Refill/rebalance reserves as necessary (glc-rebalance-withdraw / operator-side funding, per the reserve-sizing runbook)
C. Verify reserve invariants and Goldcoin UTXO maturity (glc-admin status, ops::reserve_health, the pre-admission reconciliation report)
D. If, and only if, the operator intentionally wants to discard the remaining rolling-window wait:
     glc-admin reset-rolling-window --direction <glc-to-sol|sol-to-glc> --keypair ADMIN_KEY --rpc-url URL --note "TEXT"
     (repeat for the other direction if both need it — each call touches exactly one window)
E. glc-admin show-config   # verify rolling remaining/quota state before reopening anything
F. glc-admin unpause --db PATH --direction <goldcoin|solana> --note "TEXT"   # this service's own local gate, per direction, as needed
G. glc-admin onchain-unpause --scope global --keypair ADMIN_KEY --rpc-url URL --note "maintenance: complete"   # LAST
H. Verify GET /status and glc-admin show-config both reflect the fully-reopened, reconciled state
```

Global on-chain unpause is deliberately step G, not earlier — every step before it runs with the strongest available circuit breaker still engaged, and reopening settlement is the one irreversible-in-effect action in this sequence (a transfer can be admitted the instant it clears), so it comes only after every verification step, never before.

#### API/status after reset

`GET /status` naturally reports the reset correctly with **no service-side change** — `glc_to_sol_rolling_volume_remaining`/`sol_to_glc_rolling_volume_remaining` and `*_quota_exhausted` are derived live from the same on-chain `RollingVolumeWindow` account `service/src/solana/accounts.rs::rolling_volume_remaining` always reads, so once the reset transaction lands, the very next `/status` poll shows `remaining` equal to the full configured `rolling_volume_limit` and `quota_exhausted = false` for that direction — never a value this API fabricates or caches independently of on-chain state.

#### Test coverage

`programs/glc-reserve-bridge/tests/reset_rolling_volume_window.rs` — valid admin reset of each direction, non-admin rejection, rejection while global pause is `false`, no requirement that the individual direction also be paused, resetting one direction leaving the other completely untouched, `rolling_volume_limit`/reserve accounting unchanged, remaining capacity returning to the full configured limit with `quota_exhausted` becoming false, subsequent volume counted normally from the fresh state (not appended to the discarded total), and a repeated reset while still paused behaving deterministically. `service/src/solana/instructions.rs` — the off-chain instruction encoder's discriminator/account-ordering/direction-to-PDA mapping.

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

The 3% bridge fee (docs/20-bridge-fee.md) accrues on the SOURCE reserve's
row (`reserve_ledger.accrued_fees_atomic`, canonical units, visible via
`glc-admin status` and the `/metrics` endpoint) and stays there — this
phase has **no treasury wallet/address and no fee-withdrawal path**, by
design. Accrued fees are never automatically moved anywhere and are never
counted toward `available_capacity`/the reserve invariant; they are purely
an audit-visible running total. Standing up a withdrawal procedure (who
authorizes it, where funds go, how it's distinguished from a rebalance in
the ledger) is future work, not yet scoped here.

## Admin API & admin UI (added 2026-08-29)

Full reference: [27-admin-control-plane.md](27-admin-control-plane.md).
Operational summary:

- The daemon serves an authenticated admin API when
  `service.admin_bind_addr` is configured (bind privately;
  `config.pilot-template.toml` shows the shape). Operators are listed as
  `{ name, token_env }` — the bearer token lives only in the named env
  var, never in the config file. One token per person; the token's
  operator name is the `actor` on every audit row.
- **UI-executable** (through the admin UI / API, mandatory note,
  audited): local pause/unpause per direction, admission close, admission
  open (same invariant + UTXO-liquidity gates as `glc-admin
  open-admission` — one shared implementation), resume-manual-review
  (same unconditional safety and rate-limit checks as `glc-admin
  resume-manual-review`), and the full rebalance request workflow.
  **Local pause stops new admissions/starts, NOT in-flight
  settlements** — requests already past `SourceFinalized` still settle
  on subsequent ticks (pre-existing semantics, unchanged). The full
  money-movement stop is the ON-CHAIN global pause below; see
  docs/27-admin-control-plane.md "What local pause does and does not
  stop".
- **CLI approval required** (the admin keypair never leaves the
  operator's machine): `glc-admin onchain-pause`, `glc-admin
  onchain-unpause`, `glc-admin set-limit`, `glc-admin
  reset-rolling-window` — the UI shows current on-chain state read-only
  and generates the exact command (atomic units converted server-side)
  for the operator to review and run over SSH, exactly as documented in
  the "Executable commands" section above.
- Never UI-reachable at all: `glc-admin retry-goldcoin-payout`,
  `glc-admin split-vault-utxo` (they sign and broadcast), every
  custody-transition workflow (the CLI's custody subcommands), and the
  bridge fee (a compile-time constant — docs/20-bridge-fee.md's staged
  fee-change process).
- Audit trail: the `admin_audit_log` table (docs/06-schema.md, schema
  v15) records every mutation attempt, refusals included; query it from
  the UI's Audit Log page or `GET /audit-log`.

## Explicitly deferred to real operational experience

Exact reserve thresholds, rebalance cadence, rolling-volume window size, per-transfer limits: all configuration, none defaulted in this document, per the old bridge's precedent of refusing to assert production security parameters without operational data (`docs/custody.md`'s open items #7/#8 were left open for the same reason — better an explicit open decision than a silently wrong default). See [12-management-decisions.md](12-management-decisions.md).

**Update 2026-08-21: confirmation depths are no longer on this deferred list for the pilot specifically** — see "Confirmation-depth values (pilot, approved 2026-08-21)" above for the actual interim numbers now in effect. The *final*, historical-data-backed values remain deferred, per docs/12 item 4, and are a scale gate rather than a pilot concern.
