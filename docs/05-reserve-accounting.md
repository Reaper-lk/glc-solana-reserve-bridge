# Reserve Accounting Model

> **Updated by later phases**: this document predates both the Token-2022
> canonical mint's 6-decimal precision (Goldcoin is 8 — docs/18-token-2022-
> support.md) and the 1% bridge fee (docs/20-bridge-fee.md). The
> *shape* of the model below (reserved/pending/settled/available, atomic
> integer arithmetic, no float, row-level-locked reservation) is unchanged
> and still accurate. Two specific claims below are NOT accurate anymore
> and are corrected inline where they appear: (1) `reserved_liquidity`/
> `pending_obligations`/`settled_liquidity` are tracked in each reserve's
> own NATIVE unit and represent the NET amount after the fee, not the
> gross user-declared amount; (2) the "exactness invariant" at the bottom
> is no longer 1:1 gross-for-gross — see docs/20-bridge-fee.md for the
> current, authoritative accounting model and canonical unit design.

Applied identically to both reserves (Goldcoin native-GLC reserve, Solana GLC reserve) — each has its own independent instance of every quantity below, since capacity in one direction depends only on the *destination* reserve.

## Quantities

| Quantity | Definition | Source of truth |
|---|---|---|
| `total_reserve_balance` | Actual on-chain/on-vault balance right now | Live chain read (Solana: PDA-owned ATA balance; Goldcoin: sum of `vault_utxos` confirmed at required depth, cross-checked against a live `listunspent`/`scantxoutset`-equivalent read) |
| `protected_minimum` | Configurable floor below which funds are never available for payout, regardless of demand | Config (per [12-management-decisions.md](12-management-decisions.md)) |
| `reserved_liquidity` | Sum of destination amounts committed to all **non-terminal** requests, from `LiquidityReserved` through `DestinationSubmitted` inclusive | Ledger (derived, but persisted as a running counter — see below) |
| `pending_obligations` | Subset of `reserved_liquidity` for requests that have passed `SourceFinalized` — i.e. the source deposit is irreversible and the bridge is *committed* to paying, as opposed to liquidity merely held against a reservation that could still expire | Ledger (derived) |
| `settled_liquidity` | Cumulative amount paid out, all-time (or rolling window, see below) — an accounting counter, not part of the capacity formula | Ledger, reconciled against chain |
| `available_capacity` | What can safely be promised to a new request right now | Computed |

```
available_capacity =
    total_reserve_balance
  - protected_minimum
  - reserved_liquidity
```

`pending_obligations` is a subset of `reserved_liquidity`, not separately subtracted — it exists as a distinct tracked quantity because it changes what "expiry" means (see [04-state-machines.md](04-state-machines.md)): liquidity reserved-but-not-yet-`SourceFinalized` can safely expire and be released; liquidity in `pending_obligations` cannot — the bridge is already on the hook. Monitoring should alert separately on `pending_obligations` approaching `total_reserve_balance - protected_minimum`, since that specific condition (not `reserved_liquidity` broadly) indicates real, committed exposure the bridge cannot walk back from.

## Reservation lifecycle and capacity check

Never accept a transfer that cannot be fulfilled: the capacity check and the reservation write must be atomic, or two concurrent requests can both observe sufficient `available_capacity` and both reserve against the same liquidity.

**Mechanism:** a single-row-per-direction `reserve_ledger` table (see [06-schema.md](06-schema.md)) holding the running counters (`reserved_liquidity`, `pending_obligations`, cached `total_reserve_balance` refreshed by the reconciliation job). Every reservation is:

Because `total_reserve_balance` is only a cache, its freshness relative to admission matters: for `GoldcoinReserve`, `Orchestrator::tick` runs the reconciliation pass once, early, before `solana_indexer.tick()` folds any newly observed SolToGlc obligation into a reservation — not only at the end of the tick as before — so a burst of obligations observed in the same tick (or across ticks while reconciliation itself was skipping) is admitted against a balance no staler than that tick's own start, rather than whatever the previous tick's end-of-tick pass happened to leave cached. This reuses the exact same reconciliation check (same formula, same auto-pause trigger) rather than a bare balance write, so a genuine pre-existing breach is still caught and paused, never silently absorbed into a refreshed baseline. If admitting the next obligation in the burst would still cross the protected minimum against that freshened balance, `fold_sol_deposit` parks only that one request (`ManualReview`) rather than admitting it and letting the aggregate breach trigger an auto-pause of the whole direction after the fact.

```sql
BEGIN;
SELECT reserved_liquidity, protected_minimum, total_reserve_balance
  FROM reserve_ledger WHERE direction = ? FOR UPDATE;
-- application computes available_capacity; if amount > available_capacity, ROLLBACK and reject
UPDATE reserve_ledger SET reserved_liquidity = reserved_liquidity + :amount WHERE direction = ?;
INSERT INTO bridge_requests (..., state = 'LiquidityReserved', ...);
COMMIT;
```

`FOR UPDATE` (row-level lock on the single ledger row per direction) serializes concurrent reservations for the same direction without serializing the whole table — the two directions' ledgers are independent rows and never contend with each other. This is the same discipline as the old bridge's DB-persisted UTXO reservation (`vault_utxos`, needed because `lockunspent` doesn't survive a restart) applied one level up, to bridge-request liquidity rather than individual UTXOs.

**Expiry:** a background sweeper scans for `LiquidityReserved`/`AwaitingDeposit` requests past `T_reserve_expiry`, and for each, atomically (same transaction pattern) decrements `reserved_liquidity` and marks the request `Expired`. Sweeper cadence and `T_reserve_expiry` are configuration (see [12-management-decisions.md](12-management-decisions.md)) — no default is asserted here, consistent with the old bridge's explicit no-hardcoded-confirmation-depth policy applied to this new parameter class.

**Settlement:** on reaching `Settled`, the same transaction pattern moves the amount from `reserved_liquidity` to `settled_liquidity` (and, if applicable, out of `pending_obligations`).

## Directional capacity vs. reserve thresholds

Per management's stated sizing principle ("reserves sized to cover the largest expected net outflow between operational rebalances"), the ledger additionally tracks threshold bands, all configurable, no defaults asserted:

| Band | Meaning | Typical response |
|---|---|---|
| `target_reserve` | Desired steady-state balance after a rebalance | Informational; rebalancing aims to restore this |
| `warning_reserve` | `total_reserve_balance` at or below this triggers an alert | Alert only; direction stays active |
| `critical_reserve` | `total_reserve_balance` at or below this triggers automatic directional pause | See [09-runbook.md](09-runbook.md) |
| `protected_minimum` | Hard floor, never available regardless of band | Requests that would breach it are rejected at the capacity check, not paused after the fact |

`critical_reserve` must be set strictly above `protected_minimum` (config-time validation), so the directional pause engages before the protected minimum is ever at risk of being approached by a burst of concurrent reservations racing the pause decision.

## Rate limits

Layered, all configurable:

- **Per-transfer limit**: maximum amount per single bridge request, per direction.
- **Rolling volume limit**: maximum cumulative amount per direction over a configurable rolling window (reuses the `RollingVolumeWindow` on-chain account on the Solana leg for the Goldcoin→Solana direction, since that release is program-enforced; enforced in the ledger/orchestrator for the Solana→Goldcoin direction, since Goldcoin has no program layer to enforce it — see [03-architecture.md](03-architecture.md) asymmetry note).
- **Directional pause**: independent on/off per direction, operator- or reconciliation-triggered.
- **Emergency global pause**: both directions, highest-priority gate, checked before any other.

## Rebalancing accounting

Operational rebalancing (moving reserve funds between the two reserves, or topping up from a treasury) must be **structurally distinguishable from user settlements**, not just labeled differently in a UI:

- Rebalance operations use a dedicated instruction/transaction path (`rebalance_deposit`/`rebalance_withdraw`, [03-architecture.md](03-architecture.md)) that never touches `reserved_liquidity`, `pending_obligations`, or the `bridge_requests` table — they only move `total_reserve_balance`.
- Every rebalance is tagged with an operator identity and mandatory note (reused audit discipline) and recorded in a separate `rebalance_events` ledger table ([06-schema.md](06-schema.md)), never appended to `bridge_requests`/settlement history, so a reconciliation job or an auditor scanning settlement records can never mistake a rebalance for a user bridge transfer, and vice versa.
- Rebalancing requires the same staged-out-of-band-approval pattern reused from the old bridge's governance/sweep views (ADR-0021) — it moves real reserve funds and should not be a single-click, single-key action under the recommended trust model.

## Exactness invariant

**Superseded by docs/20-bridge-fee.md** — kept here for history. For all
time: `settled_liquidity` on one reserve's outbound ledger must equal the
sum of confirmed inbound deposits on the *other* chain that authorized
those settlements, exactly — but NOT 1:1 gross-for-gross since the 1%
bridge fee round: it equals the sum of those deposits' NET amounts (gross
minus the 1% fee), with no accumulated rounding beyond the documented
floor-fee/fail-closed-conversion policy. Goldcoin uses 8-decimal atomic
units; the canonical Solana GLC mint uses 6 (docs/18-token-2022-
support.md) — the two chains are NOT the same atomic unit, and all
gross/fee/net bookkeeping uses one canonical accounting denomination
(docs/20-bridge-fee.md) specifically to make comparing them directly
impossible by construction. This is checked continuously by reconciliation
([03-architecture.md](03-architecture.md), [09-runbook.md](09-runbook.md))
and is the reserve-model's analog of the old bridge's zero-slack solvency
invariant (`wrapped_supply ≤ confirmed_deposits − completed_payouts`), now
evaluated against net (not gross) settled amounts.
