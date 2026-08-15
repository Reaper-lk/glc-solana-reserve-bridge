# Executive Summary — Reserve Bridge Architecture

**Status:** Draft for management review. No code, no commits, no repository changes beyond this documentation set have been made.

## What this document set is

This is the initial architecture package for the new Goldcoin ↔ Solana **reserve-backed** bridge, produced per the lead-architect brief. It covers, as separate documents:

| Doc | Content |
|---|---|
| [01-reuse-inventory.md](01-reuse-inventory.md) | Component-by-component REUSE / MODIFY / REMOVE / REPLACE / NEW audit of the old bridge (`glc-solana-bridge`) |
| [02-trust-model.md](02-trust-model.md) | Comparison of authorization models and a recommendation — **the central open question** |
| [03-architecture.md](03-architecture.md) | Target-state architecture: flows, modules, chain-side design |
| [04-state-machines.md](04-state-machines.md) | Bridge-request state machines for both directions |
| [05-reserve-accounting.md](05-reserve-accounting.md) | Reserve/capacity/reservation accounting model |
| [06-schema.md](06-schema.md) | Database schema |
| [07-implementation-plan.md](07-implementation-plan.md) | Module-by-module changes and phased PR sequence |
| [08-migration-strategy.md](08-migration-strategy.md) | How code moves from the old repo into this one |
| [09-runbook.md](09-runbook.md) | Operational / rebalancing runbook |
| [10-threat-model.md](10-threat-model.md) | Threats and mitigations, mapped to the safety invariant |
| [11-testing-plan.md](11-testing-plan.md) | Test plan, including real-node acceptance criteria |
| [12-management-decisions.md](12-management-decisions.md) | **The decisions only management can make** |

## Headline findings

1. **The old bridge is a strong donor codebase, not a template.** Its Goldcoin RPC client, indexer/reorg engine, database ledger discipline, transaction-construction code (Goldcoin-side payout builder, coin selection, address codec), reconciliation/solvency pattern, health/metrics scaffolding, and operational tooling (CLI shape, audit discipline, rehearsal-as-CI) are largely chain-mechanics or ops-discipline code that doesn't encode a mint/burn or federation assumption. That work should be reused with modification. See [01](01-reuse-inventory.md).

2. **Everything that encodes "mint," "burn," or "federation" must be replaced.** The wrapped-SPL-mint instructions, the `ValidatorSet` epoch PDA, the ed25519 multi-signature aggregation, the gRPC/mTLS peer federation, and the multi-relayer work-assignment/adoption layer exist to solve problems (trust-minimized minting, distributed consensus on deposit facts) that a reserve-backed, single-operator bridge does not have in the same form. Reusing them would silently re-import federation.

3. **A reserve does not verify anything by itself — this is the crux.** The old bridge's most valuable transferable idea is not federation itself, but the *discipline underneath it*: never trust a request's description of the world, independently re-derive it before authorizing value movement. We recommend preserving that discipline while dropping the *inter-organizational* federation it was built to serve. See [02-trust-model.md](02-trust-model.md) for the full comparison and recommendation: **program-enforced release on the Solana leg, authorized by an internally-threshold-signed attestation (HSM/KMS-backed, 2-of-3, single operator, diverse custody domains) — not third-party federation.**

4. **The two chains are not symmetric.** Solana has smart contracts; Goldcoin (a Litecoin-lineage UTXO chain) does not. The safety invariant ("one confirmed deposit → at most one payout") can be enforced *on-chain* for Goldcoin→Solana settlements (a PDA-based claim guard, exactly like the old bridge's replay prevention). It **cannot** be enforced on-chain for Solana→Goldcoin settlements, because Goldcoin has no program layer — that direction's replay guard lives in the bridge's own database and vault-signing discipline, which is a strictly weaker guarantee. This asymmetry is treated as a first-class fact throughout this document set, not smoothed over.

5. **No production parameters are chosen here.** Confirmation depths, reserve floors, per-transfer limits, rolling volume limits, and rebalancing thresholds are configuration, not code — consistent with the old bridge's explicit decision never to hardcode them. Defaults proposed in this package are placeholders for management/ops sign-off, not recommendations to ship as-is.

## What is explicitly NOT decided yet

Per the brief, the trust/authorization model is recommended but not chosen unilaterally. [12-management-decisions.md](12-management-decisions.md) lists every decision that gates implementation start, headlined by: **who signs settlement attestations, and how many independent custody domains are required.** Implementation should not begin until that decision is ratified.
