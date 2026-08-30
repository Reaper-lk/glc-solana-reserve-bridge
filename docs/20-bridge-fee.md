# Bridge fee, canonical accounting, and the reserve-capacity unit fix

Approved 2026-08-14/15 as an accounting and product-policy change, layered
on top of the already-approved reserve-backed architecture (docs/00-
executive-summary.md, docs/03-architecture.md) and the already-completed
Token-2022 compatibility work (docs/18-token-2022-support.md). This
document is the reference for what changed, why, and the policies it
establishes. It is the canonical cross-reference target ("docs/20-bridge-
fee.md") named throughout `service/src/amount_conversion.rs`,
`service/src/ledger/`, `service/src/signing/`, `service/src/api.rs`, and
`service/src/solana/indexer.rs`.

## Product framing

This is a **1:1 reserve-backed GLC bridge with a 3% bridge fee.** "1:1"
refers to the underlying GLC denomination *before* the service fee, not to
what a user receives. A deposit of X GLC does not entitle the depositor to
a payout of X GLC — it entitles them to `X - 3% of X`. There is still no
token creation, no minting, no burning, no wrapping, no supply
modification, and no price/oracle/exchange-rate mechanism: the bridge
transfers/releases existing GLC from pre-funded reserves, exactly as
before. The fee is the ONLY thing separating "deposit X" from "receive X."
Any older documentation, comment, or example that implies depositing X
necessarily yields X is describing the pre-fee behavior and should be read
as superseded by this document.

## Fee formula

```
BPS_DENOMINATOR = 10_000
BRIDGE_FEE_BPS   = 300          // exactly 3.00%

fee_amount  = floor(gross_amount * BRIDGE_FEE_BPS / BPS_DENOMINATOR)
net_amount  = gross_amount - fee_amount
```

- `fee_amount` is **floored**, never rounded up — it rounds in the user's
  favor. A gross amount too small for 3% of it to reach a whole atomic
  unit (`gross < BPS_DENOMINATOR / BRIDGE_FEE_BPS`, i.e. `gross <= 33`)
  charges **zero** fee, not a minimum fee.
- `net_amount` is **derived** as `gross - fee`, never computed
  independently — `gross == fee + net` is therefore a structural property
  of `amount_conversion::FeeBreakdown`, not a separately-checked invariant
  that could drift out of sync.
- All arithmetic is checked integer arithmetic (`u64::checked_mul`/
  `checked_sub`) — overflow returns `ConversionError::Overflow` rather than
  wrapping. Floating point is never used anywhere in the fee or conversion
  path, including the human-readable display strings the `/quote` endpoint
  returns (`format_atomic_as_decimal_string` is pure fixed-point integer
  arithmetic).
- Applied **identically in both directions** — Goldcoin→Solana and
  Solana→Goldcoin both charge exactly 3% of the gross amount, computed in
  the same canonical unit (see below). There is one fee formula, one
  constant, one implementation (`amount_conversion::compute_fee`); nothing
  in this codebase computes a fee a second, independent way.

Worked examples (canonical units, i.e. Goldcoin-native 8-decimal atomic
units — see below):

| Gross | Fee (3%) | Net |
|---|---|---|
| 100 GLC | 3 GLC | 97 GLC |
| 500 GLC | 15 GLC | 485 GLC |
| 1,000 GLC | 30 GLC | 970 GLC |
| 2,000 GLC | 60 GLC | 1,940 GLC |
| 20,000 GLC | 600 GLC | 19,400 GLC |
| 0.00000103 GLC (103 atomic units) | 0.00000003 GLC (3 atomic units) | 0.00000100 GLC (100 atomic units) |

## Canonical accounting unit

Goldcoin's native chain uses **8 decimals**. The canonical Solana GLC
Token-2022 mint uses **6 decimals** (docs/18-token-2022-support.md) — the
two chains do not share an atomic unit, and comparing raw atomic amounts
from each chain directly is a real bug class (this is exactly the
"reserve-capacity accounting unit gap" this document's implementation
round was tasked with fixing, alongside the fee — see below).

**One canonical accounting denomination** is used for all `bridge_requests`
gross/fee/net bookkeeping, for both directions: `CanonicalAtomic`, numerically
identical to Goldcoin's own 8-decimal native atomic unit
(`amount_conversion::GOLDCOIN_DECIMALS = 8`). This was chosen, not the
Solana mint's 6-decimal unit or some synthetic third unit, because:

- Goldcoin's decimals are a fixed chain-protocol constant; Solana's mint
  decimals are read live from the mint account and could in principle
  differ across a mint change. Canonical must be independent of anything
  read live.
- For the Goldcoin→Solana direction, the gross amount is inherently
  Goldcoin-native already (it's what the user actually deposits) — using
  Goldcoin's own unit as canonical means GlcToSol's `gross` never needs a
  conversion step at all, only `net` does (widening/narrowing to the
  reserve mint's live decimals).

### Typed units make cross-chain unit confusion a compile error

`amount_conversion` defines two newtypes with **no shared arithmetic or
comparison trait implementation between them**:

```rust
pub struct CanonicalAtomic(pub u64);   // 8 decimals, Goldcoin-native
pub struct SolanaAtomic(pub u64);      // the reserve mint's own live decimals
```

`CanonicalAtomic(1) == SolanaAtomic(1)` (or `+`, `<`, any direct mixing) is
a **compile error**, not a runtime bug someone has to catch with a test.
Conversion between them is only possible through explicit, fallible
methods — `CanonicalAtomic::to_solana(solana_decimals)` and
`SolanaAtomic::to_canonical(solana_decimals)` — both of which delegate to
the one, pre-existing canonical conversion implementation
(`goldcoin_to_solana_atomic`/`solana_to_goldcoin_atomic`, unchanged from
the Token-2022 round; this fee work builds on it, never reimplements it).
This is what "structurally difficult or impossible" cross-chain unit
comparison means concretely in this codebase: the type system, not a
runtime assertion, is the primary defense.

Every capacity check explicitly knows which chain/unit its value belongs
to: `RequestAmounts` (see below) carries `gross_atomic`/`fee_atomic`/
`net_atomic` (always canonical) and a separately-named
`net_destination_atomic` (always the DESTINATION chain's own native unit)
— the field name itself encodes the unit, and `Ledger::create_request`/
`fold_sol_deposit`'s capacity checks are hard-coded to compare
`net_destination_atomic` against the destination reserve's own
native-unit balance, never `gross_atomic`/`net_atomic` against it.

## Rounding / exactness policy (unchanged discipline, now applied twice)

The fail-closed conversion policy from the Token-2022 round is unchanged
and is now applied at two points instead of one: once for widening a
Solana-native gross amount to canonical (SolToGlc; always exact, since
canonical has more decimals), and once for narrowing a canonical net
amount to the reserve mint's live decimals (GlcToSol; exact only if the
narrowed remainder is zero). **A request that cannot be represented safely/
exactly at the destination is rejected, never silently rounded.**
`ConversionError::NotExactlyRepresentable` is returned rather than
truncating or rounding the destination amount.

## Smallest mathematically valid gross amount, per direction

Derived by construction (and brute-force-verified in
`amount_conversion::tests::smallest_valid_*`, not hardcoded), taking into
account both the decimal-precision gap and the 3% fee:

### Goldcoin → Solana: **103 canonical atomic units** (0.00000103 GLC)

The net amount must (a) be nonzero and (b) survive narrowing from 8
decimals to the reserve mint's 6 decimals exactly, i.e. `net % 100 == 0`
(the mint currently has 2 fewer decimals than canonical; this scales with
whatever the live mint decimals actually are, but 6 is what's verified
against the real canonical mint today). For `gross` in `1..103`, `net =
gross - floor(gross * 300 / 10_000)` never reaches a positive multiple of
100 — the first `gross` where it does is 103 (`fee = floor(103 * 300 /
10_000) = 3`, `net = 100`, and `100 % 100 == 0`). Every `gross` below 103
is invalid, for one of two reasons: `net == 0` (only at `gross == 0`) or
`net` not exactly representable at 6 decimals (every other `gross` from 1
to 102).

### Solana → Goldcoin: **1 Solana atomic unit** (0.000001 GLC)

Canonical already IS Goldcoin-native, so widening a Solana-native gross to
canonical is always exact regardless of amount (widening never loses
precision) — the only constraint is `net > 0`. At `gross = 1` Solana
atomic unit, canonical gross is `100` (widened by the 2-decimal gap), fee
is `floor(100 * 300 / 10_000) = 3`, net is `97 > 0`. Valid.

No arbitrary business minimum is imposed on top of these — the API layer's
only floor is `amount_atomic > 0` (`ApiError::BadRequest`); anything
between 1 and the mathematically-smallest-valid amount above fails
*because the conversion itself* rejects it
(`NotExactlyRepresentable`/effectively-zero-net), not because of a
separate policy check. A business minimum (e.g. for UX or dust-avoidance
reasons) is a decision explicitly deferred, per the governing instruction
for this round ("do not invent an arbitrary business minimum yet").

## Reserve-capacity accounting fix

Prior to this round, `reserve_ledger.reserved_liquidity`/
`pending_obligations`/`settled_liquidity_total` tracked the GROSS
user-declared amount, in whatever unit the caller happened to supply —
for GlcToSol that was Goldcoin-native (coincidentally matching
`total_reserve_balance`'s own Solana-native unit only by accident of the
numbers involved, not by any correctness guarantee), and for SolToGlc it
was raw Solana-native, compared directly against a Goldcoin-native balance.
This is the "reserve-capacity accounting unit gap" this round was tasked
with closing.

**Fixed design:** `reserved_liquidity`/`pending_obligations`/
`settled_liquidity_total` now track **NET, in the DESTINATION reserve's
own native unit** — the same unit `total_reserve_balance` is always a live
read in. Every capacity check (`Ledger::create_request`'s and
`fold_sol_deposit`'s) compares `RequestAmounts.net_destination_atomic`
against `available_capacity`, which is computed entirely in the
destination's own native unit:

```
available_capacity = total_reserve_balance - protected_minimum - reserved_liquidity
```

No cross-unit comparison ever happens — `net_destination_atomic` is
already in the right unit by construction (the caller — `api.rs`,
`solana::indexer::tick`, or a test's own fixture — computed it via
`FeeBreakdown::net.to_solana(...)`/`.to_canonical(...)` before calling into
the ledger). The ledger itself never computes a conversion or a fee; it
stays a "dumb" bookkeeping layer that trusts its caller to have already
done the conversion correctly (docs/06-schema.md's existing design
principle, unchanged).

Two ledger-level tests
(`ledger::tests::create_request_capacity_check_is_based_on_net_destination_not_gross_amount`
and its inverse,
`create_request_rejects_when_net_destination_exceeds_capacity_even_for_a_small_gross`)
directly pin this: a gross amount far beyond available capacity is
accepted when its net destination payout fits, and a small gross amount is
rejected when its net destination payout doesn't — proving the check is
genuinely against `net_destination_atomic`, not `gross_atomic`, in both
directions.

## Accrued-fee accounting

Fees are first-class ledger values, not an afterthought column. Every
`bridge_requests` row persists, at minimum: `gross_amount_atomic`,
`fee_bps`, `fee_amount_atomic`, `net_amount_atomic`,
`net_destination_atomic`, source chain/reference (`source_txid`/
`source_vout`/`source_obligation_index`), destination reference
(`destination_txid` derived from either the release signature or the
Goldcoin payout txid), and the request's own id.

**The fee stays on the SOURCE side where it was collected** — it is never
automatically transferred anywhere:

- GlcToSol (source = Goldcoin): fee accrues into the **GoldcoinReserve**
  row's `accrued_fees_atomic`, credited inside `mark_release_confirmed`
  (the same transaction that settles the request), via a second `UPDATE
  reserve_ledger ... WHERE direction = 'GoldcoinReserve'` alongside the
  primary settlement UPDATE (which targets `SolanaReserve`, the
  destination).
- SolToGlc (source = Solana): fee accrues into the **SolanaReserve** row's
  `accrued_fees_atomic`, credited inside
  `mark_goldcoin_completion_confirmed` the same way, targeting the other
  reserve than the one being settled.

`accrued_fees_atomic` is **always canonical units, regardless of which
row it's recorded on** — a deliberate, explicitly-documented exception
from that row's other columns (which are all in that reserve's own
native unit). This keeps fee revenue comparable across both directions
without a conversion step, since fee accounting is a reporting concern,
not a capacity concern.

**No treasury wallet/address exists, and no fee-withdrawal path exists
yet** — this is a deliberate scope boundary for this round (see docs/09-
runbook.md's "Accrued bridge fees" section). Accrued fees are purely an
audit-visible running total.

### Why accrued fees never inflate available capacity

`available_capacity = total_reserve_balance - protected_minimum -
reserved_liquidity`. Since `reserved_liquidity`/`pending_obligations`/
`settled_liquidity_total` were switched to track NET (not gross) amounts
as part of the capacity fix above, fee revenue was **never counted as
customer-obligation capacity in the first place** — the gross-minus-fee
gap simply becomes unencumbered headroom automatically, with no separate
subtraction needed. `accrued_fees_atomic` is therefore purely additive
reporting, never subtracted from or otherwise mixed into
`available_capacity`, `reserved_liquidity`, or the hard-invariant check.
This is verified by
`reconciliation::tests::reconciliation_reports_accrued_fees_without_them_masking_a_real_breach`,
which credits a large accrued-fee balance and confirms a real, unrelated
unexplained balance drop still classifies as `Breach` and auto-pauses —
the accrued-fee figure cannot soften or excuse a genuine solvency problem.

### Where accrued fees are surfaced

- `Ledger::accrued_fees(direction) -> u64`.
- `reconciliation::ReconciliationReport.accrued_fees` — populated on every
  `reconcile()` call, alongside the existing balance/capacity fields.
- `ops::reserve_health::ReserveSnapshot.accrued_fees` — consumed by
  `ops::collector`/`ops::health`'s `/metrics` endpoint
  (`glc_{direction}_reserve_accrued_fees_atomic` gauge) and by
  `glc-admin status`'s text output.

Accrued-fee accounting survives restart/recovery like every other
settlement field (SQLite's own durability — no separate recovery code
path exists, same discipline as docs/09-runbook.md's crash-recovery
model), and is never double-credited on a duplicate/idempotent replay of
`mark_release_confirmed`/`mark_goldcoin_completion_confirmed`, because
both functions return early (before reaching the fee-accrual `UPDATE`)
once the request is already `Settled`. Both properties are pinned by
`goldcoin_payout_lifecycle.rs`'s `accrued_fees_survive_a_restart` and
`mark_completed_is_idempotent` tests.

## Canonical-message/attestation changes: a deliberate non-change

**The on-chain program's instruction signatures and `shared::claim`'s
wire-format byte layouts (`release_claim_message`/
`goldcoin_completion_message`) are unchanged by this round.** No `fee_bps`
or `gross_amount` field was added to either the on-chain program or the
shared claim-message format. This was a deliberate architecture decision,
weighed against extending the wire format, and is the single most
significant design decision of this round.

**Why this is safe**, mechanically:

1. `BRIDGE_FEE_BPS` is a Rust `const`, never a runtime parameter passed
   into any signing or attestation function. There is no code path,
   anywhere in the trust boundary, where a caller — API client, database
   row, or on-chain instruction argument — supplies the fee rate. An
   attacker cannot "alter `fee_bps`" because `fee_bps` is not data at any
   point past `compute_fee`'s own internal use of the constant; the only
   way to change it is to change and recompile the binary.
2. `net` (the only fee-derived quantity that actually needs to reach a
   signed message or an on-chain instruction) is **never trusted from
   ledger storage**. Every settlement-construction call site —
   `signing::attestation::independently_attest_release`/
   `independently_attest_completion`, `signing::goldcoin_vault::
   DevLedgerPayoutSource::rederive_plan`, `orchestrator::submit_release` —
   calls `amount_conversion::verify_fee_breakdown(gross_atomic,
   stored_fee_atomic, stored_net_atomic)`, which **recomputes** the fee
   breakdown from `gross_atomic` using the compile-time `BRIDGE_FEE_BPS`
   and compares it against the stored `fee`/`net` columns, returning
   `ConversionError::AccountingMismatch` and refusing to proceed on any
   disagreement. The value that actually gets signed/submitted is always
   the freshly recomputed one, never the stored one — so even a
   successfully tampered database row (a direct SQL edit, a corrupted
   restore, a bug in some future code path) can influence what gets
   signed only if it happens to already match what recomputation would
   produce, which is not a bypass at all.
3. `gross` is anchored to ground truth independently of the fee mechanism
   entirely: for GlcToSol, `gross_amount_atomic` is checked byte-for-byte
   against the real, independently-observed Goldcoin deposit amount at
   `record_glc_deposit_observed` time (pre-existing exact-match discipline,
   unchanged); for SolToGlc, `gross_atomic` is derived by widening the
   real, immutable, on-chain `WithdrawalObligation.amount` — there is no
   step anywhere that accepts a caller-supplied `gross`.

Given (1)-(3), every attack the governing instruction named — fee bypass,
altered `fee_bps`, altered gross amount, altered net amount, `gross != fee
+ net`, decimal confusion, unit confusion, direct-program-call fee bypass,
API/client fee manipulation — reduces to "can an attacker get gross wrong"
(no, see 3) or "can an attacker get net signed without it matching a
correct recomputation from a correct gross" (no, see 2), without any new
wire-format byte ever needing to exist to prove it. Adding a `fee_bps`/
`gross_amount` field to the signed message would have added attack
surface (a new field to parse/validate, a new way for old and new message
versions to disagree) without closing any gap this design leaves open.

This is a considered trade-off, not an oversight: a future round that
wants `fee_bps` to be a *governance-adjustable* runtime parameter (rather
than a compile-time constant) would need to revisit this decision and
would very likely need the wire-format change this round deliberately
avoided — that is explicitly out of scope here, since the governing
instruction fixed the rate at exactly 3% / 300 bps.

Direct on-chain program invocation, bypassing this service entirely, is
covered by the pre-existing (unchanged) on-chain signature-threshold
check: `release_from_reserve`/`record_goldcoin_completion` both require a
valid aggregated attestation proof over the exact claim bytes being
submitted. Since attestation signers only ever sign a claim carrying the
correctly fee-adjusted `net` (point 2, above), a direct program call
attempting to claim a different (fee-bypassing) amount would need a valid
signature over that different amount — which requires compromising
enough attestation-signer custody domains to meet the threshold, the same
blast-radius as any other on-chain forgery attempt (docs/02-trust-model.md,
docs/10-threat-model.md).

## Fee-policy snapshots (in-flight requests across a rate change)

`BRIDGE_FEE_BPS` prices a request exactly once — at creation
(`api::create_glc_to_sol_transfer`) or fold (`solana::indexer::tick`) —
and the rate is persisted as the request's `fee_bps` snapshot. From then
on, `fee_bps`/`fee_amount_atomic`/`net_amount_atomic` are immutable
historical accounting: every settlement, attestation, and recovery path
validates the request via `verify_fee_breakdown` at the STORED snapshot
rate (`fee = floor(gross * stored_bps / 10_000)`, `net = gross - fee`),
never at the currently compiled-in rate. This is what lets a request
created under an earlier fee policy keep settling after a rate change
(the production request #818 bug: a 6%-era request was being re-judged
against 3% and refused). It does not weaken fail-closed validation: a
snapshot rate outside `HISTORICAL_FEE_BPS` is refused outright, and
stored fee/net that fail to reconcile against the genuine snapshot keep
being refused exactly as before.

## Fee-bypass protections (summary table)

| Attack | Protection |
|---|---|
| Fee bypass (net == gross claimed) | `verify_fee_breakdown` recomputes and rejects on mismatch |
| Altered `fee_bps` | The per-request snapshot (`bridge_requests.fee_bps`) is only accepted if it is a rate the protocol actually charged at some point (`HISTORICAL_FEE_BPS` allowlist: 100, 600, 300 bps) AND the stored fee/net reconcile exactly against it; NEW requests always price at the compile-time `BRIDGE_FEE_BPS`. A claimed rate outside the allowlist (0 bps, say) fails closed even when internally consistent |
| Altered gross amount | Anchored to the real observed deposit / real on-chain obligation amount |
| Altered net amount | Recomputed, never trusted from storage |
| `gross != fee + net` | `FeeBreakdown` makes this unrepresentable by construction; `verify_fee_breakdown` also catches a tampered stored triple |
| Decimal confusion | Typed `CanonicalAtomic`/`SolanaAtomic`, no shared arithmetic |
| Unit confusion | Same as above; every field name encodes its unit |
| Replay / duplicate settlement | Pre-existing on-chain `DepositClaim` PDA (GlcToSol) / DB `UNIQUE` + idempotent state machine (SolToGlc, docs/10-threat-model.md), unchanged |
| Overflow / underflow | Checked integer arithmetic throughout; `ConversionError::Overflow` on any overflow, never wraps |
| Direct-program-call fee bypass | Requires a valid threshold attestation signature over the bypassing amount, which signers never produce |
| API/client fee manipulation | `CreateTransferInput`/`QuoteInput` have no fee/net field at all; unknown JSON fields are silently ignored by serde, never merged into the computed breakdown (proven at the HTTP layer by `api::tests::client_supplied_fee_fields_in_the_request_body_are_silently_ignored`) |

## API / quote support

`POST /quote` (`ApiSource::quote`, `api::QuoteInput`/`QuoteOutput`) runs
the exact same `amount_conversion::compute_fee` the real settlement path
uses — there is no second fee-preview implementation to drift out of sync
with the authoritative one. Response fields: `direction`, `gross_amount`,
`gross_display_amount`, `fee_bps`, `fee_amount`, `fee_display_amount`,
`net_amount`, `net_display_amount`, `source_decimals`,
`destination_decimals`, `source_asset`, `destination_asset`. Display
amounts are formatted via `format_atomic_as_decimal_string` — pure integer
fixed-point arithmetic, no floating point anywhere in the response.

The future bridge UI displaying "You bridge: X GLC / Bridge fee (3%): Y
GLC / You receive: Z GLC" must source X/Y/Z from this endpoint's response,
never compute them client-side — `POST /transfers`
(`create_glc_to_sol_transfer`) independently recomputes the same breakdown
server-side regardless of what a quote previously returned, so even a
stale or tampered quote response cannot influence what actually settles.

## Schema (v5)

`CURRENT_SCHEMA_VERSION` bumped 4 → 5 (`ledger::schema::apply_v5`):

- `bridge_requests.amount_atomic` renamed to `gross_amount_atomic`.
- `bridge_requests` gained `fee_bps`, `fee_amount_atomic`,
  `net_amount_atomic`, `net_destination_atomic`.
- `reserve_ledger` gained `accrued_fees_atomic`.

Applied via SQLite's native `ALTER TABLE ... RENAME COLUMN` (verified
compatible with the bundled `libsqlite3-sys` version, including correctly
updating the table's own `CHECK` constraint) plus `ADD COLUMN`, both in
both the fresh-init and incremental-migration code paths.

## Tests

Comprehensive coverage across:

- `amount_conversion::tests` — fee formula (100/1,000 GLC worked examples),
  `gross == fee + net` by construction, fee always rounds down, overflow/
  underflow for `compute_fee` and both typed units' `checked_add`/
  `checked_sub`, smallest valid gross per direction (brute-force-derived,
  both the valid boundary and every invalid amount below it),
  `verify_fee_breakdown` accepting a correct record and rejecting a
  tampered fee, a tampered net, and a `gross != fee + net` record.
- `ledger::tests` — capacity check proven to be net-destination-based in
  both directions (a huge-gross/small-net case that succeeds, and a
  small-gross/huge-net case that fails).
- `signing::attestation::tests` — golden-layout release/completion
  messages now assert the NET (fee-adjusted) amount; two new tests
  directly tamper a ledger row's `fee_amount_atomic`/`net_amount_atomic`
  via raw SQL and confirm attestation fails closed with
  `AccountingMismatch` rather than signing the tampered value.
- `api::tests` — quote/transfer fee math, capacity reserved on net not
  gross, and the HTTP-level "client-submitted fee fields are ignored"
  test.
- `reconciliation::tests` — accrued fees reported without masking a real
  breach.
- `goldcoin_payout_lifecycle.rs`/`adversarial.rs`/`restart_recovery.rs`/
  `regtest_acceptance.rs` — every existing lifecycle, replay,
  concurrency, and real-node acceptance test updated to the real
  fee-adjusted numbers (not a zero-fee shortcut) wherever the test
  exercises the actual settlement/attestation path; a zero-fee
  `RequestAmounts` helper is used only in tests that predate the fee and
  exercise orthogonal structural/replay/restart properties.
- Full Token-2022 regression: all pre-existing Token-2022-round tests
  pass unchanged (only the numeric expectations that depended on the
  now-fixed capacity-unit bug or the new fee were updated).

See the checkpoint report (docs/21-bridge-fee-checkpoint.md) for exact
test counts and the full quality-gate result.
