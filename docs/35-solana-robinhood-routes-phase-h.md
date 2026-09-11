# Phase H — the two Solana↔Robinhood routes (`SolToRhn`, `RhnToSol`)

**Status:** implemented, tested, **DISABLED on every gate**. Nothing in this
phase enables a route, deploys a contract, or touches production state.

## 1. What was built, in one sentence each

- **`SolToRhn`** = the existing Solana deposit observation (`solana::indexer`,
  finalized `WithdrawalObligation`s) joined to the existing Robinhood payout
  engine (`robinhood::Settler`, `executePayout` under contract route `0x03`),
  closed out on Solana by the existing `record_goldcoin_completion` exactly as
  `SolToGlc` is.
- **`RhnToSol`** = the existing Robinhood deposit observation
  (`robinhood::indexer`, finalized `DepositCreated` on route `0x04`) joined to
  the existing Solana reserve release (`release_from_reserve` under the same
  2-of-3 attestation), closed out on Robinhood by the existing
  `executeSettlement` exactly as `RhnToGlc` is.

No new operation kind, no new claim family, no new signer protocol, no new
reserve. Two existing legs are joined per route, and the `route` a payload
binds is the only thing that differs.

## 2. No contract change was required

| Contract | Operation | Already supports the route? |
|---|---|---|
| `GlcRobinhoodBridge` (deployed) | `deposit(0x04, …)` | yes — `_routeLegs` models `ROUTE_RHN_TO_SOL` as inbound |
| | `executePayout` on route `0x03` | yes — `_requireOpenPayoutRoute` admits `ROUTE_SOL_TO_RHN` |
| | `executeSettlement` / `executeRefund` | yes — route read from the obligation's own storage |
| | `routeChains(0x03)`, `routeChains(0x04)` | yes — immutables set at construction, read at preflight |
| `glc-reserve-bridge` (Solana program) | `deposit_to_reserve` | yes — opaque ≤64-byte destination payload |
| | `release_from_reserve` | yes — `(txid, vout)` is an opaque replay key; the quorum verifies the source |
| | `record_goldcoin_completion` | yes — `(index, 32-byte payout id, height, net, sha256(destination))` |

Both contracts' route flags for `0x03`/`0x04` remain `false` on-chain.

## 3. Reserve and accounting model

| Route | Source reserve (fee accrues here) | Destination reserve (reservation, settlement) | Destination unit |
|---|---|---|---|
| `SolToRhn` | `SolanaReserve` | `RobinhoodReserve` | canonical 8dp |
| `RhnToSol` | `RobinhoodReserve` | `SolanaReserve` | reserve mint native (live `decimals`, 6 today) |

- The fold reserves `net_destination_atomic` on the destination reserve; a park
  reserves nothing.
- Value leaves the destination reserve at **destination finality** — Robinhood
  payout at the configured depth (`Ledger::mark_robinhood_payout_settled`),
  Solana release at `finalized` (`Ledger::mark_release_confirmed`) — and the
  cached balance is decremented then, so reconciliation never reads a routine
  settlement as an unexplained drop. The request reaches
  `DestinationConfirmed`, **not** `Settled`.
- `Settled` is reached only by the source-side close-out: the Solana completion
  for `SolToRhn` (`Ledger::mark_robinhood_payout_completion_confirmed`), the
  Robinhood settlement for `RhnToSol` (`Ledger::mark_robinhood_settlement_confirmed`).
  Neither moves a reserve.
- The fee accrues on the source reserve's `accrued_fees_atomic`, canonical
  units, never netted against any balance column. `Direction::source_reserve`
  and `Direction::destination_reserve` name the two rows and are pinned by test
  never to coincide.

## 4. Decimal conversion — explicit and tested

```text
SolToRhn:  mint (6dp, live) --x100 exact--> canonical (8dp) --fee--> net --x10^10 exact--> Robinhood (18dp)
RhnToSol:  Robinhood (18dp) --/10^10 exact-or-refuse--> canonical --fee--> net --/100 exact-or-refuse--> mint (6dp, live)
```

Every arrow is one of the two pre-existing conversion functions
(`SolanaAtomic::to_canonical`, `CanonicalAtomic::to_robinhood`,
`RobinhoodAtomic::to_canonical`, `CanonicalAtomic::to_solana`) and the one
pre-existing fee engine. `SolToRhn` can never be inexact. `RhnToSol` has two
exactness failures, both refused rather than rounded: a deposit that is not a
whole multiple of `10^10` (the contract refuses it on-chain too), and a NET
that is not a whole multiple of the canonical→mint scale (e.g. 1.00000010 GLC
at 300 bps nets 0.97000010, unspellable at 6 decimals). The second folds to
`ManualReview` (`undeliverable amount: …`), holds nothing, and is refundable on
Robinhood; `GET /quote` refuses the same shape before the deposit is made.
Mint decimals are read live on every fold, never assumed.

## 5. Route selection on the Solana leg

The Solana program has no route byte, so a Solana deposit is classified by its
destination payload: `0x` + 40 hex digits is a Robinhood destination. This is
structurally unambiguous — `0` is not in the base58 alphabet, so no Goldcoin
address can begin with `0x` — and is the same argument
`ledger::TransferAddressFilter` already rests on. Classification is ON only
when `SolToRhn` is priced (`[fees].SolToRhn`); an unpriced deployment folds
every deposit as `SolToGlc`, bit-for-bit as before. A `0x` payload that is not
a valid EVM address (bad hex, failed EIP-55 checksum, the zero address) folds
as `SolToRhn`, parked `undeliverable destination: …`, refundable on Solana —
never as a Goldcoin request.

An `RhnToSol` destination is a Solana pubkey as 32 raw bytes or its base58
text; the two are unconfusable (a base58 spelling of a 32-byte key is 43–44
bytes).

## 6. Gates, admission, limits

- **Enablement**: the unchanged three-place AND (`RouteGate`): config flag
  (`[robinhood].sol_to_rhn_enabled` / `rhn_to_sol_enabled`, default `false`),
  `bridge_routes` (seeded `0`, `is_operator_settable` now true for all four
  contract routes), adapter capability (Solana adapter serves both; Robinhood
  adapter serves them only from a verified deployment carrying their chain
  pairs). Plus the contract's own `routeEnabled`, read live before every
  broadcast.
- **Admission**: schema v27 adds `route_admission` rows for both cross routes
  (seeded OPEN), so `route-admission-close --route SolToRhn` parks that route's
  new deposits and nothing else. The reserve-wide `paused`/`admission_closed`
  and the confirmed-liquidity buffer on the destination reserve still apply,
  through the same `InboundAdmissionGates` evaluator both folds and
  `GET /chains` use.
- **Settlement loop**: `run_settlement` consults the gate per route. The
  Goldcoin pair keeps its existing "both must be open" rule; each cross route
  is gated alone.
- **Limits**: amount limits stay on-chain and per-direction (both contracts
  deliberately share windows across routes so the stated limit is the true
  limit). Backend limits are the per-reserve capacity/buffer gates. The
  Goldcoin rolling-24h recipient/source-wallet cooldowns are Goldcoin-payout
  policy and do not apply to either cross route.

## 7. Fees

`[fees].SolToRhn` and `[fees].RhnToSol`, per route, resolved once at load. A
`[fees]` table written before Phase H keeps loading unchanged: a cross route
may go **unpriced while disabled**, and enabling one without a rate is a
startup error (never another route's rate, never the compiled-in constant).
The migration fallback (no `[fees]`) carries nothing forward for them — there
was no pre-existing rate. `fees-set --route SolToRhn` is the one edit with no
"before".

## 8. ManualReview, failure, refund

Deterministic fold-time notes: `route_disabled_at_fold`,
`route_admission_closed_at_fold`, `reserve_paused_at_fold`,
`admission_closed_at_fold`, `liquidity_buffer_low_at_fold`,
`insufficient_capacity_at_fold`, `undeliverable destination: …`,
`undeliverable amount: …`. A reverted/expired/stale destination transaction
parks the request (existing Robinhood behaviour, extended by direction). A
park holds no reserve, the source deposit stays recorded, and:

- `SolToRhn` refunds on Solana via the existing `refund_withdraw` path
  (`glc-admin refund-manual-review`), with `route_disabled_at_fold` and
  `undeliverable destination` added as refundable reasons for that route only.
- `RhnToSol` refunds on Robinhood via the existing `executeRefund` path; the
  obligation's on-chain route is cross-checked against the request's before
  any authorization.
- Resume (`Ledger::resume_manual_review_cross_route`, reached through the same
  audited `resume-manual-review`) re-applies the destination reserve's
  invariant and safety buffer, reserves exactly once, is idempotent, and
  refuses any refund lifecycle. `route_disabled_at_fold` is not resumable —
  the same posture `RhnToGlc` takes.

## 9. Idempotency and replay

- One Solana obligation index, one request, ever — whichever chain it was
  bound for (chain-scoped pre-check + `ux_bridge_requests_solana_obligation`).
- One Robinhood obligation, one request (`ux_bridge_requests_obligation_source`
  + `folded_request_id`).
- `SolToRhn` contract request id = `keccak(domain ‖ 0x01 ‖ 0x03 ‖ contract ‖
  chain ‖ program_id ‖ obligation_index ‖ row_id)` — never collides with a
  Goldcoin-funded payout; the contract consumes it once.
- `RhnToSol` release: the `DepositClaim` PDA is keyed on the deposit's own
  `(tx_hash, log_index)`, recorded on the request row as
  `(source_txid, source_vout)`; the release claim binds exactly those.
- Completion for `SolToRhn` binds the EVM payout tx hash and block; a dropped
  completion is re-submitted past the grace window and settles from the
  obligation's terminal status if the signature aged out — the same recovery
  `SolToGlc` has.

## 10. Signer-deployment note (not a contract change)

The Solana release and completion claims carry no source-chain field. A custody
domain that independently resolves a claim's `(txid, vout)` or payout id must
resolve it against the chain the request's direction names (Robinhood for
`RhnToSol` releases, the Robinhood receipt for `SolToRhn` completions). The
in-tree EVM KMS binary can serve `SolToRhn`/`RhnToSol` only when
`GLC_RHN_SIGNER_ALLOWED_ROUTES` names them; its default set is unchanged.

## 11. Schema v27

Widens three CHECKs (`bridge_requests.direction` to six spellings,
`robinhood_transactions.route` to four and its payout/route agreement to both
outbound routes, `route_admission.route_id` to four), seeds the two admission
rows OPEN, and adds `onchain_completion_signature`/`onchain_completion_submitted_at`
to `robinhood_transactions`. Idempotent; changes no behaviour on upgrade.

Numbered v27 because `main` shipped v26 first (PR #81–#83: the route-less
`TreasuryWithdraw` operation, which rebuilds `robinhood_transactions` with a
nullable `route`). v27's `from` strings are v26's exact DDL, and the
`route IS NULL OR …` arm of both widened constraints is carried through, so a
withdrawal still has no route and `ux_robinhood_tx_rebalance` survives the
rebuild — pinned by `upgrading_from_v26_widens_the_three_checks_and_keeps_everything`.

## 12. Reconciliation: the in-flight term retires at the debit

`Ledger::pending_destination_settlement_amount` explains an observed balance
drop by requests whose destination transaction is on-chain but whose value the
cached book has not yet been debited for. The two cross routes are the first
directions that stay in `DestinationConfirmed` *after* that debit (they wait
there for the source-side close-out), so a `DestinationConfirmed` row is
counted only where the destination reserve is debited at `Settled` — the two
Goldcoin-bound routes, whose vault is UTXO-reconciled
(`Direction::destination_debited_at_destination_confirmed`). Without this, a
`SolToRhn`/`RhnToSol` request would have explained a second, genuine loss of
its own size for as long as its close-out took. The predicate is pinned against
the ledger's actual behaviour for all six directions, and the regression is
pinned on both reserves, in `reconciliation::tests`.

## 13. Files changed

`ledger/{types,schema,mod,robinhood_tx}.rs`, `reconciliation/tests.rs`, `routes.rs`, `chains/{mod,robinhood}.rs`,
`fees.rs`, `fees/edit.rs`, `config.rs`, `solana/{indexer,refund}.rs`,
`signing/attestation.rs`, `orchestrator.rs`, `robinhood/{preflight,auth,fold,
settlement,refund,daemon,governance,governance_session,admin}.rs`,
`signing/evm_kms/config.rs`, `signing/evm_governance.rs`, `api.rs`,
`admin_api.rs`, `bin/glc-bridge-daemon.rs`, `bin/glc-admin.rs`, plus tests.
No file under `programs/`, `shared/` or `contracts/` changed.
