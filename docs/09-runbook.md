# Operational Runbook (Draft)

Structured after the old bridge's `docs/runbooks.md` discipline: every procedure here should eventually be backed by an executable `glc-admin`/`glc-audit` command, asserted by CI to actually exist and behave as documented (reused practice — the old bridge's `runbook_commands.rs` caught real drift between docs and binaries repeatedly; ported to `service/tests/runbook_commands.rs`). [07-implementation-plan.md](07-implementation-plan.md) Phase 5 landed a first, deliberately partial set of real commands — see "Executable commands" below for exactly what exists today and what is explicitly still a paper procedure.

## Executable commands (Phase 5)

What actually exists, so this document never claims more than the binaries do:

- `glc-admin status --db PATH` — reserve snapshots (both directions) and the `ManualReview` backlog count.
- `glc-admin pause --db PATH --direction <goldcoin|solana> --note TEXT` / `glc-admin unpause ...` — this service's own local ledger admission gate (independent of the on-chain pause below).
- `glc-admin show-config --rpc-url URL` — decodes and prints the on-chain `BridgeConfig`.
- `glc-admin onchain-pause --rpc-url URL --keypair PATH --scope <global|release|deposit> --note TEXT` / `glc-admin onchain-unpause ...` — submits the admin-gated-immediate `set_paused` instruction (docs/12-management-decisions.md's Phase 2 scoping decision: pause is admin-gated-immediate, not threshold-gated).
- `glc-audit --db PATH [--quiet]` — offline integrity auditor: re-verifies every frozen attestation-claim commitment plus `PRAGMA integrity_check`. Exit 0 = clean, 1 = findings, 2 = could not run.

**No procedure exists yet** for: staged multi-operator approval of attestation-key rotation (the old bridge's equivalent depended on a P2P federation transport this bridge does not have and does not need — see IMPLEMENTATION_LOG.md's Phase 5 entry for why a simpler out-of-band-signature-collection design is the right replacement, not yet built), the Goldcoin vault sweep-to-fresh-vault compromise-response procedure, and rebalancing (the `rebalance_deposit`/`rebalance_withdraw` on-chain instructions themselves are not yet built — Phase 2 scoping decision #2, still open). These are named explicitly below wherever the procedure that needs them is described, so the gap stays visible rather than silently assumed away.

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

**No procedure exists yet** for steps 2 and 4 below — the `rebalance_deposit`/`rebalance_withdraw` on-chain instructions are not yet built (Phase 2 scoping decision #2, still open per IMPLEMENTATION_LOG.md), so there is no `glc-admin` subcommand for staging a rebalance and no dedicated rebalance instruction to execute against. What follows describes the intended shape once that work lands, not a procedure an operator can run today.

1. Operator determines direction and amount needing rebalance (from Warning/Critical alerts or scheduled review).
2. Stage a rebalance intent via a `glc-admin` subcommand analogous to `rebalance-plan` (or Goldcoin-side equivalent, not yet built) — produces an exact commitment (amount, direction, destination) for approval, reusing the staged-approval pattern from old ADR-0021.
3. Required custody-domain approvals collected (per the ratified trust model — e.g. 2-of-3 for whichever reserve is being topped up).
4. Execute via the dedicated `rebalance_deposit`/`rebalance_withdraw` path ([03-architecture.md](03-architecture.md)), which is structurally incapable of being recorded as a user settlement (separate instruction, separate ledger table — [05](05-reserve-accounting.md), [06-schema.md](06-schema.md)).
5. Reconciliation job picks up the new `total_reserve_balance` on its next tick; if the rebalance clears `critical_reserve`, the directional pause is lifted — **automatically if the pause was reserve-triggered and nothing else is holding it, otherwise requires operator confirmation** (a pause triggered by a reconciliation *mismatch* should never auto-clear just because the balance number looks healthy again — see below). Today, lifting a directional pause is always `glc-admin unpause`/`glc-admin onchain-unpause`, operator-run, with `--note`.

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

## Explicitly deferred to real operational experience

Confirmation depths, exact reserve thresholds, rebalance cadence, rolling-volume window size, per-transfer limits: all configuration, none defaulted in this document, per the old bridge's precedent of refusing to assert production security parameters without operational data (`docs/custody.md`'s open items #7/#8 were left open for the same reason — better an explicit open decision than a silently wrong default). See [12-management-decisions.md](12-management-decisions.md).
