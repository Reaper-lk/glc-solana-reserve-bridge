# Bridge-fee implementation checkpoint

Completes the round described in docs/20-bridge-fee.md: the required 1%
bridge fee, implemented as part of one coherent accounting pass alongside
the reserve-capacity unit-accounting fix. Reported against the governing
instruction's 16-item checkpoint structure.

## 1. 1% fee implementation status

**Complete.** Every settlement path — GlcToSol (`api::create_glc_to_sol_transfer`
→ `Ledger::create_request` → `signing::attestation::independently_attest_release`
→ `orchestrator::submit_release`) and SolToGlc (`solana::indexer::tick` →
`Ledger::fold_sol_deposit` → `signing::goldcoin_vault::independently_sign` →
`signing::attestation::independently_attest_completion`) — computes,
persists, independently re-verifies, and settles on the fee-adjusted NET
amount. No code path settles the raw gross amount.

## 2. Exact fee formula

```
fee = floor(gross * 100 / 10_000)   // exactly 1.00%, BRIDGE_FEE_BPS = 100
net = gross - fee                    // derived, not independently computed
```

Floors (rounds in the user's favor), never rounds up. `gross == fee + net`
holds by construction. Checked integer arithmetic throughout; no floating
point anywhere in the fee/conversion/display path. Full derivation and
worked examples: docs/20-bridge-fee.md "Fee formula".

## 3. Canonical accounting unit design

One canonical unit (`CanonicalAtomic`, 8 decimals, numerically identical
to Goldcoin's own native atomic unit) for all `bridge_requests` gross/fee/
net bookkeeping, both directions. `CanonicalAtomic`/`SolanaAtomic` are
distinct newtypes with no shared arithmetic/comparison trait
implementation — mixing them is a compile error, not a runtime bug.
Full design: docs/20-bridge-fee.md "Canonical accounting unit".

## 4. Goldcoin 8-decimal / Solana 6-decimal handling

Unchanged conversion primitives from the Token-2022 round
(`goldcoin_to_solana_atomic`/`solana_to_goldcoin_atomic`), now wrapped by
typed `CanonicalAtomic::to_solana`/`SolanaAtomic::to_canonical` methods and
used internally by `compute_fee`'s callers. Solana mint decimals are
always a live read (`solana::accounts::fetch_reserve_mint_decimals`),
never hardcoded, consistent with the Token-2022 round's policy.

## 5. Rounding/exactness policy

Unchanged fail-closed discipline: a request whose NET amount cannot be
represented exactly at the destination's decimal precision is rejected
(`ConversionError::NotExactlyRepresentable`), never silently rounded.
Applied at both conversion points (widening SolToGlc gross to canonical;
narrowing GlcToSol net to the reserve mint's decimals).

## 6. Minimum mathematically valid amount, per direction

- **GlcToSol: 101 canonical atomic units** (0.00000101 GLC) — the smallest
  gross whose NET amount is both nonzero and exactly representable at the
  reserve mint's 6-decimal precision. Every amount from 1 to 100 canonical
  units is invalid.
- **SolToGlc: 1 Solana atomic unit** (0.000001 GLC) — canonical has more
  decimals than Solana, so widening is always exact; the only constraint
  is `net > 0`, satisfied at the smallest possible nonzero input.

Both brute-force-verified (not hardcoded) in
`amount_conversion::tests::smallest_valid_glc_to_solana_gross_is_101_canonical_atomic_units`
and `..._smallest_valid_solana_to_glc_gross_is_one_solana_atomic_unit`. No
arbitrary business minimum was added on top — full derivation: docs/20-
bridge-fee.md "Smallest mathematically valid gross amount, per direction".

## 7. Reserve-capacity accounting fix

`reserve_ledger.reserved_liquidity`/`pending_obligations`/
`settled_liquidity_total` now track NET, in the DESTINATION reserve's own
native unit (matching `total_reserve_balance`), replacing the prior bug
where these columns tracked GROSS in a unit that only accidentally lined
up with the balance column for one direction and never for the other.
Every capacity check compares like-for-like units by construction — see
docs/20-bridge-fee.md "Reserve-capacity accounting fix" for the full
before/after and the two ledger-level tests that pin the net-vs-gross
distinction directly.

## 8. Accrued-fee accounting design

`reserve_ledger.accrued_fees_atomic` (new column, schema v5), always
canonical units regardless of which reserve's row it's on, credited on
the SOURCE side at settlement time (never automatically transferred; no
treasury wallet/address; no withdrawal path yet — deliberate scope
boundary, docs/09-runbook.md). Never subtracted from or mixed into
`available_capacity` — proven never to mask a real reconciliation breach.
Surfaced via `Ledger::accrued_fees`, `ReconciliationReport.accrued_fees`,
`ReserveSnapshot.accrued_fees` (→ `glc-admin status` and the
`glc_{direction}_reserve_accrued_fees_atomic` `/metrics` gauge). Survives
restart; never double-credited on idempotent replay. Full design: docs/20-
bridge-fee.md "Accrued-fee accounting".

## 9. Canonical-message/attestation changes

**No on-chain program change. No shared claim-message wire-format
change.** `BRIDGE_FEE_BPS` is a compile-time constant, never threaded
through as runtime data anywhere in the trust path. Every settlement-
construction call site recomputes gross→fee→net via
`amount_conversion::verify_fee_breakdown` and fails closed
(`AccountingMismatch`) on any disagreement with what's stored, rather than
ever trusting the stored fee/net columns. `gross` itself is anchored to
the real observed deposit (GlcToSol) or the real immutable on-chain
obligation amount (SolToGlc), never caller-supplied. This is the single
most significant architecture decision of this round — full reasoning,
including why extending the wire format was considered and rejected:
docs/20-bridge-fee.md "Canonical-message/attestation changes: a
deliberate non-change".

## 10. Fee-bypass protections

Fee bypass, altered `fee_bps`, altered gross/net, `gross != fee + net`,
decimal confusion, unit confusion, replay, duplicate settlement, overflow/
underflow, direct-program-call fee bypass, and API/client fee
manipulation are each addressed by a specific, named mechanism — full
table in docs/20-bridge-fee.md "Fee-bypass protections (summary table)".
None of these required a new on-chain instruction or wire-format field.

## 11. API quote implementation

`POST /quote` (new endpoint) runs the exact same `amount_conversion::
compute_fee` the real settlement path uses — one implementation, no
drift-prone second copy. Response: `direction`, `gross_amount`/
`gross_display_amount`, `fee_bps`, `fee_amount`/`fee_display_amount`,
`net_amount`/`net_display_amount`, `source_decimals`,
`destination_decimals`, `source_asset`, `destination_asset`. Display
strings are pure fixed-point integer formatting
(`format_atomic_as_decimal_string`), never floating point. `POST
/transfers` independently recomputes the same breakdown regardless of any
prior quote, so a stale or tampered quote can never influence settlement.

## 12. Tests added and full test results

Comprehensive matrix added across `amount_conversion::tests` (fee formula,
worked 100/1,000 GLC examples, `gross == fee + net` by construction, fee
always floors, overflow/underflow for `compute_fee` and both typed units'
checked arithmetic, smallest valid gross per direction with brute-force
verification of every amount below the boundary, `verify_fee_breakdown`
accepting a correct record and rejecting a tampered fee/net/gross-sum),
`ledger::tests` (capacity checked on net not gross, both directions),
`signing::attestation::tests` (golden-layout messages assert NET; two new
tests directly tamper a ledger row via raw SQL and confirm attestation
fails closed), `api::tests` (fee-aware quote/transfer math, capacity on
net, HTTP-level client-fee-field-ignored test), `reconciliation::tests`
(accrued fees never mask a breach), and integration tests
(`goldcoin_payout_lifecycle.rs` gained accrued-fee-idempotency and
accrued-fee-survives-restart tests; every existing lifecycle/replay/
concurrency/real-node-acceptance test updated to real fee-adjusted
numbers).

**Full quality-gate result** (`service/` workspace):

```
cargo +nightly fmt --check          -> clean
cargo +nightly clippy --all-targets -D warnings -> clean, zero warnings
cargo +nightly test --all-targets   -> 296 lib tests + 8 adversarial +
                                        2 daemon_smoke + 7 goldcoin_payout_lifecycle +
                                        3 regtest_acceptance (skipped, no local
                                        goldcoind/solana-test-validator prereqs) +
                                        7 restart_recovery + 3 runbook_commands
                                        = ALL PASS, 0 failed
cargo build (root workspace, on-chain program) -> clean, unaffected by
                                        this round's no-wire-format-change
                                        decision
cargo deny check                    -> advisories ok, bans ok, licenses ok,
                                        sources ok
```

## 13. Token-2022 regression status

**All pass, no regressions.** The Token-2022 round's own tests (live mint-
decimals reads, `MetadataPointer`/`TokenMetadata`-only extension
allowlist, real `spl_token_2022` litesvm program integration, real-node
Token-2022 acceptance tests) are unchanged in behavior and pass unchanged
in this round's full test run — only numeric expectations that depended
on the now-fixed capacity-unit bug or the new fee were updated where those
same test functions also exercised amount math.

## 14. Commits created

1. `Implement 1% bridge fee and fix reserve-capacity unit accounting` —
   core library implementation: `amount_conversion` fee/typed-unit
   extensions, schema v5, `RequestAmounts`-based ledger signatures, every
   settlement-construction call site switched to net-and-verified amounts,
   `/quote` endpoint, and every existing call site/test updated to compile
   and pass against the new signatures.
2. `Surface accrued fees in reconciliation/health/audit output; expand fee
   test matrix` — `accrued_fees` wired into `ReconciliationReport`/
   `ReserveSnapshot`/`glc-admin status`/`/metrics`, plus the full
   user-specified fee/accounting test matrix (tamper detection, overflow/
   underflow, net-vs-gross capacity proof, accrued-fee idempotency and
   restart survival, reconciliation-with-accumulated-fees, HTTP-level
   client-fee-bypass-attempt test).
3. (this commit) — documentation: docs/20-bridge-fee.md (the canonical
   design reference), this checkpoint, and targeted updates to docs/03-
   architecture.md, docs/05-reserve-accounting.md, docs/09-runbook.md, and
   docs/10-threat-model.md correcting stale unqualified "1:1" framing and
   cross-referencing the new document.

No push, no PR, no merge, no deploy, no production keys, no mainnet
transactions — all work is local commits on `main` only, per the governing
instruction.

## 15. Remaining blockers

None for this round's scope. Explicitly out of scope, unchanged from
before: a fee-withdrawal procedure/treasury (docs/09-runbook.md — not yet
scoped, deliberately), a governance-adjustable (rather than fixed
compile-time) fee rate (would require revisiting the no-wire-format-change
decision), and a business-minimum-transfer policy on top of the
mathematically-derived minimums (deferred per the governing instruction).
These were named as explicit non-goals for this round, not gaps
discovered during it.

## 16. Updated completeness percentages

Relative to the full bridge scope described in docs/00-executive-summary.md
through docs/19-token-2022-checkpoint.md, plus this round's addition:

- **Fee/accounting model**: 100% of this round's specified scope
  (formula, canonical unit, capacity fix, accrued-fee tracking and
  surfacing, quote API, fee-bypass protections, test matrix,
  documentation).
- **Overall bridge implementation**: unchanged from the prior Token-2022
  checkpoint's assessment for everything this round didn't touch
  (on-chain program, trust model, reconciliation mechanics, Goldcoin-side
  vault/payout construction) — this round is a pure accounting-layer
  addition on top of already-complete settlement plumbing, not a new
  settlement path.
- **Explicitly still open** (unchanged from before this round): fee
  withdrawal/treasury design, staged multi-operator attestation-key
  rotation tooling, Goldcoin vault sweep-to-fresh-vault compromise
  response tooling, and production parameter selection (confirmation
  depths, reserve floors, rate limits) — all named in docs/09-runbook.md
  as deferred to real operational experience, not code gaps.
