# Trust Model Comparison and Recommendation

> **STATUS: APPROVED AND LOCKED for the current implementation** (see [12-management-decisions.md](12-management-decisions.md) item 1). Management has approved Option 6 below: program-enforced Solana-side reserve release, authorized by an internal 2-of-3 threshold-signed attestation across three genuinely separate signing/custody domains, HSM/KMS-backed in the production design, paired with M-of-N Goldcoin reserve custody. **This is an internal threshold custody model, not third-party or inter-organizational federation.** Documentation, UI, and code comments must not describe this design as "federated" — that word is reserved in this codebase for a multi-organization trust-distribution architecture, which this is explicitly not, and which management has ruled out (see Option 4). If that ever changes, it requires a new, explicit management decision, not a documentation update.

Management has stated this bridge is **not federated**, and explicitly declined to specify what replaces federation. This document originally compared realistic alternatives against the actual requirement (a reserve does not itself prove a deposit happened on the other chain; something must verify deposits and something must authorize releases) without assuming an answer. The comparison below is retained as the record of that analysis; the recommendation it reached has since been approved.

## The asymmetry that shapes every option

Solana has a program layer; Goldcoin (a Litecoin-lineage UTXO chain, no PSBT, no covenants beyond bare multisig) does not. Consequences:

- **Goldcoin → Solana settlements** (release Solana GLC from the Solana reserve) can have their replay guard **enforced on-chain**, by a Solana program, independent of which off-chain party requested it — exactly the old bridge's `DepositClaim` PDA pattern (see [01-reuse-inventory.md](01-reuse-inventory.md)). Whoever can produce a valid attestation still needs to be trustworthy, but *double-spending the same deposit* is a program-level impossibility, not a policy the bridge service promises to follow.
- **Solana → Goldcoin settlements** (release native GLC from the Goldcoin reserve) have **no equivalent on-chain enforcement available**, because Goldcoin has no general program capability. The replay guard for this direction can only live in the bridge's own database and the discipline of whoever holds the Goldcoin vault signing keys. This is a structurally weaker guarantee no matter which trust model is chosen, and it should be named as such rather than implied away.

This means the trust model decision is really two coupled decisions: **who is authorized to produce a settlement attestation**, and **who controls the signing keys for each reserve** (Solana PDA vs. Goldcoin vault key(s)). The options below evaluate both together, since they interact.

## Options

### 1. Single trusted bridge operator (one hot key/service, no threshold)

One service process holds both the Solana reserve authority (a hot keypair, or a PDA whose only guard is "this service's pubkey signed") and the Goldcoin vault key, and both verifies deposits and authorizes releases itself.

- **Security:** Low. One process is the entire trust boundary.
- **Custody risk:** High — key material lives where the service runs.
- **Hot-key exposure:** Constant; signing capability is always live.
- **Operational complexity:** Lowest.
- **Liveness:** Highest — no coordination required.
- **Recovery:** Poor. A stolen key requires migrating both reserves to new addresses/authorities under time pressure, with no independent party to notice the compromise before funds move.
- **Auditability:** Moderate — single identity, easy to log, but nothing independently checks its claims.
- **Compromise blast radius:** 100% of both reserves, immediately, on first compromise.
- **Implementation effort:** Lowest.
- **Still federated in practice?** No — this is the opposite of federation.

### 2. Central bridge service with HSM/KMS-controlled reserve keys (single authority, policy-gated)

Same single logical authority as (1), but the signing key never leaves an HSM/KMS; the service calls a sign API subject to policy (rate limits, per-transaction caps encoded in the HSM/KMS access policy).

- **Security:** Medium. Key material can't be exfiltrated, but a compromised orchestrator with legitimate API access can still request signatures up to whatever the policy allows, repeatedly, until someone notices.
- **Custody risk:** Medium.
- **Hot-key exposure:** None (key never leaves the HSM); *signing capability* exposure remains.
- **Operational complexity:** Low–medium (HSM/KMS provisioning and policy authoring).
- **Liveness:** High.
- **Recovery:** Better than (1) — HSM key rotation is more tractable than migrating a raw hot key.
- **Auditability:** Good — HSM/KMS access logs are an independent record of every signing call.
- **Compromise blast radius:** Bounded by policy ceiling (e.g. max GLC per hour) rather than unbounded, but still a single authority that can be driven to that ceiling repeatedly.
- **Implementation effort:** Low–medium.
- **Still federated in practice?** No.

### 3. Multisig reserve custody with a centralized observer

Reserve funds (both legs) sit behind M-of-N multisig; a single service observes and verifies source-chain deposits and requests signatures from the M-of-N signers, each of whom independently verifies before signing (reusing the old bridge's `policy.rs` discipline: never trust the requester, re-derive).

- **Security:** Medium–high, and depends entirely on whether the M signers are genuinely independent custody domains or all controlled by the same operator with no real separation.
- **Custody risk:** Low–medium — compromise requires M keys, not one.
- **Hot-key exposure:** None if HSM-backed partials.
- **Operational complexity:** Medium–high (signing-ceremony coordination — this is essentially what the old bridge's Goldcoin vault already does, see ADR-0015/0017).
- **Liveness:** Medium — stalls if fewer than M signers are available.
- **Recovery:** Good — sweep-to-fresh-vault pattern is proven (old bridge rehearsed this against real nodes).
- **Auditability:** High, if signers independently re-derive rather than trust the observer.
- **Compromise blast radius:** Bounded to "M signers compromised," **except** the single centralized *observer* remains a weak link for deposit-verification correctness — if it is compromised or buggy, it can still feed false claims to signers. This risk is only closed if signers verify independently, at which point this option converges with option 6 below.
- **Implementation effort:** Medium–high, but much of the Goldcoin-side code already exists in the old repo.
- **Still federated in practice?** **Ambiguous — this is the crux of the whole comparison.** If the M signers are separate organizations, this *is* federation with a smaller N. If the M signers are the same operator's own keys held in genuinely separate custody domains (different HSMs, different cloud accounts, different on-call humans, no single person/process with access to more than one), it is **internal dual control**, a standard financial-infrastructure pattern (e.g. requiring two authorized officers to release a wire transfer) — not a trust-minimization protocol across mutually distrusting parties. Whether option 3 satisfies "not federated" depends entirely on which of these it is, and that must be stated explicitly rather than left implicit.

### 4. Multiple independent observers with threshold authorization

This is, definitionally, the old bridge's federation model: independent organizations each run their own indexer, each independently verify deposit facts, and a threshold of them must co-sign every release.

- **Security:** Highest in theory — Byzantine-fault-tolerant across independent orgs.
- **Custody risk:** Lowest per party.
- **Hot-key exposure:** Distributed; no single party holds full authority.
- **Operational complexity:** Highest — recruiting, contracting, and operationally managing independent operator organizations. The old bridge never resolved this in practice (custody decision #1, "federation composition," was open at end of life across 31 ADRs).
- **Liveness:** At risk of stalls if independent operators are slow, offline, or uncooperative; governance/coordination overhead across organizations.
- **Recovery:** Slower — rotation requires threshold + timelock coordination across parties who don't share an incident-response chain of command.
- **Auditability:** High.
- **Compromise blast radius:** Requires compromising M independent organizations — best theoretical containment of any option here.
- **Implementation effort:** Highest — this is where most of the old bridge's unresolved complexity lived.
- **Still federated in practice?** **Yes, definitionally.** Management has already ruled this out. Included only for completeness of comparison — **not a candidate.**

### 5. Program-controlled Solana reserve plus controlled Goldcoin reserve

This names an architectural *layer*, not a complete authority model: the Solana reserve is held in a program-owned PDA token account, and the transfer-out instruction is gated by on-chain verification of a settlement attestation (mirroring the old bridge's ed25519-precompile verification, but checking a small fixed key set rather than a rotating federation). The Goldcoin reserve, having no program layer, is "controlled" by whatever key/multisig scheme is chosen for it — this option is really an axis that must be combined with an authority model (2 or 3) for the actual attestation-signing decision.

- **Security:** High on the Solana leg specifically — replay, per-transfer caps, rolling-volume caps, and directional pause are enforced by code the operator cannot bypass by mistake or under duress, regardless of who signs the attestation. The Goldcoin leg's security is exactly whatever the paired custody choice provides.
- Evaluated jointly with option 6 below, since in isolation it is incomplete.

### 6. Recommended: program-enforced Solana leg + internal threshold-signed attestations (not third-party federation)

Combine the on-chain enforcement layer from (5) with **internal** (not inter-organizational) M-of-N custody from (3), and explicitly reuse the old federation's *independent re-derivation discipline* — the single most valuable idea in the old codebase — inside one operator's infrastructure rather than across separate organizations.

Concretely:

- **Solana leg:** Reserve GLC sits in a PDA-owned SPL token account. A `release_from_reserve` instruction only executes if it is accompanied by a valid signature set from a small, fixed **attestation key group** (recommend 2-of-3 to start) over a canonical message binding source chain, txid/vout, amount, recipient, and direction — reusing the old bridge's ed25519-precompile verification mechanics against a much smaller, non-federated key set. The program independently re-checks: the claim PDA doesn't already exist (replay), the live reserve-ATA balance covers the amount (no cached numbers), per-transfer and rolling-volume caps, and the directional pause flag. This is exactly option 5, made concrete.
- **Goldcoin leg:** Reserve GLC sits in an M-of-N P2SH multisig vault (recommend 2-of-3, reusing ADR-0015/0017's vault construction and payout-signing mechanics near-verbatim). Because Goldcoin cannot enforce replay on-chain, the replay guard here is a database UNIQUE constraint on the source Solana deposit signature, written in the same transaction that authorizes payout construction, plus the old bridge's four-layer double-pay defense (persist signed bytes + txid before broadcast, idempotent rebroadcast, UTXO-set as final truth). This is the direction's acknowledged weaker point — see the asymmetry note above and [10-threat-model.md](10-threat-model.md).
- **Key separation, not organizational separation:** the 2-of-3 attestation keys and the 2-of-3 (or 3-of-5, sized independently) Goldcoin vault keys are held in genuinely separate custody domains — e.g. one in a cloud KMS reachable only by the automated service under tight IAM policy, one in a hardware HSM requiring human presence to invoke, one offline/cold for emergency and rotation use only — all operated by the single bridge operator. No individual and no single automated process can reach two of the three. This satisfies "not federated" (there is one legal/operational trust root, the operator) while avoiding a single blast-radius domain.
- **Independent re-derivation, preserved:** each attestation signer (and each vault-key holder, for Goldcoin payouts) re-derives the claim from its own chain read before signing rather than trusting the orchestrator's assertion — reusing `policy.rs`'s discipline. A compromised orchestrator alone cannot manufacture a false deposit; it can at most withhold or delay legitimate ones (a liveness risk, not a safety one).
- **Deposit verification stays single-observer** (one canonical indexer, or a primary+shadow pair for fault detection rather than trust distribution) — this is what makes the design genuinely non-federated rather than federation-with-a-smaller-N. What replaces trust-distribution-via-consensus is trust-boundedness-via-cryptographic-threshold-plus-on-chain-enforcement-plus-limits.

**Evaluated:**
- **Security:** High on Solana leg (code-enforced); medium-high on Goldcoin leg (bounded by 2-of-3 custody separation, no on-chain backstop).
- **Custody risk:** Low — no single key or process is sufficient on either leg.
- **Hot-key exposure:** None; all signing keys are HSM/KMS-resident.
- **Operational complexity:** Medium — a real but bounded increase over option 1/2, most of it (Goldcoin multisig mechanics) already built in the old repo.
- **Liveness:** Medium-high — requires 2 of 3 custody domains reachable; sized to tolerate one domain's planned or unplanned unavailability.
- **Recovery:** Good — attestation-key rotation via the reused timelocked-governance pattern; Goldcoin vault compromise response is the old bridge's rehearsed sweep-to-fresh-vault procedure, repointed at internal custody.
- **Auditability:** High — HSM/KMS logs plus the reused signature-grant audit-logging pattern.
- **Compromise blast radius:** One custody domain compromised → nothing releasable (threshold not met). Two of three compromised → that leg's reserve is at risk, bounded by per-transfer/rolling-volume caps and anomaly-triggered auto-pause until operators intervene. This is the honest floor of any threshold-custody design, federated or not; naming it plainly is preferable to implying zero risk.
- **Implementation effort:** Medium — substantially lower than rebuilding federation, substantially higher than option 1/2, and the Goldcoin-side mechanics are largely reused code.
- **Still federated in practice?** **No**, provided custody domains are genuinely separated within one operator as described. If that separation is not actually implemented (e.g. all three keys reachable by the same on-call engineer with the same credentials), this degrades to option 1/2 with extra steps — the distinction is operational reality, not the architecture diagram, and should be verified, not assumed.

## Recommendation — APPROVED

**Option 6** — program-enforced Solana-side release, authorized by a small internally-threshold-signed attestation (2-of-3, three genuinely separate custody domains), paired with an M-of-N internal-custody multisig for the Goldcoin vault, preserving the old federation's independent-re-derivation discipline without its inter-organizational trust distribution.

Approved by management on 2026-08-14 as documented in [12-management-decisions.md](12-management-decisions.md) item 1. Implementation proceeds under this model. The three custody domains and HSM/KMS backing are binding requirements on the production design (see [12-management-decisions.md](12-management-decisions.md) item 2 for domain composition, still to be finalized operationally); development/testing infrastructure uses non-production key material only (see [10-threat-model.md](10-threat-model.md) and the implementation/decision log).
