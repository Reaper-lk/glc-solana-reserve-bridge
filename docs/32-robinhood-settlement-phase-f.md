# Robinhood Network — Phase F handoff (settlement engine)

**Status:** implementation complete on `feat/robinhood-settlement`, **uncommitted**,
**not deployed**, **routes ship disabled**. Phase F builds the machinery that
*could* execute `GlcToRhn` and `RhnToGlc`; it does not open either one, and it
does not ship a production authorization signer. The launch blockers in
[§10](#10-remaining-launch-blockers) are the gap between "the code exists" and
"real money may cross this bridge".

Phase 1 (route identity, fail-closed admission, inert adapter) is
`docs/30-robinhood-network-phase1.md`. Phase E (deposit indexer, observation
store, schema v22) is the previous commit, `1f62529`. This document is Phase F.

---

## 1. What Phase F adds

| Area | Module | What it is |
|---|---|---|
| EIP-712 authorization | `service/src/robinhood/auth.rs` | `PayoutAuth` / `RefundAuth` / `SettlementAuth` digests, transcribed from `GlcRobinhoodBridge.sol` and cross-checked against a golden fixture |
| Quorum signing | `service/src/robinhood/signer.rs` | `EvmAuthSigner` trait, 2-of-3 collection, local recovery of every signature |
| Contract reads + calldata | `service/src/robinhood/calls.rs` | `ContractGate` pre-broadcast checks; `executePayout` / `executeRefund` / `executeSettlement` calldata |
| Startup verification | `service/src/robinhood/preflight.rs` | proves the configured addresses are the contracts this code was written against |
| Submitter + nonce manager | `service/src/robinhood/submitter.rs` | the single gas-paying EOA, durable nonce allocation, broadcast/replacement policy |
| Deposit fold | `service/src/robinhood/fold.rs` | one finalized Robinhood deposit → exactly one `bridge_requests` row |
| Settlement engine | `service/src/robinhood/settlement.rs` | `fold → authorize → broadcast → receipts`, all resumable |
| Refunds | `service/src/robinhood/refund.rs` | return an obligation's exact principal to its depositor |
| Settlement config | `service/src/robinhood/settlement_config.rs` | `[robinhood.settlement]`, every field required, no defaults |
| Durable tx record | `service/src/ledger/robinhood_tx.rs` | `robinhood_transactions` + signatures + submitter state; the idempotency design |
| EVM primitives | `service/src/evm/{abi,rlp,secp,tx}.rs` | ABI encode/decode, RLP, secp256k1 sign/recover/low-`s`, legacy + EIP-1559 tx signing |
| Schema | `service/src/ledger/schema.rs` (`apply_v23`) | v22 → v23 |

---

## 2. `GlcToRhn` flow (Goldcoin → Robinhood)

```
API quote/create  ->  Goldcoin deposit observed and confirmed
                  ->  request reaches SourceFinalized (existing orchestrator)
                  ->  fee/net computed in canonical 8dp, re-verified from gross
                  ->  net widened EXACTLY to Robinhood 18dp
                  ->  PayoutAuth digest minted, 2-of-3 signatures collected
                  ->  ContractGate check at Latest
                  ->  nonce allocated, tx signed, raw bytes persisted
                  ->  executePayout broadcast
                  ->  receipt read back, status must be 1
                  ->  bridge event present in the receipt's logs
                  ->  contract replay guard confirms the operation at that block
                  ->  required_confirmations reached  ->  request Settled
```

Notable properties:

- The **fee breakdown is recomputed** from the request's own gross at its own
  recorded rate and must reconcile (`verify_fee_breakdown`) before an
  authorization is minted, so a tampered fee column cannot produce a payout.
- The canonical→Robinhood widening is an exact ×10<sup>10</sup>
  (`CANONICAL_TO_ROBINHOOD_SCALE`). It is checked, not assumed; a value that
  cannot be represented exactly is a refusal, both at quote time and at
  authorization time.
- The **quote path already refuses** an amount whose net is not exactly
  representable at 18dp, so a quote never promises something the transfer
  would reject.
- The destination reserve is `RobinhoodReserve`; see [§7](#7-reserve-accounting).

## 3. `RhnToGlc` flow (Robinhood → Goldcoin)

```
DepositCreated observed (Phase E)  ->  promoted FINAL at the indexer's depth
   ->  folded into exactly one bridge_requests row
   ->  Goldcoin payout built and broadcast by the EXISTING orchestrator
   ->  Goldcoin payout CONFIRMED at required_payout_confirmations
       (request reaches DestinationConfirmed)
   ->  SettlementAuth digest minted, 2-of-3 signatures collected
   ->  executeSettlement broadcast
   ->  receipt + effect verification + confirmation depth
   ->  obligation Settled on-chain, request Settled
```

**The ordering is the load-bearing part.** `executeSettlement` moves the
on-chain obligation out of `Pending`, which is the only state a refund can be
issued from. Marking it settled is an irreversible statement that the depositor
has been paid on Goldcoin, so it is the **last** step and its precondition is
the strongest available evidence: the Goldcoin payout's own transaction,
verified against the chain, at or past `goldcoin.required_payout_confirmations`.
Not "broadcast", not "in the mempool", not "we signed it". This mirrors the
Solana leg's existing `record_goldcoin_completion` discipline rather than
reinventing it.

**Folding happens even when the route is disabled.** A deposit that has already
landed on-chain cannot be un-landed by a flag on this side. If a closed route
meant "do not fold", real irreversible deposits would have no ledger row, no
reserve accounting, no operator visibility and no refund path. So a finalized
deposit is *always* folded; the route gate governs what happens next. A request
folded while the route is closed lands in `ManualReview` with an explicit
reason, holds no reserve, and pays out nothing until a human resumes or refunds
it. Same posture as `Ledger::fold_sol_deposit` for a Solana deposit arriving
while the Goldcoin reserve is paused.

The event carries both the 18dp `amount` and the contract's `canonicalAmount`.
The Phase E decoder cross-checked them; the fold **re-derives** the canonical
value a second time at the moment the amount becomes money in a ledger, and
refuses on any disagreement.

---

## 4. EIP-712 authorization

`service/src/robinhood/auth.rs` is a **transcription, not a design**. Every
constant, field and field order is copied from
`contracts/src/GlcRobinhoodBridge.sol` and must match the deployed bytecode
exactly.

- Domain: `name = "GlcRobinhoodBridge"`, `version = "1"`, plus `chainId` and
  `verifyingContract` from the verified deployment.
- Actions: `PAYOUT = 0x01`, `REFUND = 0x02`, `SETTLE = 0x03`.
- Each digest binds: action, route byte, the contract's own
  `(protocolSourceChainId, protocolDestChainId)` pair, token, request id,
  recipient/obligation, amount, `signerEpoch` and `expiry`.

**Drift protection.** Neither side generates the fixture:
`contracts/test/fixtures/eip712-golden.json` is asserted by
`contracts/test/GoldenDigests.t.sol` to be what the *deployed contract*
produces, and `auth::tests::golden` asserts the Rust encoder produces the same
bytes. A drift on either side fails a test on that side.

**`AbandonmentAuth` is deliberately absent.** It exists on-chain and closes an
obligation without paying out *and* without refunding — it retains a depositor's
principal. Nothing in this service can produce such a signature. If that path is
ever needed it belongs in a separately reviewed operator tool.

**Quorum.** `SIGNER_THRESHOLD = 2` of 3. Each returned signature is recovered
locally against the exact digest that was sent; the recovered address must be
the identity that signer was configured as *and* a member of the contract's
authorized set. A bad signature therefore costs nothing — it is caught before a
nonce is allocated, before gas is spent, and before a reverting broadcast.
Signatures are normalised low-`s` (EIP-2), which OpenZeppelin's
`ECDSA.recover` requires.

---

## 5. EVM submitter and nonce manager

**The submitter is not a bridge authority.** It pays gas and broadcasts. Every
value-moving call carries the 2-of-3 quorum inside its calldata, verified by the
contract against its own signer set, and no authorized path in
`GlcRobinhoodBridge` reads `msg.sender`. A stolen submitter key can waste gas
and can refuse to broadcast — a denial of service — and can do nothing else.
Config refuses a deployment where the submitter address is also a signer or the
bridge contract.

Nonce rules, each with the failure it prevents:

| Rule | Prevents |
|---|---|
| Allocate from the **ledger's** own maximum, in the same write transaction that stores it | two operations racing to one nonce between two RPC round trips |
| Persist the nonce **before** signing, and the signed bytes **before** broadcasting | a crash leaving a broadcast nobody can find or re-send |
| A retry re-broadcasts the **identical bytes** under the **same nonce** | a second transaction racing the first |
| `eth_getTransactionCount("pending")` is a **floor**, never the allocator | a lagging replica handing out a nonce already in use |
| A replacement bumps the fee and keeps the nonce | the same |
| An **uncertain broadcast never reallocates** | duplicate payouts |
| A **reverted** transaction is terminal, never auto-retried | spending gas repeatedly on a call whose precondition is false |

When `eth_sendRawTransaction` fails at the transport layer the question was
never answered — the bytes may be in the mempool, propagating, already mined, or
never delivered. Allocating a fresh nonce would mean that in the "already
arrived" case *both* could mine. Re-broadcasting the identical bytes has no such
case. `already known` and `nonce too low` are routine, expected answers.

An uncertain broadcast is resolved by evidence, in order of strength:

1. a receipt for the recorded hash;
2. the contract's replay guard `requestExecuted(action, requestId)` — this
   survives a receipt aging out of a node's index;
3. `eth_getTransactionCount` moving past the nonce with no receipt and no
   replay-guard hit. That is **not** a resolution; it is an incident, and it
   stops for a human.

Fees and gas are bounded by configuration: `max_fee_per_gas_wei`,
`priority_fee_wei`, `gas_limit_margin_percent`, `max_gas_limit`,
`min_submitter_balance_wei` (checked *before* a nonce is allocated),
`rebroadcast_after`, `max_replacements`. The envelope (legacy vs EIP-1559) has
no default and is cross-checked against the chain at preflight.

---

## 6. Receipt and finality behaviour

`RobinhoodTxState`: `Authorizing → Authorized → Signed → Broadcast → Included →
Finalized`, with `Reverted` and `ManualReview` as the terminal off-ramps.

- **No receipt** is not "failed" — it is "not mined yet, as far as this node
  knows". The broadcast phase decides whether to replace, based on
  `rebroadcast_after` and `max_replacements`.
- **`status = 0`** → `Reverted`. Terminal. Deliberately **not** retried under a
  fresh nonce: a revert means a contract-checked precondition was false, and
  re-sending would revert identically while spending more gas.
- **`status = 1` is not sufficient.** The receipt must also contain at least one
  log from the bridge contract, *and* the contract's replay guard must report
  the `(action, requestId)` as executed **at the receipt's own block**. A
  status of 1 says the transaction did not revert, not that it did what was
  intended.
- **`Included → Finalized`** only at `robinhood.settlement.required_confirmations`
  depth, counted against the live head. This is a separate knob from the
  indexer's inbound `confirmation_depth`: inbound depth decides when someone
  else's deposit is irreversible, outbound depth decides when this service's own
  payout is.
- `Finalized` is the **only** state from which the bridge request itself may be
  completed. Any post-finality contradiction moves the row to `ManualReview`.

Contract state is read again immediately before every broadcast
(`ContractGate::check`): replay guard, migration status, pause flags, route
enable flag, signer epoch. Pre-broadcast reads are pinned to `Latest`;
post-receipt verification reads the exact block the transaction landed in.

---

## 7. Refund and settlement lifecycle

A refund is **not a withdrawal**. Neither the destination nor the amount is
chosen by an operator, by this service, or by the signers:

- the **recipient** is the obligation's own recorded `depositor`, read back from
  the contract;
- the **amount** is the obligation's own recorded `amount`, from the same place —
  not the ledger's gross, not the net, not anything this service computed;
- there is **no fee** — charging for a payout that never happened would be
  taking money for nothing;
- there are **no partial refunds** — the contract compares exactly and reverts
  on any difference.

Signers choose *which* obligation to refund and nothing else.

Request lifecycle: `ManualReview → RefundPending → RefundBroadcast → Refunded`,
one-way, never returning to `ManualReview`.

**Settlement and refund are mutually exclusive, twice over.** On-chain: both
require the obligation to be `Pending` and both leave a terminal status, so
whichever lands first makes the other revert. In the ledger: `begin_refund`
refuses on finding a settlement row (and vice versa), and
`ux_robinhood_tx_operation` on `(kind, request_id)` makes a duplicate of either
impossible. Two independent mechanisms, because "we refunded a deposit we had
already settled" is the failure that loses real money.

Refunds are **never automatic**. `robinhood::begin_refund` is exported and fully
tested but has **no production caller** — the operator entry point is Phase G
(see blocker G). A deposit parked in `ManualReview` stays parked until a human
decides between resuming and refunding, because those are opposite,
irreversible answers to the same question.

---

## 8. Reserve accounting

A third physical reserve, `ReserveDirection::RobinhoodReserve`, accounted
separately from the Goldcoin and Solana ones and **never netted against
either**.

- **Units are canonical 8dp**, like every other reserve row — not Robinhood's
  native 18dp. At 18 decimals one whole GLC is 10<sup>18</sup>, so `i64::MAX` is
  about 9.2 GLC and an INTEGER column would overflow on a single real transfer.
  Nothing is lost: the two units differ by an exact factor of
  10<sup>10</sup>, and every amount crossing the boundary must be an exact
  multiple of it (the contract's `_requireCanonicalAmount` on-chain,
  `RobinhoodAtomic::to_canonical` off-chain).
- Which reserve each direction draws down:
  `RobinhoodReserve → [GlcToRhn]`, `GoldcoinReserve → [SolToGlc, RhnToGlc]`,
  `SolanaReserve → [GlcToSol]`. `pending_destination_settlement_amount` sums a
  **set** of directions per reserve, so a real in-flight `RhnToGlc` payout is
  not read as an unexplained Goldcoin balance drop.
- **Fees accrue on the source side**, where they were withheld
  (`docs/20-bridge-fee.md`): a `GlcToRhn` fee accrues to `GoldcoinReserve`, an
  `RhnToGlc` fee accrues to `RobinhoodReserve` — always on a separate
  `accrued_fees_atomic` column, never netted against the paying reserve's own
  balance columns.
- **The reserve is configured only when `[reserve.robinhood]` is present**, which
  no existing production config has. An unconfigured reserve has no
  `reserve_ledger` row at all, so nothing can be reserved against it and no
  Robinhood settlement can pass admission. The fail-closed default is "this
  reserve does not exist", not "this reserve is empty".
- Reconciliation and reserve-health treat `RobinhoodReserve` like `SolanaReserve`
  for the UTXO-specific terms (no `vault_utxos` concept, no UTXO backpressure).
- `admin_api` reports it as `"robinhood"`.

Sizing and rolling limits are **not chosen** — see blocker F.

---

## 9. Restart and idempotency guarantees

The hard requirement is that no restart, at any point, can produce a duplicate
payout, a duplicate settlement, a duplicate refund, or a second nonce for one
unresolved broadcast. That is achieved by making the duplicate
**unrepresentable**, not by careful statement ordering — an orchestrator can be
killed between any two statements.

| Duplicate | What prevents it |
|---|---|
| Two payouts for one request | `ux_robinhood_tx_operation` on `(kind, request_id)` |
| Two operations under one nonce | `ux_robinhood_tx_nonce` on `(submitter, chain_id, nonce)` |
| Two operations claiming one contract request id | `ux_robinhood_tx_contract_request` |
| A broadcast with no persisted nonce or bytes | table `CHECK` on `state` |
| A completed operation with no successful receipt | table `CHECK` on `state` |
| A third signature, or two from one signer | signatures-table PK + unique index |
| Two requests for one observation | `ux_robinhood_observation_request` |

Every one is a database constraint. A bug in the engine produces a constraint
violation and a stalled request — never a second transfer.

Supporting properties:

- **No phase holds state across a tick.** Each reads what is durably recorded,
  decides one step, and commits it before moving on.
- The authorization row is written **before the first signer is asked**, so a
  crash mid-collection resumes with the same payload rather than minting a
  second, differently-expiring one.
- `raw_tx` holds the exact bytes written before the first
  `eth_sendRawTransaction` and never rewritten. After a crash mid-broadcast the
  service re-broadcasts identical bytes rather than reconstructing or re-signing.
- The two daemon loops (`glc-bridge-daemon`'s Solana/Goldcoin loop and
  `robinhood::daemon::run_settlement`) interleave **through the ledger only** —
  every transition either performs is a committed SQLite transaction with its own
  preconditions, so neither can observe a half-applied step of the other.
- Tick phase order is `fold → authorize → broadcast → receipts`, with receipts
  last so a transaction broadcast in a tick gets its first poll on the next one.

---

## 10. Schema v23

`CURRENT_SCHEMA_VERSION = 23`. v22 shipped an *observation* store and said so
structurally: `settled INTEGER NOT NULL DEFAULT 0 CHECK (settled = 0)`, a column
whose only permitted value was "not settled", precisely so that settling a
Robinhood deposit would require dropping a `CHECK` in a migration a reviewer
would see. v23 is that migration.

Three widenings:

1. `bridge_requests.direction` gains `'GlcToRhn'` and `'RhnToGlc'`.
   `'SolToRhn'` / `'RhnToSol'` are **deliberately not added** — a database that
   cannot spell them is a second, independent guarantee on top of the absent
   `Direction` variants.
2. `reserve_ledger.direction` gains `'RobinhoodReserve'`.
3. `robinhood_deposit_observations.settled` becomes `CHECK (settled IN (0,1))`
   and gains `folded_request_id`.

Three new tables: `robinhood_transactions`, `robinhood_authorization_signatures`,
`evm_submitter_state`.

**How the widenings are performed.** SQLite cannot alter a `CHECK`, so each
table is rebuilt. `widen_check_constraint` reads the table's *real* current DDL
from `sqlite_master`, replaces one exact substring, and copies rows through a
column list read from `PRAGMA table_info` — never a hand-written column list
(`reserve_ledger` alone has nine `ALTER TABLE ADD COLUMN` migrations behind it).
Indexes and triggers are captured before the drop and replayed after the rename
from the same authoritative source. The substring must occur **exactly once** or
the migration refuses to run. The whole migration runs in one `BEGIN IMMEDIATE`
transaction, with `PRAGMA foreign_keys` toggled outside it and restored on every
path out, and a post-rebuild `foreign_key_check` + `integrity_check` that rolls
back on any violation. It is structurally idempotent: the presence of
`robinhood_transactions` is the probe.

**The nonce is a column of the operation, not a row in an allocator table.**
A nonce and the operation it was allocated for commit or roll back together, so
there is no window in which a nonce exists without an owner.

> **Migration warning, unchanged from Phase 1:** `open_and_migrate` refuses to
> open a database written by a newer binary and stamps its own version on every
> database it opens. A v23 daemon touching a production ledger makes that ledger
> unopenable by the currently deployed binary — recoverable only via
> `scripts/restore-ledger.sh`. Do not point this branch's daemon at a production
> ledger.

---

## 11. `SolToRhn` / `RhnToSol` remain non-executable

Unchanged from Phase 1 and re-asserted by v23:

- `Route::as_direction()` returns `None` for both, so no `Direction` value can be
  produced — and every reserve, ledger and signing function requires one.
- No `Direction` variant names both a Solana and a Robinhood endpoint.
- `Route::contract_route_id()` returns `0x03` / `0x04`, but
  `VerifiedDeployment::chains_for()` returns `None` for both, so no
  authorization can be built.
- The v23 `bridge_requests.direction` CHECK cannot spell either name.
- `service/tests/robinhood_route_isolation.rs` pins all of it, including
  `d_the_solana_robinhood_routes_are_unspellable_in_the_database`.

`GlcToRhn` / `RhnToGlc` also ship **disabled**: `Route::default_enabled()` is
`false` for all four Robinhood routes, the settlement loop is spawned only when
`[robinhood.settlement]` is present *and* preflight passed, and the loop's own
gate requires **both** executable routes to be open before it acts on either.

---

## 12. Test results

Run on this worktree, at the state described here:

| Check | Result |
|---|---|
| `git diff --check` | clean |
| `cargo +nightly fmt -- --check` | clean |
| `cargo +nightly clippy --all-targets -- -D warnings` | clean |
| `cargo +nightly test` | **1806 passed, 0 failed, 2 ignored** (26 suites) |
| `forge test` (in `contracts/`) | **320 passed, 0 failed, 0 skipped** (19 suites) |

The 2 ignored tests are pre-existing long-running harnesses unrelated to
Phase F (`funding_bootstrap_matures_enough_balance_for_a_large_reserve_profile`,
`soak_profile_wiring_short_duration_smoke`).

---

## 13. Remaining launch blockers

None of these are defects in the Phase F code. They are the things that must be
true about the *world* before a Robinhood route may be opened.

### A. Production EIP-712 signer backend is still missing

Current production signer infrastructure does not yet provide Robinhood EIP-712
quorum signing. `Config::load_robinhood_auth_signers` returns an **empty pool**
in `operators.mode = "production"`, and that is deliberate rather than
unfinished: the existing `signing::remote` protocol signs Goldcoin BIP-143
sighashes and Solana ed25519 messages, neither of which is a 32-byte EIP-712
digest returned as a 65-byte compact secp256k1 signature with low-`s` applied.
Extending it is a change to the signer-side deployment as much as to this
client, so it belongs in the same reviewable change as the signer service that
answers it.

Until then a production deployment has no Robinhood authorization signers,
cannot assemble a quorum, and cannot broadcast — the correct fail-closed
outcome, and the daemon logs it loudly at startup. `DevEvmAuthSigner` is
dev/test only and is never constructed in production mode.

### B. Robinhood live-chain parameters are still unverified

Nothing in this repository establishes any of the following, and none of them
were guessed:

- **transaction envelope / fee behaviour** — whether the chain has an EIP-1559
  fee market at all. `tx_envelope` has no default and is cross-checked against a
  live block header at preflight, but the correct value is unknown.
- **gas behaviour** — real execution cost, and whether
  `gas_limit_margin_percent` / `max_gas_limit` defaults are sane.
- **finality depth** — the correct `required_confirmations` (outbound) and
  indexer `confirmation_depth` (inbound).
- **reorg characteristics** — depth and frequency; the indexer's rollback path
  exists but its depth budget is unvalidated.
- **RPC provider behaviour** — `eth_getLogs` range limits, rate limits,
  pagination behaviour, receipt-index retention.

### C. `GlcRobinhoodBridge` has not been deployed

Deployment address and start block are **unknown**. Every configuration field
that names them is therefore unfillable, and preflight cannot be run against
anything.

### D. Robinhood GLC token security properties require verification

Preflight reads `decimals()` and asserts it is 18. It proves **nothing else**
about the token, and claiming otherwise because a `decimals()` call succeeded
would be the worst possible outcome. Still to be established by a separate
mainnet token review:

- decimals (independently confirmed, not just read back)
- mint authority and minting path
- blacklist / freeze / pause capability
- proxy / upgradeability
- fee-on-transfer / rebase behaviour
- privileged transfer behaviour (transfer hooks, admin transfer, clawback)

Any of these being true changes the bridge's solvency model.

### E. `libsecp256k1` dependency/security issue is unresolved

RUSTSEC-2025-0161 — `libsecp256k1` 0.6 is unmaintained upstream. It is a
**direct** dependency, already used for Goldcoin vault signing, pinned for
`rand` 0.7 / `rand_core` 0.5 ABI compatibility, and acknowledged in
`service/deny.toml` with a standing P2 item: *evaluate migrating to an
actively-maintained secp256k1 crate before production custody keys are ever
used with this code path*. Phase F adds a second call site (EVM signing and
recovery) and does **not** change that assessment. Adding `k256` alongside it
would put two secp256k1 implementations in one binary; the migration should move
both call sites at once.

Must be resolved before real custody keys touch this path.

### F. Production Robinhood reserve sizing and rolling limits are not chosen

`[reserve.robinhood]` bounds (protected minimum, target, warning, critical) and
the contract's own `Limits` (inbound/outbound rolling limits, bucket width,
protected minimum reserve) have no chosen production values. The contract's
rolling buckets are per-**direction** and shared across routes, so the real
ceiling is the sum of the configured limits — sizing them is a policy decision,
not a default.

### G. Phase G still needs admin recovery / health / status support

Specifically:

- an operator entry point for `robinhood::begin_refund` (implemented and tested,
  **no production caller** today);
- resume/park controls for `ManualReview` requests folded while the route was
  closed;
- a `RefundView` projection for `RhnToGlc` in the public API — `refund_view`
  currently returns `None` for that direction rather than mislabelling a
  Robinhood refund as a Goldcoin or Solana one;
- admin surfacing of `robinhood_transactions` state, submitter balance and
  nonce, and stuck/reverted operations;
- health and status reporting for the settlement loop alongside the indexer's.

---

## 14. Classification of the blockers

| Blocker | Kind |
|---|---|
| A — production EIP-712 signer backend | **code** (signer service + client protocol) |
| B — live-chain parameters | launch / configuration verification |
| C — contract not deployed | launch / deployment |
| D — token security properties | security verification |
| E — `libsecp256k1` | **code** (dependency migration) + security |
| F — reserve sizing and rolling limits | launch / configuration policy |
| G — admin recovery / health / status | **code** (Phase G scope) |

No Critical or High issue is outstanding **in the Phase F code itself**. A, E
and G are code work that is deliberately out of Phase F's scope; B, C, D and F
are launch, configuration and security-verification work that no amount of code
in this repository can discharge.

---

## 15. What was NOT done

- Nothing was committed or pushed.
- No production deploy, no config activation, no route enablement.
- No production config file references `[robinhood.settlement]` or
  `[reserve.robinhood]`.
- The on-chain program (`programs/glc-reserve-bridge`) is unchanged.
- `GlcRobinhoodBridge.sol` is unchanged by Phase F. On the contracts side only
  a test, a fixture and one build setting were added:
  `contracts/test/GoldenDigests.t.sol`,
  `contracts/test/fixtures/eip712-golden.json`, and a **read-only**
  `fs_permissions` entry in `contracts/foundry.toml` scoped to
  `test/fixtures/` — read-only on purpose, because a test that could *write*
  the fixture could quietly edit it into agreement with itself, which is
  precisely the failure the fixture exists to make impossible.
