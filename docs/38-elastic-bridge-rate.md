# Elastic bridge rate — Phase 2A: the persisted bridge quote (schema v37)

Implemented 2026-09-14 on founder decisions received the same day (recorded
below). Phase 2A introduces the bridge-quote math, its persistence, and
quote-aware verification across all six routes at a **fixed bridge rate of
exactly 1.0**. Phase 2B — live rail prices and the gates around them — is
scoped at the end of this document and is NOT part of this change.

This document is the canonical cross-reference target ("docs/38-elastic-
bridge-rate.md") named throughout `service/src/bridge_rate.rs`,
`service/src/ledger/`, `service/src/api.rs` and the settlement paths.

## Terminology

Bridge **rate**, bridge **fee**, bridge **quote** — and only those words.
Nothing in this design converts one asset into another: the bridge still
transfers existing GLC from pre-funded reserves. The rate is the ratio of
the two rails' prices, and a quote is the rate applied to one deposit.

## What was decided (2026-09-14)

| # | Decision |
|---|---|
| J-2 | The per-route `[fees]` table (bps) stays the fee. No `bridge_fee_pct` key is added, and no production fee value changes in Phase 2A. |
| J-3 | Phase 2B sources are fixed: Goldcoin L1 → NonKYC, Solana → Jupiter, Robinhood Chain → Uniswap pool. Exact endpoints, symbols, quote currencies, pool addresses and fee tiers are researched and documented in Phase 2B. **No live feeds in Phase 2A.** |
| J-4 | Goldcoin-sourced routes lock the settlement quote at the **first deposit observation**. The `POST /transfers` quote is indicative only. |
| J-5 | Fee accounting is **option (a)**: the bridge fee is persisted and accrued in the destination-canonical interpretation. No second, source-denominated fee. |
| J-6 | Phase 2B: with no valid "one window ago" reference (restart, feed gap) the affected routes **halt** until the full reference window exists. No degraded fallback, no last-known-price admission. |
| J-7 | Phase 2B may floor to destination precision with less than one destination atomic unit of residual retained by the bridge. **Phase 2A at rate 1.0 preserves current behaviour exactly** — no dust behaviour in live route processing. |
| J-8 | Phase 2A now, Phase 2B afterwards. **When Phase 2B ships, all six route `[fees]` are set to 300 bps.** Not before. |

## The math

```text
PRICE_SCALE = 1_000_000_000_000                       // a price of 1.0

gross_out = floor(gross_in * source_price_e12 / destination_price_e12)
fee_out   = floor(gross_out * fee_bps / 10_000)       // the existing fee rule, unchanged
net_out   = gross_out - fee_out
```

`gross_in` is what the depositor sent — anchored to the real deposit
exactly as before (`gross_amount_atomic`, checked byte-for-byte against
the observed Goldcoin output, or widened from the immutable on-chain
obligation). `gross_out`, `fee_out` and `net_out` are the destination-
asset figures in the canonical 8-decimal unit; `net_out` then goes through
the unchanged decimal conversion to the destination chain's own unit
(`net_destination_atomic`), with the unchanged exactness rule.

Every intermediate is `u128`, every step is checked, and no floating
point exists anywhere in `service/src/bridge_rate.rs` — a test asserts the
module also contains no HTTP client, no RPC client and no async runtime.
Two processes given the same integers produce the same integers.

**Phase 2A pins both prices to `PRICE_SCALE`** (`RateBook::fixed_unit`).
At a unit rate `gross_out == gross_in`, so `fee_out`/`net_out` are
bit-for-bit what `compute_fee_at_bps` produced before this change — the
signer messages, the reserve accounting and every API amount are
unchanged. `bridge_rate::tests::a_unit_rate_reproduces_the_fee_rule_bit_for_bit`
pins this across the fee rule's whole range; the synthetic 0.8 / 1.25 /
1/3 cases pin the math Phase 2B will drive.

## Where a quote is struck, and where it is locked

There is ONE `RateBook` per daemon process (`glc-bridge-daemon` builds it
from `[bridge_rate]` and hands the same value to the public API, both
Solana folds, both Robinhood folds and the Goldcoin deposit observation),
so no two components can quote one deposit differently.

| Route | Struck | Locked (= the settlement quote) |
|---|---|---|
| `GlcToSol`, `GlcToRhn` | `POST /transfers` — **indicative**, stored on the row unlocked | `Ledger::record_glc_deposit_observed_from`, when the deposit is first seen in a block; re-struck at that instant at the row's own `fee_bps` snapshot |
| `SolToGlc`, `SolToRhn` | the Solana indexer's fold | the fold (the obligation is already final) |
| `RhnToGlc`, `RhnToSol` | the Robinhood fold (`robinhood::fold`) | the fold (the observation is already `Final`) |
| `GET /quote` | a preview, through the same `RateBook::quote` | never |

A reorg that orphans the observing block (`mark_glc_reorged`,
`goldcoin_rollback_reorg`) clears `quote_locked_at` together with the
outpoint; the re-observation strikes and locks a fresh quote. A quoted row
whose quote is not locked cannot settle
(`ConversionError::QuoteNotLocked`).

Phase 2A invariant at the lock: the observation-time quote reproduces the
reservation's fee and net exactly (it must — the rate is 1.0), so only the
quote columns are written and the row's amounts and reservation are
untouched. A lock that would NOT reproduce them (a row with corrupted
amounts, or a fee snapshot the fee rule refuses) is not written; the
observation is still recorded — the deposit is real and must stay visible
and refundable — and settlement then refuses the row exactly as a
corrupted reservation was refused before v37. Phase 2B replaces that
branch with re-reserving from the lock and parking an unquotable deposit.

## Schema v37

Eight nullable columns on `bridge_requests`, added in place with
column-level idempotent `ALTER TABLE ... ADD COLUMN`:

| column | meaning |
|---|---|
| `quote_source_price_e12` | source rail price × 10¹² |
| `quote_destination_price_e12` | destination rail price × 10¹² |
| `quote_gross_out_atomic` | `floor(gross_in × src / dst)`, canonical |
| `quoted_at` | when the quote was struck |
| `quote_expires_at` | `quoted_at + quote_lifetime_secs` — metadata; nothing reads it |
| `quote_source_feed_at`, `quote_destination_feed_at` | feed read times (audit; the quoting instant at a fixed rate) |
| `quote_locked_at` | when the quote became the settlement quote; NULL while indicative |

`CHECK`s make the first seven all-NULL or all-set, prices positive, and a
lock impossible without a quote. **NULL means legacy**: no row is
backfilled, and a request created before v37 keeps every quote column
NULL and keeps settling through `amount_conversion::verify_fee_breakdown`
at an implicit unit rate, exactly as before.

The existing amount columns keep their names with sharpened meaning under
a quote: `gross_amount_atomic` = `gross_in`; `fee_amount_atomic` =
`fee_out`; `net_amount_atomic` = `net_out`; `net_destination_atomic` =
`net_out` in the destination chain's unit. `ledger::RequestAmounts` gained
`quote: Option<BridgeQuote>`; every production pricing site supplies one,
and the ledger refuses to store amounts that are not the quote's own
figures (`LedgerError::BridgeQuote`).

## Verification — one entry point

`BridgeRequest::verify_breakdown` is the canonical amount verification for
a row, and every settlement, recovery and reconciliation path calls it
instead of choosing a verifier itself:

- quoted row → must be locked, and `gross_out`/fee/net must reproduce
  from `(gross_in, prices, fee_bps)` — `bridge_rate::verify_quoted_breakdown`,
  refusing with `ConversionError::QuoteMismatch`;
- legacy row → `verify_fee_breakdown`, refusing with `AccountingMismatch`.

The returned breakdown is always the freshly recomputed one; the stored
figures are only ever compared against. Callers: `signing::attestation`
(release and completion), `orchestrator::submit_release`,
`signing::goldcoin_vault::DevLedgerPayoutSource::rederive_plan`,
`goldcoin::payout_recovery`, `robinhood::settlement::authorize_payout`,
`solana::reconcile_request`. The Solana completion attestation's
cross-check against the on-chain obligation amount prices that amount
through `BridgeRequest::expected_net_for_gross` — the row's own persisted
rate, never a live one.

## Signers

Unchanged: wire formats (`shared::claim`), signer keys, multisig, custody
policy, the Solana program, the Robinhood contract. Remote signers never
query a price source; they sign what the daemon re-derives from the
persisted row, exactly as before. At a unit rate the bytes are identical
to a legacy row's —
`signing::attestation::tests::a_quoted_request_signs_exactly_the_bytes_a_legacy_request_signs`
proves it for both the release claim and the completion claim, and the
pre-existing golden-layout tests now run against quoted rows.

## Accounting (J-5, option a)

`fee_amount_atomic` is `fee_out` — the bridge fee in destination-asset
canonical units — and it is what `reserve_ledger.accrued_fees_atomic`
accrues, on the SOURCE reserve, in `mark_release_confirmed` /
`mark_goldcoin_completion_confirmed`, exactly as before. The fee is still
physically retained on the source reserve; the accrued figure reports that
retention valued in the destination asset at the request's own quoted
rate. At a unit rate the two are the same number. There is deliberately
no second, source-denominated fee.

## API

- `GET /quote` gains `bridge_quote` (`bridge_rate`, `source_price_e12`,
  `destination_price_e12`, `gross_in_amount`, `gross_out_amount`,
  `fee_bps`, `bridge_fee_amount`, `net_out_amount`, `quoted_at`,
  `quote_expires_at`). The pre-quote fields (`gross_amount`, `fee_amount`,
  `net_amount`, display strings) are kept for existing clients;
  `gross_amount` keeps meaning what the user sends.
- `POST /transfers` returns the indicative quote as `bridge_quote`
  (`locked_at` absent).
- `GET /transfers/{id}` reports the row's quote as `bridge_quote`, with
  `locked_at` once it is the settlement quote; absent for a legacy row.

## Config

```toml
[bridge_rate]
quote_lifetime_secs = 60   # default when the section is absent
```

Metadata only in Phase 2A. `[fees]` is unchanged and remains the fee.

## Rounding

Unchanged in live route processing: at rate 1.0 every figure is what it
was, the fee floors as before, and the destination exactness rule
(`NotExactlyRepresentable`) still refuses rather than rounds. The
`bridge_rate` unit tests exercise the generic math at 1.0 / 0.8 / 1.25 /
1/3 (`gross_out` floors; `gross_out == fee_out + net_out` structurally).
Destination-precision flooring with sub-unit residual (J-7) is Phase 2B.

## Explicitly NOT in Phase 2A

No live feeds. No smoothing window. No staleness gate. No band check. No
rate-dependent route halt. No non-1.0 rate reachable through production
configuration.

## Phase 2B (scoped, not started)

1. Feeds: NonKYC (Goldcoin L1, BTC→USD conversion if the market is
   BTC-quoted), Jupiter (Solana), Uniswap pool (Robinhood Chain) —
   research and document exact endpoints, symbols, quote currencies, pool
   addresses and fee tiers. Reuse the in-tree `reqwest` / `EvmCallRpc` /
   `SolanaRpc` patterns; no new dependency unless unavoidable.
2. `RateBook` live mode: per-rail samples, ring buffer over
   `price_window_secs` (default 360), staleness (`price_staleness_secs`,
   default 120), invalid-sample drop (zero/negative/non-numeric/overflow),
   no last-known fallback. Make the book positional on `BridgeApi::new`,
   `SolanaIndexer::new`, `goldcoin::Indexer::new`, `Settler::new`,
   `Orchestrator::new`, and drop the `fold_observation` /
   `fold_observation_to_solana` default-book wrappers.
3. Halt: `bridge_rate_unavailable` as a new `InboundAdmissionBlocker`
   (after the operator gates, before the wallet windows); `POST /transfers`
   503; folds park; the Goldcoin observation parks an unquotable deposit.
4. Band: `rate_band_pct` (default 25) against one window ago; new deposits
   park `bridge_rate_band_exceeded_*` with the locked quote; restart / feed
   gap with no reference window halts (J-6).
5. Re-reserve at the lock: when the observation-time quote moves the net,
   rewrite the row's amounts and adjust `reserved_liquidity` in the same
   transaction; interaction with the destination floor and
   `per_transfer_limit` checked at quote time.
6. Destination-precision flooring with sub-unit residual (J-7).
7. Set all six `[fees]` entries to 300 bps at the Phase 2B release (J-8).
8. `docs/20-bridge-fee.md`'s 1:1 product framing superseded in full.
