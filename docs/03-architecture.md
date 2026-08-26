# Target Architecture

Assumes the recommended trust model in [02-trust-model.md](02-trust-model.md) unless noted; the parts marked "authority-agnostic" hold under any of the options compared there.

> Every settlement below moves the NET amount after a 6% bridge fee, not
> the user-declared gross amount — see [20-bridge-fee.md](20-bridge-fee.md)
> for the fee/accounting model this implies, layered on top of the flows
> described here without changing their shape.

## Components

```
┌─────────────────────────────────────────────────────────────────────┐
│                          Bridge Service (single operator)            │
│                                                                        │
│  ┌───────────────┐   ┌───────────────┐   ┌────────────────────────┐ │
│  │ Goldcoin       │   │ Solana         │   │ Reservation / Ledger   │ │
│  │ Indexer        │   │ Indexer        │   │ (reserve accounting,   │ │
│  │ (reorg-aware)  │   │ (finalized-    │   │  state machine, DB)    │ │
│  │                │   │  commitment)   │   │                        │ │
│  └───────┬────────┘   └───────┬────────┘   └───────────┬────────────┘ │
│          │                    │                          │            │
│          └──────────┬─────────┴──────────────────────────┘            │
│                      │                                                 │
│              ┌───────▼────────┐        ┌──────────────────────────┐  │
│              │ Orchestrator    │◄──────►│ Reconciliation /          │  │
│              │ (tick loop,     │        │ Solvency Monitor          │  │
│              │ state machine)  │        │ (continuous, auto-pause)  │  │
│              └───────┬────────┘        └──────────────────────────┘  │
│                      │                                                 │
│         ┌────────────┴─────────────┐                                 │
│         │                          │                                 │
│  ┌──────▼───────┐          ┌───────▼──────┐                          │
│  │ Attestation   │          │ Goldcoin      │                        │
│  │ Signer Group  │          │ Vault Signers │                        │
│  │ (2-of-3, HSM/ │          │ (2-of-3+, HSM)│                        │
│  │  KMS)         │          │               │                        │
│  └──────┬───────┘          └───────┬──────┘                          │
└─────────┼───────────────────────────┼─────────────────────────────────┘
          │                           │
   ┌──────▼───────────┐      ┌────────▼──────────┐
   │ Solana: bridge    │      │ Goldcoin: reserve  │
   │ program + reserve  │      │ P2SH multisig      │
   │ PDA token account  │      │ vault               │
   └────────────────────┘      └────────────────────┘
```

- **Indexers** (one per chain): authority-agnostic. Goldcoin indexer reuses old-repo reorg-walk/confirmation-depth mechanics. Solana indexer watches the reserve program's accounts at `finalized` commitment only (per old bridge's hard rule that reversible state must never trigger payout).
- **Reservation/Ledger**: new component (no old-repo analog) — owns the reserve-capacity/reservation accounting described in [05-reserve-accounting.md](05-reserve-accounting.md) and the state machine in [04-state-machines.md](04-state-machines.md). This is the system of record for "is this liquidity actually available," distinct from and reconciled against the chains.
- **Orchestrator**: reused tick-loop shape (reconcile-first, reload-and-recompute-before-acting, idempotent) from `relayer/src/orchestrator.rs`, rewritten around reserve release instead of minting.
- **Reconciliation/Solvency Monitor**: continuous background process comparing DB-expected state against both chains' observed state (see below and [09-runbook.md](09-runbook.md) for pause triggers).
- **Attestation Signer Group / Goldcoin Vault Signers**: the authority layer from the trust model decision. Drawn as separate processes to make the custody-domain separation concrete; if management selects a cheaper option from [02-trust-model.md](02-trust-model.md), this collapses accordingly — the rest of the architecture doesn't change shape.

## Flow: Goldcoin → Solana

1. **Create request.** User calls the bridge API. Ledger checks `available_capacity` on the Solana leg (see [05](05-reserve-accounting.md)); if insufficient, request is rejected before any reservation is made (never promise unavailable liquidity).
2. **Reserve liquidity.** Ledger atomically decrements `available_capacity` and creates a request row in `LiquidityReserved`, with an expiry timestamp. Concurrency-safe via row-level locking on the per-direction reserve-ledger row (see [05](05-reserve-accounting.md) for the exact mechanism).
3. **User deposits.** Bridge gives the user a Goldcoin deposit address (the reserve vault) plus an OP_RETURN recipient-binding convention (reused from old bridge, ADR verified against a real node — 32-byte Solana pubkey payload). Request moves to `AwaitingDeposit`.
4. **Independent verification.** Goldcoin indexer observes the deposit (`DepositObserved`), accumulates confirmations (`Confirming`), and on reaching the configured finality depth marks it `SourceFinalized` — chain-derived, automatic, irreversible for depths chosen to make reorg-past-finality a genuine incident rather than a routine case (see [10-threat-model.md](10-threat-model.md)).
5. **Settlement authorization.** Orchestrator assembles a canonical claim (txid, vout, NET amount after the 6% bridge fee — [20-bridge-fee.md](20-bridge-fee.md), recipient, direction) and requests attestation signatures. Each signer independently re-derives the claim from its own Goldcoin chain read and refuses on mismatch (reused `policy.rs` discipline) before signing. Once threshold is met, request moves to `SettlementAuthorized`.
6. **Release.** Orchestrator submits `release_from_reserve` to the Solana program with the aggregated attestation. Program verifies threshold signatures, checks the claim PDA doesn't already exist (replay guard — the on-chain enforcement this direction gets that the reverse direction cannot), checks live reserve-ATA balance and caps, transfers existing SPL GLC to the recipient, and creates the claim PDA atomically. Request moves to `DestinationSubmitted` then, once the transaction lands at `finalized`, `DestinationConfirmed`.
7. **Settle.** Ledger converts `reserved_liquidity` into `settled_liquidity`, request moves to `Settled`.
8. **Reconcile.** Background monitor confirms DB-recorded settlement matches both chains' observed state.

## Flow: Solana → Goldcoin

Mirror image, with the asymmetry named explicitly:

1–3. Create request, reserve Goldcoin-leg capacity, user transfers existing Solana GLC into the reserve-owned ATA via a `deposit_to_reserve` instruction (program atomically records a `WithdrawalObligation` PDA — reused pattern from `WithdrawalRequest`, ADR-0006).
4. Solana indexer waits for `finalized` commitment on the deposit transaction (chain-derived, automatic).
5. **Settlement authorization — the weaker direction.** Because Goldcoin has no program layer, there is no on-chain replay guard available here. The orchestrator writes a settlement-authorization row under a database UNIQUE constraint on the source Solana transaction signature *before* requesting Goldcoin vault signatures, and each vault-key holder independently re-verifies the Solana deposit against its own Solana chain read before signing its multisig partial. This is a database + multisig-discipline guarantee, not a cryptographic impossibility — see [10-threat-model.md](10-threat-model.md) for the residual risk and compensating controls (per-transfer caps, rolling-volume caps, anomaly-triggered pause).
6. **Release.** Orchestrator assembles the M-of-N Goldcoin payout transaction (reused `builder.rs`/`multisig.rs`), collects partials, verifies conservation (`verify_payout_tx`, reused unchanged), broadcasts, persisting signed bytes + txid *before* broadcast (reused four-layer double-pay defense from ADR-0013).
7. Once confirmed at Goldcoin's configured depth, the orchestrator submits a `record_goldcoin_completion` instruction to the Solana program (mirrors ADR-0018's rationale: on-chain status must not depend solely on the bridge's own database) — the `WithdrawalObligation` PDA is marked completed, irreversibly.
8. Settle and reconcile as above.

## Solana program (new)

Reworked from the old `programs/glc-bridge` skeleton (see [01](01-reuse-inventory.md)):

- **Accounts:** `BridgeConfig` (admin, paused flags per direction, per-transfer limits, rolling-volume window params, protected-minimum, attestation-key set, timelock params) — singleton PDA, same pattern as before. `ReserveVault` — the PDA that owns the reserve SPL token account (existing GLC mint, not a wrapped mint). `DepositClaim` — per-settled-Goldcoin-deposit PDA, seed `txid‖vout`, existence = replay guard, reused verbatim. `WithdrawalObligation` — per-Solana-deposit PDA recording a pending Goldcoin-side payout, status `Pending → Completed`, reused pattern from `WithdrawalRequest`. `PendingGovernanceAction` — singleton, reused timelock pattern for limit/threshold/attestation-key changes. `RollingVolumeWindow` — new, tracks a time-bucketed volume counter per direction for the rolling-volume-limit enforcement.
- **Instructions:** `initialize`, `set_paused` (per-direction + global), two-step admin handover (reused), `propose/execute/cancel_governance_action` (reused pattern, now governing limits/attestation-keys instead of validator sets), `release_from_reserve` (replaces `mint_wrapped`), `deposit_to_reserve` (replaces `burn_wrapped`), `record_goldcoin_completion` (replaces/extends `complete_withdrawal`), `rebalance_deposit`/`rebalance_withdraw` (new — operator-authorized reserve top-up/drain, tagged distinctly from user settlements so it can never be mistaken for one, see [09-runbook.md](09-runbook.md)).

## Goldcoin side (new, off-chain-enforced)

No program layer exists, so all enforcement is bridge-service + multisig discipline: reserve vault as P2SH M-of-N (reused `vault.rs`/`multisig.rs`), payout construction with pre-broadcast conservation verification (reused `builder.rs`), DB-persisted UTXO reservation (reused `vault_utxos`), and the settlement-authorization UNIQUE-constraint replay guard described above (new — the old bridge didn't need this because Goldcoin was only ever a *deposit* source in the old design; here it's also a *destination*).

## Reconciliation

Continuous background job (reused `solvency.rs` shape, rewritten formula) checks, for each reserve, every tick:

```
actual_chain_balance
  == protected_minimum + reserved_liquidity + available_capacity
```

with zero tolerated slack beyond in-flight settlement transactions accounted for explicitly (not folded into "explainable drift" the way the old bridge's fee-drift accounting did — see [05](05-reserve-accounting.md)). A mismatch beyond a strict, small, explicitly-itemized tolerance (in-flight fees, known pending transactions) triggers **automatic directional pause** — never silent continuation. See [09-runbook.md](09-runbook.md) for the exact pause-decision table and [10-threat-model.md](10-threat-model.md) for what each mismatch class implies.
