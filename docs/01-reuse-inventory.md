# Reuse / Remove / Replace / New Inventory

Source: read-only inspection of `/home/reaper/work/glc-solana-bridge` (old bridge — federated, mint/burn, 31 ADRs, 883 tests, never launched; custody decisions #1 "federation composition," #5 "upgrade authority," #7 "pause quorum," #8 "proof-of-reserves cadence" were left open at end of life).

Legend: **A**=Reuse unchanged · **B**=Reuse with modification · **C**=Remove · **D**=Replace · **E**=New component required.

## Goldcoin-side infrastructure

| Component | Old repo location | Class | Notes |
|---|---|---|---|
| Goldcoin JSON-RPC client | `relayer/src/glc/rpc.rs` | **A** | Hand-rolled, chain-mechanics only (no mint/burn coupling). Verified against real Goldcoin v0.17.0-beta1 quirks: boolean-only `getblock` verbosity, mandatory `-txindex=1`, non-Bitcoin RPC ports, `-27`/`-25` broadcast-code normalization, no PSBT/`combinerawtransaction`. These facts came from real-node verification and should not be re-derived. |
| Goldcoin indexer (block walk, reorg detection) | `relayer/src/glc/indexer.rs` | **B** | Reorg-walk-until-agreement-or-halt, forward block scan, vault-output matching by exact `scriptPubKey` (never address string) are chain-mechanics, reusable as-is. The `ReadyForSignature` step currently emits a **mint-claim artifact**; that emission is replaced with a reserve-settlement-authorization record. |
| Deposit candidate extraction (`deposit.rs`) | `relayer/src/glc/deposit.rs` | **A** | Pure, no I/O: OP_RETURN recipient-binding extraction, atomic-unit conversion, vault-output matching. Directly reusable. |
| Goldcoin-side transaction construction: address codec, coin selection, fee sizing | `relayer/src/withdrawal/address.rs`, `coin.rs` | **B** | Base58check/P2PKH codec and coin-selection algorithm reusable unchanged. The multisig-input fee-sizing function needs a same-shape single-key or M-of-N-vault variant depending on the [trust model decision](12-management-decisions.md). |
| Vault UTXO tracking / reservation (in-DB, since `lockunspent` doesn't survive restart) | `relayer/src/glc/withdrawal_db.rs` (`vault_utxos` table) | **A** | Reservation-in-DB is required regardless of authorization model; reuse unchanged. |
| Goldcoin-side payout builder + verification (`verify_payout_tx`) | `relayer/src/withdrawal/builder.rs`, `vault.rs` | **B** | Transaction construction and pre-broadcast conservation checks (output count/amounts/destinations/change) are model-agnostic — the same checks a reserve release needs. Reused with adjusted signing-authority wiring. |
| Multisig vault construction (P2SH M-of-N) | `relayer/src/withdrawal/multisig.rs`, ADR-0015/0017 | **B** | If the [recommended trust model](02-trust-model.md) is accepted, the Goldcoin reserve vault is itself an M-of-N P2SH multisig — reuse this code near-verbatim. Only the *meaning* of the M signers changes (internal custody domains, not federation members). |
| Sweep (drain vault to fresh address) | `relayer/src/withdrawal/sweep.rs` | **B** | Plan/verify transaction-construction logic reusable; the M-of-N *operator-approval* authorization model must be re-pointed at whatever custody model is chosen. |
| Multi-relayer work assignment / adoption (`deposit_id mod N`, adopt-after-verification) | `relayer/src/withdrawal/assignment.rs`, `adoption.rs`, ADR-0019/0031 | **C**, unless multiple bridge-service instances are run for HA (then **B**) | Exists to reconcile *independent* operator databases under federation. A single-operator design with one active orchestrator (+ warm standby) doesn't need leaderless assignment; if HA requires multiple active writers later, revisit as a Phase 2 concern — do not build day one. |
| Executor state machine / four-layer double-pay defense | `relayer/src/withdrawal/executor.rs`, ADR-0013 | **B** | The tick → reconcile-first → reload-and-recompute → build → submit → confirm orchestration pattern is exactly what a reserve-release pipeline needs on both legs. Concrete steps (claim-PDA check, mint instruction) are replaced. |

## Solana-side infrastructure

| Component | Old repo location | Class | Notes |
|---|---|---|---|
| Anchor program skeleton (state/errors/events/validation/verification module split) | `programs/glc-bridge/src/*.rs` | **A** | Good structural convention, orthogonal to mint vs. reserve. |
| PDA design pattern: singleton config, per-item seeded records, bump storage, reserved-byte padding for future fields | `state.rs`, ADR-0006/0007/0008 | **A** | Directly portable: reserve program needs an analogous config PDA, a claim/replay PDA per settled deposit, and a payout-obligation PDA per Solana→Goldcoin request. |
| `DepositClaim` PDA (seed = `txid‖vout`, existence-as-replay-guard) | `state.rs`, ADR-0002/0003 | **A** | This is the single most valuable reusable primitive. Applies verbatim to "one confirmed Goldcoin deposit authorizes at most one Solana reserve release." |
| `mint_wrapped` instruction | `instructions/mint_wrapped.rs` | **D** | Replaced by a `release_from_reserve` instruction: verify attestation → check claim PDA doesn't exist → check reserve-vault balance ≥ amount → SPL `transfer_checked` from PDA-owned reserve ATA → create claim PDA. Same shape, no `MintTo` CPI. |
| `burn_wrapped` / `complete_withdrawal` | `instructions/burn.rs`, `complete_withdrawal.rs`, ADR-0006/0018 | **D** | Replaced by `deposit_to_reserve` (user SPL-transfers existing GLC into the reserve-owned ATA; program atomically records a `WithdrawalObligation` PDA, same pattern as the old `WithdrawalRequest`) and a `record_goldcoin_completion` instruction mirroring ADR-0018's rationale (on-chain status must not depend solely on the bridge's DB). |
| ed25519-precompile multi-sig proof verification (`verification.rs`) | `verification.rs`, ADR-0010 | **D** | Replaced by verification of a **small internal threshold** (e.g. 2-of-3) attestation signature set, reusing the same precompile-introspection mechanics (still valuable regardless of who the signers are) but against a much smaller, non-rotating-by-governance-vote key set. See [02-trust-model.md](02-trust-model.md). |
| `ValidatorSet` epoch PDA + rotation governance | `state.rs`, `governance.rs`, ADR-0007/0014§7 | **C** | Exists to let a federation replace itself. A single-operator model with 2-of-3 internal keys rotates by direct admin action (with timelock, kept — see below), not by validator-set governance. |
| Governance pattern: propose→timelock→permissionless-execute singleton pending-action PDA; asymmetric fast-pause/slow-raise authority | `instructions/governance.rs`, ADR-0014§7 | **A** (pattern) | Timelocked, singleton-pending-action governance and asymmetric pause/raise authority (fast to reduce exposure, slow+2-of-3 to increase it) is reused for: directional pause, per-transfer/rolling-volume limit changes, protected-minimum changes, attestation-key rotation. |
| Two-step admin handover (`transfer_admin`/`accept_admin`) | `instructions/admin.rs` | **A** | Reuse unchanged. |
| Supply-cap check pattern (`checked_add` before mint, never after) | `mint_wrapped.rs` | **B** | Directly analogous check needed before reserve release: `checked_sub` against live reserve-ATA balance before transfer, never a cached number. |
| Wrapped-mint creation + Metaplex token metadata | `instructions/create_mint.rs`, `token_metadata.rs`, ADR-0009/0028 | **C** | No wrapped token exists in this design — the existing Solana GLC SPL mint is used directly. There is nothing for the bridge program to mint or attach metadata to. |
| Solana RPC/confirm/epoch/instruction-encoding infra | `relayer/src/solana/*.rs` | **B** | Bounded-confirmation logic and hand-encoded (no anchor-client) instruction builders are reusable; account layouts/instruction discriminators must be regenerated against the new program. |

## Federation / signer infrastructure

| Component | Old repo location | Class | Notes |
|---|---|---|---|
| `federation.proto`, `p2p/` transport (aggregation, mTLS peer identity, gossip collector) | `relayer/proto/federation.proto`, `relayer/src/p2p/*.rs` | **C** | This entire subsystem exists to let independent organizations jointly authorize actions. Not needed for a non-federated design. If the recommended model's internal threshold signers run as separate processes, a much smaller point-to-point RPC (no peer discovery, no rotating validator set) replaces it — new, minimal component, not this one. |
| ed25519 aggregate-signature builder (`signer/aggregate.rs`) | — | **B** | The instruction-building mechanics (assembling an ed25519-precompile instruction from N signatures) is reusable for the smaller internal threshold scheme; the "count unique threshold signers against on-chain `ValidatorSet`" logic is replaced by "count against a small fixed internal key set." |
| `VaultSignerMap` / federation payout collector (Goldcoin side) | `relayer/src/withdrawal/federation.rs` | **C** | Replaced by whatever custody model is chosen (see trust model doc) — likely a much simpler "ask our own N HSM-backed signer processes" collector without cross-organization identity binding. |
| Rate limiting (per-peer token bucket) | `p2p/ratelimit.rs` | **A** (pattern) | Generic defensive pattern (protect a signer from being starved by one caller before doing expensive re-derivation) — reused even in an internal-threshold design where "peers" become the operator's own signer processes. |
| "Never trust the requester, independently re-derive" policy discipline | `p2p/policy.rs` | **A** (principle) | This is the single most important idea to carry forward from federation, deliberately decoupled from federation itself. Each internal attestation signer independently re-derives the claim from its own chain read before signing, rather than trusting the orchestrator's claim. See [02-trust-model.md](02-trust-model.md). |
| Staged out-of-band approval for non-derivable actions (governance, sweep) | `p2p/governance_view.rs`, `sweep_view.rs`, ADR-0021 | **A** (pattern) | Directly transferable to any operator-driven action with no on-chain fact to derive (e.g., manual reserve top-up/rebalance approval). |
| Signature-grant audit logging | `p2p/audit_log.rs`, ADR-0026 | **A** | Event-shape (identity + action type + notable/routine split, never logging key material) reusable regardless of authorization model. |
| Multi-relayer leaderless assignment/adoption (deposits & withdrawals) | ADR-0019/0031 | **C** | Federation-specific coordination problem; doesn't exist in a single-operator design (see note in Goldcoin-side table above re: HA). |
| Compromise/rotation rehearsal test suites | `relayer/tests/rehearsal_*.rs` | **B** (pattern, not content) | "Rehearsal as automated CI, not ceremony" is a proven-valuable discipline (it caught real bugs: byte-order errors, stale-approval races). The *scenarios* rehearsed must be rewritten around the new custody model, but the practice of encoding them as tests against real regtest/local-validator infra is reused. |

## Database & ledger

| Component | Old repo location | Class | Notes |
|---|---|---|---|
| `indexed_blocks`, `chain_state`, `reorg_events` tables | `relayer/src/glc/db.rs` | **A** | Chain-tracking tables, model-agnostic. |
| `deposit_candidates` + `deposit_state_log` (append-only audit trail per state transition) | `db.rs` | **B** | State machine columns generic; drop the `claim_artifacts` table (mint-claim message) and replace with a settlement-authorization record. The append-only state-log pattern is reused unchanged — see [06-schema.md](06-schema.md). |
| `withdrawal_requests`, `withdrawal_payout_inputs` (UNIQUE(txid,vout) as structural double-spend guard) | `withdrawal_db.rs` | **A** | Reuse unchanged; this is exactly the reservation ledger a reserve model needs. |
| `withdrawal_payouts` | `withdrawal_db.rs` | **B** | Drop `quorum_indices`/multisig-federation columns unless the multisig-vault trust model is adopted, in which case keep them (renamed) for the internal-signer quorum. |
| `vault_utxos` (DB-persisted UTXO reservation, since `lockunspent` is in-memory only) | `withdrawal_db.rs` | **A** | Reuse unchanged. |
| `withdrawal_quorum_history`, `reconciled_payouts` | `withdrawal_db.rs` | **C** | Federation-specific (multi-operator payout adoption). Drop unless HA multi-writer is built later. |

## Reconciliation, monitoring, tooling

| Component | Old repo location | Class | Notes |
|---|---|---|---|
| Solvency check (`wrapped_supply ≤ confirmed_deposits − completed_payouts`, zero-slack) | `relayer/src/ops/solvency.rs`, ADR-0020 | **B** | The *shape* — continuously compare a DB-derived running total against a live chain read, zero tolerance for unexplained slack — is exactly what reserve reconciliation needs. Formula rewritten to `reserve_balance(chain) == protected_minimum + reserved_liquidity + available_capacity` per direction (see [05-reserve-accounting.md](05-reserve-accounting.md)). |
| Offline integrity auditor (recompute-and-diff every stored commitment against a backup) | `relayer/src/ops/audit.rs`, `glc-audit` CLI | **B** | Pattern reusable unchanged; the two recompute functions (`check_claim`, `check_payout`) need rewriting against the new commitment format. |
| Health (`/health`, 503-on-breach) + Metrics (`/metrics`, Prometheus text) | `relayer/src/ops/health.rs`, `metrics.rs` | **A** | Hand-rolled registry/encoder and health/metrics separation reusable as-is. |
| Indexer status tracking (halt state, deepest reorg, staleness) | `relayer/src/ops/indexer_status.rs` | **A** | Reuse unchanged, chain-agnostic. |
| Preflight checks | `relayer/src/ops/preflight.rs` (governance-timelock specific), `withdraw_preflight.rs` (paused/min-amount/balance/address-validity) | **B** / **A** | `preflight.rs`'s federation-timelock checks removed; `withdraw_preflight.rs`'s generic pre-payout checks reused unchanged, extended with reserve-capacity checks. |
| CLI tooling shape (`glc-admin`, `glc-audit`, `glc-wallet`; mandatory `--note` audit trail; status/recovery/audit/bootstrap grouping) | `relayer/src/bin/*.rs` | **B** | Shape and audit discipline reused; governance/rotation/quorum-reassignment subcommands removed, replaced with reserve-specific admin ops (set limits, pause direction, approve rebalance). |
| Docker/regtest facts (Goldcoin regtest ports, `-txindex=1`, decimal precision, OP_RETURN size) | `docker/README.md` | **A** (as documentation) | Empirically verified against a real node; carry forward rather than re-derive. No executable compose file exists yet in the old repo — build fresh. |
| `tests/bridge-e2e.ts` | — | **E** | Placeholder only in the old repo (19 lines, no harness). New component required either way. |
| Orchestrator event loop shape | `relayer/src/orchestrator.rs` | **B** | Tick → reconcile-first-always → reload-and-recompute-before-acting → act → mark-state pattern reused; every concrete step (claim-PDA check, mint submission, validator-threshold check) rewritten around reserve release. |

## Summary counts

- **A (reuse unchanged):** ~20 components — mostly chain RPC/indexer mechanics, DB tables, transaction-construction primitives, ops/health/metrics scaffolding, audit/governance *patterns*.
- **B (reuse with modification):** ~20 components — same code, different authorization wiring or renamed formula/commitment format.
- **C (remove):** federation transport, validator-set epoch PDA, multi-relayer assignment/adoption, wrapped-mint/metadata creation.
- **D (replace):** mint/burn instructions → reserve-transfer instructions; ed25519 federation-proof verification → small internal-threshold attestation verification.
- **E (new):** reserve-capacity/reservation ledger and its concurrency control (no old-repo analog — the old bridge had no concept of "promised but not yet settled" liquidity, since minting was uncapped-per-transfer by construction), directional/global pause plumbing wired to reserve state, executable local dev/test harness (docker-compose + Solana localnet + Goldcoin regtest), reconciliation-triggered auto-pause logic.
