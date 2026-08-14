# Migration Strategy

There is no running production system and no live data — "migration" here means **code migration** from the old repo into this one, not data migration. This document defines how that code movement should happen safely, given the instruction that federation/mint assumptions must not be copied merely because they already exist.

## Principle

Nothing is copied wholesale. For every file classified **A** or **B** in the [reuse inventory](01-reuse-inventory.md), the process is: copy into the new workspace, then immediately review against this design's assumptions (no mint, no burn, no federation, existing SPL token) before it is treated as done — not copy-then-trust. For every file classified **C**, it is not copied at all. For **D**, only the file's *shape* (module boundaries, error handling patterns) informs the new implementation; the file itself is not copied. For **E**, there is nothing to migrate — it's written fresh.

## Process per component

1. **Copy** the source file(s) into the corresponding new-repo path (per [07-implementation-plan.md](07-implementation-plan.md)'s workspace layout), preserving git history where practical via `git log --follow`-friendly copies (e.g. `git show old-repo-commit:path > new/path` rather than a fresh rewrite, so `git blame` in the new repo can still be traced back if the old repo is ever added as a reference remote — optional, not required).
2. **Strip** anything referencing mint/burn/validator-set/federation types, even if it compiles without them removed (dead federation-shaped fields left in "just in case" are exactly how old assumptions silently survive a rewrite).
3. **Rename** where the old name encodes the wrong concept (`mint_wrapped` → `release_from_reserve`, `WithdrawalRequest` → `WithdrawalObligation`, `ValidatorSet` → deleted, not renamed).
4. **Re-test** against this repo's own fixtures — old-repo tests are a reference for *what to test*, not something to import and assume still passes, since account layouts and instruction shapes change.
5. **Attribute in the PR description** which old-repo file(s) a given change was derived from, so a future reviewer can trace provenance without the two repos being formally linked.

## What moves with high confidence (Phase 0–1 candidates)

Goldcoin RPC client, indexer/reorg engine, deposit extraction, address codec, coin selection, chain-tracking DB tables, health/metrics scaffolding, CLI shape, docker/regtest facts. These have no mint/burn/federation coupling per the reuse inventory and can be copied and adapted with low risk of importing an unwanted assumption.

## What requires a design review before it moves (Phase 2–4 candidates)

Anything touching PDA/account design (`state.rs`), the vault/multisig mechanics, the executor/orchestrator, and the ed25519-precompile verification machinery. These are structurally sound but their *authorization semantics* depend directly on the trust model decision ([02-trust-model.md](02-trust-model.md)) — moving them before that decision is ratified risks building against the wrong key-set shape and re-doing the work.

## What does not move

Federation transport (`p2p/`, `federation.proto`), validator-set epoch PDA and rotation governance, multi-relayer assignment/adoption, wrapped-mint/token-metadata creation, mint/burn instructions. These are not adapted-and-reused; they inform the *threat model* (what federation was defending against, [10-threat-model.md](10-threat-model.md)) and are otherwise left in the old repo, which remains untouched per the operating constraint of this engagement.

## Sequencing relative to the trust model gate

Per [07-implementation-plan.md](07-implementation-plan.md), Phase 0–1 code migration can start immediately (no trust-model dependency). Phase 2 onward (program instructions, vault authorization, signing clients) should not begin migration until [12-management-decisions.md](12-management-decisions.md) item 1 is ratified, to avoid migrating code shaped around a trust model management may not select.
