# Confirmed-Goldcoin-liquidity admission safety buffer

## 1. The problem

The hard reserve invariant is a solvency floor:

```
total_reserve_balance >= protected_minimum + reserved_liquidity
```

Breaching it stops payouts (`Ledger::check_invariant`, and the auto-pause
that follows). It is correct, and it is not changed by anything here.

What was wrong is where **admission** sat relative to it. `fold_sol_deposit`
admitted a new SolToGlc obligation whenever the reserve could *just* cover
it:

```
net_destination_atomic <= total_reserve_balance - protected_minimum - reserved_liquidity
```

So ordinary traffic could walk the reserve right down to the invariant and
park the next arrival at the floor. The floor was doing duty as both the
emergency stop and the routine admission gate, which meant the emergency
stop was being touched routinely.

## 2. The change

Admission now closes **substantially earlier**, leaving a deliberate
cushion of mature confirmed liquidity between routine operation and the
invariant. A new obligation is admitted only when:

```
mature_confirmed_balance >= protected_minimum
                          + reserved_liquidity
                          + incoming_request_amount
                          + ADMISSION_SAFETY_BUFFER
```

Two mechanisms, deliberately separate:

| | What it does | Where |
|---|---|---|
| **Per-request gate** | Refuses one obligation that would eat the cushion. Parks it in `ManualReview` with reason `liquidity_buffer_at_fold` — never drops it. | `fold_sol_deposit`, `resume_manual_review_sol_to_glc` |
| **Direction-wide state machine** | Closes admission entirely below the close threshold; reopens only above the higher reopen threshold. | `evaluate_liquidity_admission`, run from the reconciliation tick |

## 3. Production policy

| Parameter | Value | Atomic (8 decimals) | Config key |
|---|---|---|---|
| Admission safety buffer / automatic close threshold | **250,000 GLC** | `25000000000000` | `goldcoin.admission_safety_buffer_atomic` |
| Automatic reopen threshold | **350,000 GLC** | `35000000000000` | `goldcoin.admission_reopen_buffer_atomic` |

Both are the shipped defaults. `0` disables the buffer and restores the
previous behaviour exactly. `reopen >= close` is validated at config load
and again in `set_admission_liquidity_buffer`; an inverted pair is
refused, because a reopen threshold below the close threshold would reopen
admission while it was still closing.

## 4. Only mature confirmed liquidity counts

This is the property the whole change rests on, and it is **structural**,
not a filter someone could later relax:

- `total_reserve_balance` is written by reconciliation from
  `observed_balance`, which sums only outputs with
  `confirmations >= vault_min_confirmations`
  (`Orchestrator::tick_goldcoin_reconciliation`).
- Immature payout change and **every zero-conf recursive change
  candidate** live in `vault_utxos` rows with `state = 'Unconfirmed'` —
  `zero_conf_change_vault_utxos_with_depth` selects exactly those rows.
- Therefore neither can enter `total_reserve_balance` at all. The headroom
  the buffer reads cannot be inflated by value that is spendable-but-
  unconfirmed.

Both figures **are** reported — `immature_excluded_atomic` and
`own_unconfirmed_change_atomic` in the status snapshot — so an operator
can see that recovery is already en route. Neither is ever added to
anything.

Pinned by `immature_own_change_does_not_count_as_admission_capacity` and
`zero_conf_recursive_change_does_not_count_as_admission_capacity`.

## 5. Hysteresis, and whose closure it is

A single threshold would flap: the reserve hovers at 250,000, admission
closes, one payout confirms, admission opens, the next obligation closes
it again. So closing and reopening use different thresholds, and the
100,000 GLC band between them is dead space where nothing happens.

`admission_auto_closed` records **who** closed admission:

| Closure | `admission_auto_closed` | Automatic reopen? |
|---|---|---|
| This liquidity rule | `1` | Yes, at >= reopen threshold |
| An operator (`glc-admin close-admission`) | `0` | **Never** |

An operator who closes admission to investigate something unrelated does
not have it silently reopened underneath them when liquidity happens to
recover. Conversely, an operator closing admission while it was already
auto-closed *takes ownership* — automatic recovery stops applying.

A **manual** reopen is held to the reopen threshold too
(`check_liquidity_buffer_for_admission`, called from
`open_admission_guarded`), so a manual open cannot be used to slip
admission back on inside the band.

## 6. What does not change

- **The hard invariant is untouched.** Same formula, same auto-pause, same
  error. The buffer sits above it; it does not replace, weaken, or restate
  it.
- **Existing accepted obligations keep processing.** Closing admission
  gates *new* obligations only. Reservations are not released, requests
  are not cancelled, and the payout pipeline continues.
- **`ManualReview` recovery cannot bypass the buffer.** A resume re-admits
  real demand exactly as a fresh fold does, so it is held to the same
  rule — otherwise a backlog would drain straight through the cushion and
  back down to the invariant, which is the outcome this exists to prevent.
  The refusal is transient: the same resume succeeds once confirmed
  liquidity recovers.
- **Fail-closed throughout.** Every refusal parks or refuses; nothing is
  dropped, and no path treats "uncertain" as "admit".

## 7. Operator visibility

`glc-admin status` prints, for the Goldcoin reserve:

```
GoldcoinReserve: balance=... protected_minimum=... reserved_liquidity=... ...
  admission_buffer: confirmed_unreserved_headroom=... close_threshold=... \
    reopen_threshold=... auto_closed=... immature_excluded=... \
    own_unconfirmed_change=... reason=...
```

and, when admission is closed, one line saying **which kind** of closure it
is and what will clear it. The same fields are on `ReserveSnapshot`
(`ops::reserve_health`), so the admin API and any UI get them without a
second query.

If the buffer is unconfigured, `glc-admin status` says so explicitly
rather than staying silent — an unset buffer means admission closes only
at the hard invariant.

## 8. Schema

Schema version **17 → 18**. `apply_v18` adds three columns to
`reserve_ledger`, each `INTEGER NOT NULL DEFAULT 0`:
`admission_safety_buffer_atomic`, `admission_reopen_buffer_atomic`,
`admission_auto_closed`.

Structurally idempotent per column (the `column_exists` guard `apply_v9`
documents), additive only, and defaulting to the disabled state — so the
migration is behaviour-neutral until the daemon applies the configured
thresholds at startup. No table is rewritten and no existing column
changes meaning, so an older binary continues to read the database
correctly apart from not enforcing the buffer.
