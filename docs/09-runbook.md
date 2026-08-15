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

**No procedure exists yet** for: staged multi-operator approval of attestation-key rotation (the old bridge's equivalent depended on a P2P federation transport this bridge does not have and does not need — see IMPLEMENTATION_LOG.md's Phase 5 entry for why a simpler out-of-band-signature-collection design is the right replacement, not yet built) and the Goldcoin vault sweep-to-fresh-vault compromise-response procedure. **Rebalancing's off-chain engineering layer is now built** (the commands above: imbalance detection, a proposal/approval/execution-evidence/confirmation state machine, structural separation from settlement accounting, replay protection, reconciliation interaction, restart recovery, and audit trail) — what remains unbuilt is the dedicated on-chain `rebalance_deposit`/`rebalance_withdraw` instructions docs/03-architecture.md originally envisioned for the Solana leg specifically (an atomic, on-chain-enforced structural separation between a rebalance transfer and an arbitrary one); until those exist, the real fund movement a rebalance evidences is an ordinary SPL/Goldcoin transfer executed through whatever wallet/custody tooling already holds the relevant keys, not a bespoke program instruction. These gaps are named explicitly below wherever the procedure that needs them is described, so they stay visible rather than silently assumed away.

## Reserve sizing

Per management's stated principle: **reserve levels should cover the largest expected net outflow between operational rebalances.** Concretely:

```
target_reserve(direction) =
    expected_peak_directional_volume_per_rebalance_interval
  + safety_margin
  + protected_minimum
```

`expected_peak_directional_volume_per_rebalance_interval` and `safety_margin` are operational judgment calls informed by observed volume once the bridge is live; no value is asserted here. `rebalance_interval` itself is a policy choice (fixed schedule vs. threshold-triggered) — see [12-management-decisions.md](12-management-decisions.md).

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
4. **Rotate**: for the Solana leg, execute attestation-key rotation via the timelocked governance instruction (already built on-chain — `propose/execute/cancel_attestation_key_rotation`, Phase 2) once a clean replacement key is provisioned in a fresh custody domain; **no `glc-admin` command stages the required multi-operator approval yet** (see "Executable commands" above — the old bridge's staged-approval CLI depended on a P2P transport this bridge doesn't have, and the simpler out-of-band-signature replacement isn't built). For the Goldcoin leg, execute the sweep-to-fresh-vault procedure — **no procedure or command exists for this yet**; the old bridge's `sweep.rs` and its independent-commitment-re-derivation discipline (docs/01-reuse-inventory.md) is the intended shape but nothing has been ported.
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

Confirmation depths, exact reserve thresholds, rebalance cadence, rolling-volume window size, per-transfer limits: all configuration, none defaulted in this document, per the old bridge's precedent of refusing to assert production security parameters without operational data (`docs/custody.md`'s open items #7/#8 were left open for the same reason — better an explicit open decision than a silently wrong default). See [12-management-decisions.md](12-management-decisions.md).
