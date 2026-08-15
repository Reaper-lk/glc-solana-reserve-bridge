# Management Decisions Required Before Implementation

Genuine decisions only — not routine engineering choices this document set has already resolved. Each item names the options, the recommendation where one exists, and what's blocked until it's answered.

## 1. Trust / authorization model — the central decision

> **STATUS: APPROVED 2026-08-14.** Option 6 is locked for the current implementation: program-enforced Solana-side reserve release, authorized by an internal 2-of-3 threshold-signed attestation across three genuinely separate signing/custody domains, HSM/KMS-backed in production, paired with M-of-N Goldcoin reserve custody. Independent verification/re-derivation before signing is required; no single operator or single hot key may release reserves. **This is an internal threshold custody model, not third-party or inter-organizational federation** — see the status banner in [02-trust-model.md](02-trust-model.md). Revisiting this decision requires a new, explicit management decision.

**Question:** Who verifies source-chain deposits, who authorizes destination reserve releases, who controls each reserve's signing keys, and how is compromise of that authority contained?

**Options considered:** See full comparison in [02-trust-model.md](02-trust-model.md) — (1) single operator/hot key, (2) single operator with HSM/KMS-policy-gated key, (3) internal multisig custody with centralized deposit observer, (4) true multi-organization federation [ruled out by prior management statement], (5) program-enforced Solana leg as an architectural layer, (6) **approved**: program-enforced Solana leg + internal 2-of-3 threshold-signed attestations across genuinely separate custody domains, paired with an internal multisig Goldcoin vault, preserving independent re-derivation discipline without inter-organizational federation.

**Unblocks:** All of [03-architecture.md](03-architecture.md) instruction design, [07-implementation-plan.md](07-implementation-plan.md) Phase 2 onward, and the specific containment claims in [10-threat-model.md](10-threat-model.md).

## 2. Custody-domain count and composition

> **PARTIALLY RESOLVED:** threshold size approved as 2-of-3 across three domains for both the attestation-signer group and (as a starting point) the Goldcoin vault ([12](12-management-decisions.md) item 1). **Still open:** which specific cloud accounts / HSM vendor(s) / personnel constitute each of the three domains for this organization, and whether Goldcoin vault sizing should grow beyond 3 signers at higher volume. Development/testing uses non-production key material (dev HSM simulators or equivalent local key stores, never production KMS/HSM instances) until this is resolved — see the implementation/decision log.

This is an organizational/operational question this document cannot fully answer on its own — it depends on which infrastructure and personnel the operator actually has available to hold keys with real independence from each other.

**Blocks:** Production key-ceremony planning and production deployment. Does **not** block development/testing implementation, which proceeds against a documented dev-only stand-in threshold scheme (see decision log).

## 3. Program upgrade authority

Named explicitly in [10-threat-model.md](10-threat-model.md): an upgradeable Solana program whose upgrade authority isn't held under the same custody discipline as the reserve keys undermines every on-chain control this design relies on. The old bridge left this open (custody item #5) through its entire documented life. **This should not repeat that outcome.**

**Options:** (a) upgrade authority under the same threshold-custody scheme as reserve keys, (b) upgrade authority revoked entirely after a stabilization period (immutable program — maximally safe, zero flexibility for bug fixes), (c) timelocked upgrade authority (any change visible and delayed before taking effect, giving operators a window to react to a malicious or erroneous upgrade).

**Recommendation:** (c) as an interim state, moving toward (a) or (b) once the program has enough real-world running time to be trusted stable — but this is a judgment call for management, not asserted here as settled.

**Blocks:** `initialize` instruction design, mainnet deployment planning.

## 4. Confirmation/finality depths (per chain, per direction)

Goldcoin deposit confirmation depth, Goldcoin payout confirmation depth, Solana finality assumptions. The old bridge deliberately shipped with zero defaults here (owner decision, since Goldcoin's PoW hashrate makes reorg depth a real risk requiring actual data, not a guess). This design inherits that discipline rather than picking numbers.

**Recommendation:** Do not set final values until real Goldcoin regtest/testnet hashrate and historical reorg data (or a conservative published community consensus for this chain) is reviewed with whoever owns Goldcoin infrastructure operationally.

**Blocks:** Cannot finalize [11-testing-plan.md](11-testing-plan.md)'s real-node acceptance run parameters, cannot set production config.

## 5. Reserve sizing parameters

`target_reserve`, `warning_reserve`, `critical_reserve`, `protected_minimum`, per direction, per [05-reserve-accounting.md](05-reserve-accounting.md) and [09-runbook.md](09-runbook.md). Requires actual expected volume data (or a launch-phase conservative estimate) that only management/product has visibility into.

**Blocks:** Cannot finalize reserve-ledger config schema defaults (schema itself is not blocked — only the values).

## 6. Rate limits: per-transfer and rolling-volume caps

Both a security control (bounds a drain, per [10-threat-model.md](10-threat-model.md)) and a product decision (affects legitimate large-transfer UX). Needs a joint security/product call, not an engineering default.

**Blocks:** Program `RollingVolumeWindow` parameterization, [11-testing-plan.md](11-testing-plan.md) rate-limit test values.

## 7. Reservation TTL and rebalance cadence policy

`T_reserve_expiry` (how long a user has to complete their source deposit before losing their reservation) and whether rebalancing is scheduled or threshold-triggered. Affects UX (TTL too short frustrates slow depositors; too long ties up capacity) and ops workload (rebalance cadence).

**Blocks:** Ledger sweeper implementation defaults (implementation itself proceeds; only tuning is blocked).

## 8. Refund/compensation process for `Failed` requests

Named in [04-state-machines.md](04-state-machines.md): if a request becomes permanently unpayable after a real, irreversible source-chain deposit was received (e.g., malformed destination address discovered too late), the bridge cannot auto-refund cross-chain. What's the actual process — manual operator-initiated refund on the source chain? A support/claims process? This is a product/support-operations decision, not an engineering one.

**Blocks:** `Failed`-state resolution procedure in the runbook; not blocking for core settlement-path implementation.

## 9. External security audit scope and timing

The old bridge never started one across its entire documented life. This design's real-node acceptance testing ([11-testing-plan.md](11-testing-plan.md)) is not a substitute for independent review of the Solana program, the attestation-verification logic, and the Goldcoin multisig mechanics, especially given the Solana→Goldcoin direction's weaker (non-cryptographic) replay guard.

**Recommendation:** Scope and schedule this once Phase 2–4 ([07-implementation-plan.md](07-implementation-plan.md)) code exists and before any production-funds decision — not deferred indefinitely the way the old bridge's version was.

**Blocks:** Production launch, not implementation start.

## 10. Existing Solana GLC token program assumptions

This design assumes the existing Solana GLC token is a standard SPL Token Program mint (not Token-2022, no transfer hooks/fees) based on the old bridge's precedent, but this was not independently re-verified against the *current* live GLC mint as part of this engagement (the old bridge worked with its own wrapped mint, not the existing token). **This should be confirmed against the actual live mint address before program design is finalized**, since Token-2022 extensions (transfer fees, transfer hooks, confidential transfers) would materially change the reserve-transfer instruction design.

**Blocks:** `release_from_reserve`/`deposit_to_reserve` instruction finalization ([07](07-implementation-plan.md) Phase 2).

**Resolved 2026-08-14** (docs/17-p1-checkpoint.md §1-6, docs/18-token-2022-support.md):
independently re-verified read-only against mainnet. The assumption above
was wrong — the live mint is Token-2022
(`Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump`,
`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`), 6 decimals, carrying only
`MetadataPointer`/`TokenMetadata`. Token-2022 support was subsequently
built as a dedicated, reviewed piece of work (docs/18) rather than folded
into routine execution, per this item's own flagged risk — the program now
supports either legacy SPL Token or Token-2022, structurally pinned per
configured reserve, with an explicit extension allowlist re-checked on
every reserve-touching call.

---

**Summary for immediate attention:** items 1 and 3 are the two decisions that most shape irreversible architecture (trust model, upgrade authority) and should be resolved before Phase 2 begins. Item 10 is a quick verification, not a deliberation, and should happen immediately regardless of the others' timeline. The remainder are parameter/policy decisions that can be resolved in parallel with Phase 0–1 implementation without blocking it.
