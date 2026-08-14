# Testing and Acceptance Plan

Layered: unit → integration (mocked chains) → real-node (regtest Goldcoin + local Solana validator) → rehearsal (fault injection, as automated CI, reusing the old bridge's proven "rehearsal as tests, not ceremony" discipline).

## Functional coverage

| Scenario | Layer | Notes |
|---|---|---|
| Normal Goldcoin → Solana settlement | Real-node | End-to-end, both directions of the asymmetry named in [10-threat-model.md](10-threat-model.md) exercised separately |
| Normal Solana → Goldcoin settlement | Real-node | |
| Concurrent transfers, same direction | Real-node | Verifies row-level-lock reservation serialization ([05-reserve-accounting.md](05-reserve-accounting.md)) under real concurrency, not just unit-level mutex tests |
| Concurrent transfers, opposite directions | Real-node | Confirms the two directions' reserve ledgers never contend |
| Reserve depletion (request exceeds `available_capacity`) | Integration + real-node | Must reject at creation, never partially reserve |
| Minimum reserve boundary (`protected_minimum`) | Integration | Exact-boundary and one-atomic-unit-over/under cases |
| Reservation expiry | Integration | Includes the late-deposit-after-expiry case from [04-state-machines.md](04-state-machines.md) — capacity-available and capacity-unavailable sub-cases both required |
| Duplicate deposit processing | Integration | Same source txid/vout submitted via two concurrent indexer ticks |
| Replay attempts | Real-node | Attempt to re-authorize an already-`Settled` claim on-chain (GLC→SOL) and via direct DB/API manipulation attempt (SOL→GLC) — the two directions need separate test designs given the asymmetry |
| Duplicate destination submission | Integration | Retry of `DestinationSubmitted` step must not double-broadcast/double-transfer |
| Crash before payout | Real-node, fault injection | Kill service mid-`SettlementAuthorized`, verify resume without double-pay |
| Crash after payout but before DB commit | Real-node, fault injection | The hardest case — payout broadcast succeeds, process dies before recording it; verify reconciliation-on-restart detects the on-chain payout and correctly marks `Settled` rather than re-attempting |
| Restart recovery | Real-node | Full service restart at every state in [04-state-machines.md](04-state-machines.md), verify correct resume/no-duplicate-action for each |
| RPC outage (Goldcoin, Solana, each independently) | Integration, fault injection | Verify `Transport`-class retry/backoff and no false-negative state transitions |
| Delayed finality | Real-node | Confirmation depth reached slower than expected; verify no premature settlement |
| Source reorg (pre-finality) | Real-node | `invalidateblock`/`reconsiderblock` against regtest, reused verified mechanics |
| Source reorg (post-finality) | Real-node, fault injection | Must trigger global pause + paged incident per [10-threat-model.md](10-threat-model.md), never silent |
| Destination failure (broadcast rejected, insufficient fee, etc.) | Integration | `DestinationSubmissionFailed` retry-then-`ManualReview` path |
| Accounting mismatch | Integration | Deliberately desync ledger vs. chain balance, verify reconciliation `BREACH` classification and auto-pause |
| Stale database (service resumes against an old DB snapshot) | Real-node, fault injection | Restore an old backup, verify reconciliation detects the gap rather than resuming as if current |
| Rate-limit exhaustion (per-transfer, rolling-volume) | Integration | Both program-enforced (GLC→SOL) and service-enforced (SOL→GLC) paths |
| Emergency pause | Integration | Global pause blocks both directions at every gated transition point |
| Directional pause | Integration | One direction blocked, other unaffected |
| Reserve replenishment (rebalance) | Real-node | Verify rebalance never appears in `bridge_requests`/settlement accounting, only `rebalance_events` ([06-schema.md](06-schema.md)) |
| Unauthorized payout attempt | Real-node | Attempt `release_from_reserve`/Goldcoin payout without valid threshold attestation — must be rejected on-chain (GLC→SOL) and at the signing-client layer (SOL→GLC) |
| Compromised service simulation | Rehearsal (real-node) | Service orchestrator credentials assumed fully compromised; verify attacker cannot produce a valid settlement without meeting the signing threshold — direct test of the [10-threat-model.md](10-threat-model.md) "compromised service credentials" row |
| Repeated bidirectional transfers | Real-node | Many consecutive transfers both directions, verify no drift in reserve accounting over time |
| Exact 1:1 accounting | Real-node, continuous | Every real-node test run asserts `settled_liquidity` deltas match confirmed deposit deltas exactly, atomic-unit-for-atomic-unit, per [05-reserve-accounting.md](05-reserve-accounting.md)'s exactness invariant |

## Rehearsal suites (fault injection as CI, not ceremony)

Reusing the old bridge's proven pattern (its rehearsal tests found real bugs — byte-order errors, stale-approval races — that unit tests missed):

1. **Custody-domain compromise rehearsal**: simulate one, then two-of-three, custody domains compromised; verify the one-domain case produces no unauthorized action, and the threshold-met case is contained by rate/volume limits until pause.
2. **Attestation-key rotation rehearsal**: full timelocked rotation against a real program deployment, verify old keys stop working post-rotation and no in-flight settlement is lost.
3. **Goldcoin vault sweep rehearsal**: reused near-verbatim from old bridge's `rehearsal_compromise.rs` pattern, repointed at internal custody — sweep to fresh vault, verify a stale-view signer refuses to approve a superseded sweep commitment.
4. **Multi-transfer stress rehearsal**: sustained concurrent bidirectional traffic against real regtest + localnet, verify no accounting drift and no missed reconciliation breach over an extended run (hours, not seconds — chain-timing-sensitive bugs often only appear under real block-interval conditions).

## Real-node acceptance criteria (production-readiness gate)

Per the brief and consistent with the old bridge's own launch-readiness discipline: the implementation must demonstrate, against **real Goldcoin v0.17.0-beta1 binaries in regtest** and a **local Solana validator**, using **real reserve token accounts** (not mocks):

- Multiple consecutive transfers, both directions, with exact 1:1 accounting maintained throughout.
- Full restart-and-recovery at every state-machine state.
- Fault injection: every "real-node, fault injection" row above, passing without manual database editing or manual state repair — the old bridge's own bar ("no manual database editing... required for a successful normal transfer") is inherited here as a hard requirement, and extended to hold under the fault-injection scenarios too, not just the happy path.
- No test double standing in for a real HSM/KMS in the final acceptance run — if the ratified trust model uses HSM/KMS-backed keys, acceptance testing should exercise the real signing infrastructure (or a faithful hardware/cloud equivalent), not a software stub, at least once before any production-funds decision.

## Out of scope for this repo's test suite

Load/scale testing beyond what's needed to validate correctness under realistic concurrency; formal verification of the Solana program (worth considering as a separate, later engagement, not a gate on initial implementation); external security audit (a distinct, separately-scoped activity — see [12-management-decisions.md](12-management-decisions.md) — this test plan is not a substitute for one).
