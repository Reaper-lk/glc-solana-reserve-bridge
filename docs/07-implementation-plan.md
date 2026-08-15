# Module-by-Module Implementation Plan and Phased PR Sequence

Assumes the [reuse inventory](01-reuse-inventory.md) classifications and the [recommended trust model](02-trust-model.md), pending ratification per [12-management-decisions.md](12-management-decisions.md). Nothing in this plan should be executed until that ratification lands — it is written now so implementation can start immediately once it does.

## Workspace layout

Mirrors the old bridge's two-workspace split (ADR-0001: on-chain program isolated from off-chain dependencies), carried forward:

```
programs/glc-reserve-bridge/   -- Anchor program (Solana leg)
service/                        -- off-chain bridge service (Rust)
  src/
    goldcoin/                   -- RPC client, indexer, deposit extraction (reused)
    solana/                     -- RPC/confirm/instruction-encoding (reused w/ modification)
    ledger/                     -- reserve accounting, reservation, state machine (new)
    orchestrator/                -- tick loop, settlement pipeline (reused shape, new steps)
    signing/                    -- attestation + vault signer clients (new, small)
    reconciliation/              -- solvency/reconciliation monitor (reused w/ modification)
    ops/                         -- health, metrics, preflight, audit (reused)
    bin/                         -- glc-admin, glc-audit CLIs (reused shape)
shared/                          -- cross-workspace types, claim/commitment encoding (reused w/ modification)
tests/                           -- integration + e2e, real-node acceptance (new harness)
docker/                          -- local dev: Goldcoin regtest + Solana localnet (new, facts reused)
```

## Module-by-module

| Module | Action | Depends on |
|---|---|---|
| `programs/glc-reserve-bridge` state/errors/events/validation | Port skeleton from old `programs/glc-bridge`, strip mint/wrapped-mint/validator-set fields, add `ReserveVault`, `RollingVolumeWindow` | Trust model ratified (attestation key-set shape) |
| `release_from_reserve`, `deposit_to_reserve`, `record_goldcoin_completion` instructions | New instructions per [03-architecture.md](03-architecture.md); replay guard reused from `DepositClaim` pattern | Program skeleton |
| Governance instructions (pause, limits, attestation-key rotation, two-step admin) | Port from old `governance.rs`/`admin.rs`, repoint at new config fields | Program skeleton |
| `service/goldcoin/{rpc,indexer,deposit}` | Port near-verbatim from old repo | — |
| `service/solana/*` | Port `config`/`confirm`/`rpc` from old repo; regenerate instruction encoders against the new program's IDL | Program instructions defined |
| `service/ledger` | New: reservation/state-machine implementation of [04](04-state-machines.md) and [05](05-reserve-accounting.md) | Schema ([06-schema.md](06-schema.md)) |
| `service/orchestrator` | Port tick-loop shape from old `orchestrator.rs`; rewrite concrete steps around `release_from_reserve`/Goldcoin payout instead of mint | Ledger, signing clients |
| `service/signing` | New, small: client for attestation signer group (2-of-3) and Goldcoin vault signers; reuses ed25519-precompile instruction-assembly mechanics from old `signer/aggregate.rs`, and the "independent re-derivation" policy shape from old `p2p/policy.rs`, but as a minimal point-to-point RPC, not the full federation transport | Trust model ratified |
| `service/reconciliation` | Port `solvency.rs`/`audit.rs` shape; rewrite formula per [05](05-reserve-accounting.md); wire to auto-pause | Ledger |
| `service/ops` | Port `health.rs`, `metrics.rs`, `indexer_status.rs`, `withdraw_preflight.rs` near-verbatim; drop federation-timelock preflight | — |
| `service/bin/glc-admin`, `glc-audit` | Port CLI shape and `--note` audit discipline; drop governance/rotation/quorum subcommands, add reserve-limit/pause/rebalance-approval subcommands | Ledger, ops |
| `shared` | Port claim/commitment encoding types, rewrite for the new attestation message format (still domain-tagged, still binds program id/direction/txid/vout/amount/recipient, no epoch field since there's no rotating validator set — attestation-key version field instead) | Trust model ratified |
| `tests/` real-node harness | New: docker-compose with Goldcoin v0.17.0-beta1 regtest (ports/flags per old bridge's verified `docker/README.md` facts) + `solana-test-validator` | — |

## Phased PR sequence

**Phase 0 — Foundations (no external dependencies, can start immediately)**
1. Workspace scaffold, CI (reuse `cargo-deny`, lint config from old repo where applicable).
2. `service/goldcoin/{rpc,indexer,deposit}` ported, unit-tested against recorded fixtures.
3. `service/solana/{config,confirm,rpc}` ported (generic parts only, no program-specific encoding yet).
4. Schema migrations for chain-tracking tables ([06-schema.md](06-schema.md) sections 1).
5. `docker/` local dev harness: Goldcoin regtest + Solana localnet, verified against real binaries.

**Phase 1 — Ledger and state machine (independent of trust model choice)**
6. `service/ledger`: reserve accounting, reservation concurrency control, expiry sweeper ([05](05-reserve-accounting.md)).
7. `bridge_requests` + state-log schema, state machine implementation ([04](04-state-machines.md)) with all automatic/chain-derived transitions except `SettlementAuthorized` (stubbed).
8. Reconciliation monitor skeleton against Goldcoin indexer + a stub Solana balance reader.

**— Gate: trust model ratified (12-management-decisions.md item 1) before Phase 2 —**

**Phase 2 — Solana program**
9. `programs/glc-reserve-bridge`: config/vault/claim/obligation accounts, `initialize`, two-step admin, `set_paused`.
10. `release_from_reserve` + `deposit_to_reserve` + `record_goldcoin_completion`, with attestation verification against the ratified key set.
11. Governance instructions (limits, rolling-volume window, attestation-key rotation, timelocked).
12. Program test suite: replay rejection, insufficient-reserve rejection, cap enforcement, pause enforcement — ported/adapted from old `programs/glc-bridge/tests/*`.

**Phase 3 — Goldcoin vault and payout construction**
13. Vault construction (P2SH M-of-N per trust model decision), ported `vault.rs`/`multisig.rs`.
14. Payout builder + `verify_payout_tx`, ported `builder.rs`.
15. `vault_utxos` reservation, coin selection, ported `coin.rs`/`address.rs`.
16. `service/signing`: Goldcoin vault-signer client, independent re-derivation policy.

**Phase 4 — Orchestrator and end-to-end settlement**
17. `service/orchestrator`: full Goldcoin→Solana pipeline wired end-to-end against regtest + localnet.
18. Full Solana→Goldcoin pipeline wired end-to-end.
19. Attestation signer group service (2-of-3), wired to both pipelines.
20. `service/reconciliation`: full formula, auto-pause wiring, `reconciliation_findings`.

**Phase 5 — Operations**
21. `service/ops`: health/metrics/preflight ported and extended for reserve state.
22. `glc-admin`/`glc-audit` CLIs: reserve-specific subcommands.
23. Runbook procedures ([09-runbook.md](09-runbook.md)) implemented as executable commands, CI-checked for doc/binary consistency (reused discipline from old bridge's `runbook_commands.rs`).

**Phase 6 — Rehearsal and acceptance**
24. Rehearsal test suites (compromise response, key rotation, reserve depletion, rebalance) as automated CI against real regtest/localnet — reused discipline, new scenarios (see [11-testing-plan.md](11-testing-plan.md)).
25. Full acceptance test plan execution against real Goldcoin v0.17.0-beta1 + local Solana validator.
26. Security review (external, scoped separately — see [12-management-decisions.md](12-management-decisions.md)).

Each phase should land as multiple small PRs, not one PR per phase; the numbering above indicates dependency order and reasonable PR-sized units, not a mandate for exactly 26 PRs.
