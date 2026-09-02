# Robinhood Network — Phase 1 (scaffolding, both routes disabled)

**Status:** experimental. Both Robinhood routes are disabled and cannot be
executed. Nothing here is deployed, and the on-chain program is unchanged.

Phase 1 adds the *shape* of a third bridge chain — route identity, a
three-place fail-closed admission gate, an inert chain adapter, optional
configuration, an API representation, and tests — without implementing any
Robinhood settlement behaviour and without guessing any Robinhood chain
parameter.

---

## Route names

| Route | Meaning | Phase 1 state |
|---|---|---|
| `GlcToSol` | Goldcoin L1 → Solana | enabled (unchanged production route) |
| `SolToGlc` | Solana → Goldcoin L1 | enabled (unchanged production route) |
| `GlcToRhn` | Goldcoin L1 → Robinhood Network | **disabled** |
| `RhnToGlc` | Robinhood Network → Goldcoin L1 | **disabled** |

`L1ToRobinhood`/`RobinhoodToL1` are **not** accepted spellings anywhere —
`Route::from_str` rejects them, and a test pins that. Two spellings for one
route is how a gate eventually gets skipped.

Direct Solana↔Robinhood bridging is out of scope and is structurally
impossible: no `Route` variant names both as its endpoints, and a test
asserts none ever will.

---

## The three-place AND

`RouteGate::ensure_enabled` admits a route only when **all three** of these
independently agree. Each is evaluated on every call; none is cached.

| # | Gate | Source | Disabled when |
|---|---|---|---|
| 1 | Config | `[robinhood]` in the TOML file | section absent, flag absent, or `false` |
| 2 | Ledger | `bridge_routes` table | table absent, row absent, or `enabled = 0` |
| 3 | Adapter | `ChainAdapter::capability` for **both** legs | either leg not `Operational` |

Plus a fourth, structural check that is not a gate at all:
`Route::as_direction()` returns `None` for Robinhood routes, so even a
deployment with all three gates forced open cannot produce the
`Direction` value that every reserve, ledger and signing function requires.
`service/tests/robinhood_route_isolation.rs` proves each gate closes on its
own, and that the fourth holds when the other three are subverted.

Gate 3 is the one that cannot be flipped from outside the repository:
`RobinhoodAdapter` is a zero-sized type with no RPC client, no chain id, no
token contract, no decimals, no reserve address and no signer, and returns
`Unavailable` unconditionally. Enabling a Robinhood route requires a
reviewed code change, not an operator action or a config edit.

---

## Schema: nothing changed in Phase 1

`CURRENT_SCHEMA_VERSION` **stays at 17.** No migration ships in this phase.

This is deliberate and load-bearing. `schema::open_and_migrate` refuses to
open a database written by a newer binary (`LedgerError::SchemaTooNew`), and
it stamps its own version onto every database it opens. Shipping a migration here
would mean that the moment this experimental branch's daemon touched a
ledger, the **currently deployed production daemon could never open that
ledger again** — recoverable only by restoring a pre-upgrade backup via
`scripts/restore-ledger.sh`.

So the ledger gate is written to be correct against an unmigrated database:

| `bridge_routes` state | `Ledger::route_enabled` returns |
|---|---|
| table absent (today, and every production ledger) | `Route::default_enabled()` |
| table present, no row for this route | `Route::default_enabled()` |
| table present, row present | that row's `enabled` flag |

`default_enabled()` is `true` for the two legacy routes and `false` for
everything else — so an untouched production ledger admits exactly what it
admits today, and any new route is closed.

Table existence is probed via `sqlite_master`, not by catching a "no such
table" error string, so a rusqlite rewording cannot make it fail open.

### The deferred v19 migration (designed, NOT implemented)

**Renumbered from v18 on 2026-09-02.** v18 was taken by the
confirmed-liquidity admission safety buffer, merged upstream as PR #55
(commit `9dbc692`), which adds four columns to `reserve_ledger`:
`admission_buffer_atomic`, `admission_reopen_atomic`,
`liquidity_admission_closed`, `liquidity_admission_closed_at`. Nothing about
this change touches that work; only our number moves.

When Phase 2 needs persistent, operator-settable route state:

```sql
CREATE TABLE IF NOT EXISTS bridge_routes (
    route_id        TEXT PRIMARY KEY,
    source_chain    TEXT NOT NULL,
    destination_chain TEXT NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 0,   -- fail closed
    disabled_reason TEXT,
    updated_at      INTEGER NOT NULL
);
INSERT OR IGNORE INTO bridge_routes VALUES ('GlcToSol','goldcoin','solana',1,NULL,:now);
INSERT OR IGNORE INTO bridge_routes VALUES ('SolToGlc','solana','goldcoin',1,NULL,:now);
INSERT OR IGNORE INTO bridge_routes VALUES ('GlcToRhn','goldcoin','robinhood',0,NULL,:now);
INSERT OR IGNORE INTO bridge_routes VALUES ('RhnToGlc','robinhood','goldcoin',0,NULL,:now);
```

Because it seeds exactly the values `default_enabled()` already produces,
this migration is a **behavioural no-op** — which is the property that lets
it be reviewed on its own merits rather than as a behaviour change.

#### Re-confirm the number before implementing

v19 was free across every local and remote ref at the time of writing: no
branch defines `apply_v19` and none references `bridge_routes` outside this
work. Two reasons to re-check anyway when Phase 2 actually writes it:

1. **A second, unmerged v18 exists.** `feat/goldcoin-admission-safety-buffer`
   (`1598c76`, pushed, not an ancestor of `upstream/main`) carries a
   *different* `apply_v18` adding differently-named columns
   (`admission_safety_buffer_atomic`, `admission_reopen_buffer_atomic`,
   `admission_auto_closed`). If that branch is reconciled by renumbering
   rather than dropping, v19 is its most likely target — and it has a prior
   claim, since its work predates ours.
2. The migration is still documentation only, so renumbering costs nothing
   until the code is written. Deciding late is strictly cheaper than
   deciding wrong.

> **Not our finding to fix, but worth flagging:** those two v18s are mutually
> exclusive. A database that ran the merged v18 is stamped `18`, so
> `open_and_migrate`'s `current < Some(18)` guard would skip the branch's v18
> entirely and its three columns would never be created — surfacing later as
> missing-column errors rather than as a migration failure. That is a
> coordination issue on the safety-buffer branch, untouched by this work.

### Migrations still deferred beyond that

Two further schema changes are needed before a Robinhood route can carry
traffic, and neither is safe or necessary yet:

1. **`reserve_ledger.direction` CHECK widening** — needed for an independent
   Robinhood reserve row (protected minimum, bounds, pause, admission).
   Low risk: nothing has a foreign key to `reserve_ledger` and it holds two
   rows. Still a table rebuild, so still a version bump.

2. **`bridge_requests.direction` CHECK widening + `source_chain` column** —
   high risk: **eight** tables have foreign keys into `bridge_requests`, and
   the existing `ux_bridge_requests_sol_source` unique index puts every
   chain's obligation indices in one namespace, so a Robinhood obligation 0
   would collide with Solana obligation 0. This is **blocking** before
   `RhnToGlc` is ever enabled.

Both are numbered **relative to** the `bridge_routes` migration, not fixed:
if `bridge_routes` lands at v19 these become v20 and v21, in that order —
the low-risk `reserve_ledger` rebuild before the high-risk `bridge_requests`
one, so the risky change can be reverted without unwinding the other. All
three numbers are provisional until Phase 2 re-runs the collision scan.

Until then the narrow `CHECK (direction IN ('GlcToSol','SolToGlc'))` is
retained deliberately as a second backstop underneath the type system: even
a bug that somehow produced a Robinhood request would be rejected by SQLite.

---

## Known merge conflict with `upstream/main`

Verified against `upstream/main` at `597db2b` on 2026-09-02, by trial-merging
this branch into a throwaway branch. **This branch's history was not
rewritten** and the trial branch was deleted.

Everything auto-merges — including `service/src/ledger/mod.rs`, the file both
sides touch most — except one:

**`service/src/config/tests.rs`** conflicts, because both sides appended a
test block to the end of the same file:

| side | block |
|---|---|
| this branch | 5 Robinhood configuration tests (`[robinhood]` absent / empty / independent flags / config-alone-cannot-open / no chain-parameter fields) |
| `upstream/main` | 3 confirmed-liquidity admission-buffer tests (PR #55) |

**Resolution: keep both blocks.** They share no symbol and no fixture beyond
the pre-existing `valid_config` helper, so the order between them does not
matter. This is file adjacency, not a design conflict.

The merged tree was verified: **1033 backend tests pass, 0 fail** — this
branch's 1011 plus upstream's 22 new ones. `CURRENT_SCHEMA_VERSION` resolves
to `18` from upstream, correctly and without intervention, because this
branch never touches `schema.rs`.

### Why this is not pre-resolved by rebasing

Rewriting a reviewed, pushed Phase-1 history to pre-empt a test-file
adjacency would trade something real (a stable, reviewed commit history, and
a PR whose two commits are exactly what was reviewed) for something
temporary: the same class of conflict reappears the next time `upstream/main`
gains a test at the end of that file. Resolving it once, at merge time, is
both cheaper and more honest about what actually happened.

---

## Configuration

The `[robinhood]` section is **optional**, and both its flags default to
`false`. Every existing production config file — none of which has the
section — loads byte-for-byte unchanged and resolves both Robinhood routes
to disabled. Tests pin this.

```toml
# Optional. Absent == present-but-empty == both routes disabled.
[robinhood]
glc_to_rhn_enabled = false
rhn_to_glc_enabled = false
```

The two flags are independent: setting one does not imply the other, so the
directions can never be opened together by accident.

There is deliberately **no chain-parameter field** — no RPC URL, chain id,
token contract, decimals or reserve address. A config slot for a value
nobody has verified is an invitation to fill it with a guess, and a wrong
decimals constant silently mis-scales amounts while a wrong finality depth
silently accepts reversible deposits.

---

## API

Additive only. **No existing endpoint changed shape**, so the deployed UI
and operator tooling are unaffected.

- **`GET /chains`** (new) — the authoritative chain/route registry. Each
  route reports `enabled`, a cause-agnostic `disabled_reason`, and
  `implemented` (whether settlement machinery exists at all, letting a UI
  choose "Coming soon" over "temporarily paused" without parsing prose).
- **`POST /transfers`** — optional `route` field; absent means `GlcToSol`,
  exactly as before. Gated *before* fee computation, chain reads, capacity
  reservation and deposit-address derivation, so a refused route leaves no
  row, no reserved liquidity and no derived address.
- **`POST /quote`** — `direction` is now parsed as a `Route`, so `GlcToRhn`
  is a *recognised* name refused with **409**, distinct from nonsense (400).
- **`ApiError::RouteDisabled` → 409**, deliberately distinct from `Paused`:
  a paused direction is machinery an operator reopens; a disabled route is
  machinery that does not exist in this build. The public message never
  names which gate refused, so a probing client cannot map the deployment.

`GET /status` and `GET /stats` were left completely untouched rather than
gaining a duplicate `routes` array — one source of truth, and zero risk to
their existing consumers.

---

## What Phase 1 deliberately does NOT contain

No Robinhood RPC client, deposits, withdrawals, transaction construction,
reserve address, token contract, confirmation/finality logic, signer
authorization, reserve row, or deployment. No change to the Anchor program,
its IDL, or its program id. No change to the Solana↔Goldcoin settlement
machinery, signing modules, orchestrator, or indexers.

---

## Unresolved chain parameters (blocking for Phase 2)

Enumerated in code at
`service/src/chains/robinhood.rs::UNRESOLVED_CHAIN_PARAMETERS`, with a test
asserting the list has not been quietly emptied:

chain family · chain id · mainnet/testnet · L1/L2/rollup classification and
reorg or challenge window · RPC endpoints and auth · GLC token contract ·
token standard and transfer-hook/freeze behaviour · decimals · reserve
address and custody model · confirmation/finality rule · explorer templates
· fee model · reserve sizing · treasury withdrawal allowlist.

The reorg/finality answer in particular decides whether the Robinhood
indexer follows the Goldcoin model (block scan, reorg-aware, halts) or the
Solana model (cursor over a monotonic obligation count) — and getting that
wrong is a fund-loss class of bug, not a cosmetic one.
