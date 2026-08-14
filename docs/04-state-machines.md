# State Machines

One state machine shape, parameterized by direction (Goldcoin→Solana, Solana→Goldcoin) rather than two independent designs, since the divergence is localized to a small number of steps (see notes inline). Chosen names deliberately differ from the illustrative list in the brief where the actual mechanics warranted it — noted per state.

## States

| State | Meaning | Transition trigger |
|---|---|---|
| `Created` | Request registered; no capacity reserved yet | User/API call |
| `LiquidityReserved` | Destination `available_capacity` atomically decremented; reservation has expiry `T_reserve_expiry` | **Automatic**, on successful capacity check |
| `AwaitingDeposit` | User has deposit instructions; nothing observed yet | Automatic, immediately follows reservation |
| `DepositObserved` | Indexer has seen a matching, correctly-bound source transaction (mempool or 0-conf) | **Chain-derived, automatic** |
| `Confirming` | Accumulating confirmations toward configured finality depth | Chain-derived, automatic |
| `SourceFinalized` | Finality depth reached; source deposit treated as an irreversible fact | Chain-derived, automatic, **irreversible by policy** (see [10-threat-model.md](10-threat-model.md) on depth selection) |
| `SettlementAuthorized` | Threshold of independent signers has attested to this exact claim; replay guard checked (Solana leg: claim PDA absent; Goldcoin leg: DB UNIQUE constraint written) | Automatic once threshold reached, but **gated on independent re-derivation succeeding at each signer** — not purely mechanical |
| `DestinationSubmitted` | Payout transaction built, signed, and broadcast to the destination chain | Automatic, retryable, **idempotent** (rebuild from the `SettlementAuthorized` record is deterministic) |
| `DestinationConfirmed` | Payout observed at destination chain's required confirmation depth | Chain-derived, automatic |
| `Settled` | Terminal success. `reserved_liquidity` converted to `settled_liquidity`; reconciliation entry written | Automatic |

## Terminal / error states

| State | Meaning | Reached from | Recovery |
|---|---|---|---|
| `Expired` | Reservation TTL elapsed before deposit observed | `LiquidityReserved`, `AwaitingDeposit` | Automatic capacity release. **Retryable** — user may create a new request. See note on late deposits below. |
| `Cancelled` | User or operator explicitly cancels before deposit observed | `LiquidityReserved`, `AwaitingDeposit` | Automatic capacity release. Operator-controlled or user-controlled. |
| `InsufficientLiquidity` | Request rejected at creation; capacity never reserved | `Created` (rejection, not really a transition of an accepted request — kept as a terminal audit record) | N/A — request never became live |
| `Reorged` | A pre-finality deposit's block was reorganized out | `DepositObserved`, `Confirming` only — **never** from `SourceFinalized` or later, by construction of the finality-depth policy | Automatic, chain-derived. Retryable: if the transaction still exists (different block), return to `Confirming`; otherwise return to `AwaitingDeposit`. |
| `InsufficientReserveAtSettlement` | Capacity was reserved at request time, but the live reserve balance at settlement time doesn't actually cover it (accounting bug, unexpected manual withdrawal, reconciliation mismatch) | `SourceFinalized`, `SettlementAuthorized` | **Fail closed — do not pay out.** Goes to `ManualReview`. This is distinct from `Expired`: the user's source-chain deposit is real and irreversible; forfeiting it is not acceptable, so this is a paged incident, not a routine terminal state. |
| `DestinationSubmissionFailed` | Broadcast/build failed (RPC error, fee issue, node rejection) | `SettlementAuthorized`, `DestinationSubmitted` | Automatic retry with backoff, idempotent rebuild from the `SettlementAuthorized` record. Exhausted retries → `ManualReview`. |
| `ManualReview` | Safe-halt catch-all for anything that doesn't fit automatic recovery (integrity mismatch, reconciliation breach, repeated failure, `InsufficientReserveAtSettlement`) | Any non-terminal state | **Operator-controlled exit only**, requires a non-empty note (reused audit discipline from `glc-admin`). Never auto-retried. |
| `Failed` | Permanently unpayable (e.g. malformed destination address that passed initial validation but can't be paid) | `SettlementAuthorized`, `DestinationSubmitted` | Terminal. Requires an explicit refund/compensation process — see open item below; the bridge cannot auto-refund cross-chain. |

`Paused` is **not** a per-request state. It is a system-level gate (per direction, or global) checked at every automatic transition point (`LiquidityReserved`, `SettlementAuthorized`, `DestinationSubmitted`). A request mid-flight when a pause engages simply stops advancing (holds at its current state) until unpaused or explicitly moved to `ManualReview` by an operator if the pause reason implicates that specific request.

## Transition properties

| Transition | Automatic | Chain-derived | Irreversible | Retryable | Idempotent | Operator-controlled |
|---|:-:|:-:|:-:|:-:|:-:|:-:|
| `Created → LiquidityReserved` | ✓ | | | | ✓ (capacity check is a pure function of current ledger state) | |
| `LiquidityReserved → AwaitingDeposit` | ✓ | | | | ✓ | |
| `→ Expired` (from reservation states) | ✓ | | | ✓ (new request) | ✓ | |
| `→ Cancelled` | | | | | ✓ | ✓ (or user-initiated) |
| `AwaitingDeposit → DepositObserved` | ✓ | ✓ | | | | |
| `DepositObserved → Confirming` | ✓ | ✓ | | | | |
| `Confirming → SourceFinalized` | ✓ | ✓ | ✓ | | | |
| `Confirming/DepositObserved → Reorged` | ✓ | ✓ | | ✓ | | |
| `SourceFinalized → SettlementAuthorized` | ✓ (given signer availability) | | ✓ | ✓ (retry signer collection) | ✓ (same claim, same result) | |
| `→ InsufficientReserveAtSettlement → ManualReview` | ✓ (detection) | | | | | ✓ (exit) |
| `SettlementAuthorized → DestinationSubmitted` | ✓ | | | ✓ | ✓ (rebuild deterministic) | |
| `→ DestinationSubmissionFailed` | ✓ | | | ✓ | ✓ | ✓ (after retry exhaustion) |
| `DestinationSubmitted → DestinationConfirmed` | ✓ | ✓ | | | | |
| `DestinationConfirmed → Settled` | ✓ | | ✓ | | ✓ | |
| `any → ManualReview` | ✓ (detection) | | | | | ✓ (exit only) |
| `ManualReview → (resumed state or Failed)` | | | depends | | | ✓ |

## Direction-specific notes

- **Replay-guard mechanism differs by direction**, not by state name: on the Goldcoin→Solana leg, `SettlementAuthorized → DestinationSubmitted` is backstopped by an on-chain claim PDA (cryptographically enforced impossibility of double-authorization). On the Solana→Goldcoin leg, the equivalent is a database UNIQUE constraint plus multisig-signer independent re-verification (operationally enforced, not cryptographically impossible). Both use the same state names deliberately, so this asymmetry doesn't leak into a proliferation of direction-specific states — it's documented once, here and in [10-threat-model.md](10-threat-model.md), rather than re-derived from the state names.
- **`DestinationConfirmed` finality depth is direction-specific**: Solana-side payouts confirm at `finalized` commitment (fast, seconds); Goldcoin-side payouts confirm at a configured block-depth (slower, minutes-to-hours depending on chosen depth — see [12-management-decisions.md](12-management-decisions.md) for the parameter, not hardcoded here per the old bridge's explicit no-default policy on confirmation depths).

## Open design item: late deposits after expiry

A user's source-chain deposit is irreversible once broadcast; a bridge-side reservation `Expired` for operational reasons (capacity-management TTL) does not un-happen the user's payment. If a deposit is observed against an already-`Expired` request:

- If capacity is still available, the orchestrator should attempt to **auto-recreate a fresh reservation** and continue the flow normally from `DepositObserved`.
- If capacity is no longer available, this must route to `ManualReview` for a compensating action (refund on the source chain, or honor the settlement as an explicit above-capacity exception with operator sign-off) — **never silently drop the request**, since real funds were received. This case is deliberately called out because it's easy to design a reservation system that is correct for the bridge's own ledger and wrong for the user who already paid; see [11-testing-plan.md](11-testing-plan.md) item "stale reservation with late deposit."
