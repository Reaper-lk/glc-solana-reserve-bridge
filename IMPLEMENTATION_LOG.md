# Implementation & Decision Log

Running log of implementation-phase decisions, per approval to proceed autonomously through Phase 2 and subsequent non-production phases (docs/07-implementation-plan.md). Entries are append-only, newest last. Each entry: date, phase, decision, rationale, alternatives considered where relevant.

Governing constraints for every entry below (do not repeat per-entry unless an entry specifically tests one):

1. 1 Solana GLC released requires 1 corresponding GLC locked/received on the source side.
2. Reverse transfers preserve the same 1:1 backing invariant.
3. Never release based only on a requester's claim.
4. Verify source-chain state independently.
5. Prevent replay/double-release across restarts and concurrent operators.
6. Reserve insufficiency fails closed.
7. Reorg/orphan handling fails safely.
8. Signing keys never stored in the repository.
9. No production/mainnet funds, keys, wallets, or infrastructure during development/testing.
10. Auditable reserve accounting and reconciliation preserved throughout.

---

## 2026-08-14 — Trust model approved and locked

Management approved docs/02-trust-model.md Option 6: program-enforced Solana-side release, internal 2-of-3 threshold-signed attestation across three genuinely separate custody domains (HSM/KMS-backed in production), M-of-N Goldcoin reserve custody, independent re-derivation before signing, no single-key release capability. Explicitly **not** third-party/inter-organizational federation — docs/02-trust-model.md and docs/12-management-decisions.md updated with an approval banner and instructed never to use "federated" to describe this design. Proceeding with implementation under this model.

**Dev/test key posture (per constraint 8 and 9):** all threshold signing in development and local/regtest testing uses locally-generated, non-production key material (plain Solana keypairs / local secp256k1 keys held in the dev signer process's own memory or an on-disk file explicitly excluded from git via `.gitignore`), standing in for the eventual HSM/KMS-backed keys. No real HSM/KMS integration is built or exercised in this phase — that is production-infrastructure work requiring a distinct, later decision (docs/12-management-decisions.md item 2) and is out of scope here. This stand-in is documented at every point it appears in code (module-level comments, not inline noise) so it is never mistaken for the production posture.

---

## 2026-08-14 — Phase 2: Solana program (`glc-reserve-bridge`) implemented

Workspace scaffolded (`programs/glc-reserve-bridge`, `shared`), following the old bridge's two-workspace convention (on-chain code isolated from off-chain deps — `service/` is a later phase, not yet created). Toolchain: Anchor 0.31.1, Solana/Agave 2.1.21, rustc 1.85.0 host — the same verified pairing the old repo pinned.

**Shared crate** (`glc-reserve-bridge-shared`): canonical attestation message encoding (`claim.rs`: `release_claim_message`, `goldcoin_completion_message`; `governance.rs`: rotation/cancel governance messages), adapted from the old bridge's `shared::claim`/`shared::governance` with a fresh domain tag (`GLC_RSV_CLAIM_V1` / `GLC_RSV_GOVRN_V1`, distinct from the old bridge's `GLC_BRIDGE_CLAIM`/`GLC_BRIDGE_GOVRN` — a signature from either system must never be interpretable as valid for the other). Golden-vector tests reused/rewritten to pin every byte.

**Program** (`glc-reserve-bridge`): accounts `BridgeConfig`, `AttestationKeySet` (2-of-3 minimum enforced on-chain via `validation::MIN_THRESHOLD = 2` — a threshold of 1 is a hard `ThresholdBelowMinimum` error, not just a config convention), `DepositClaim` (replay guard, GLC→SOL leg), `WithdrawalObligation` (SOL→GLC leg, no on-chain replay backstop — see docs/02-trust-model.md asymmetry), `PendingGovernanceAction`, `RollingVolumeWindow` (per direction). Instructions: `initialize`, `initialize_reserve_vault`, `set_paused` (global/release/deposit scopes), `set_limit`, `transfer_admin`/`accept_admin`, `propose/execute/cancel_attestation_key_rotation` (threshold+timelock, never admin-gated), `release_from_reserve`, `deposit_to_reserve`, `record_goldcoin_completion`.

**Scoping decisions made this phase (documented per the "safest option, document, continue" instruction):**

1. **Limit/pause changes are admin-gated-immediate, not the timelocked asymmetric-governance design** docs/03-architecture.md describes. Deliberately mirrors the old bridge's own Phase-1 posture (which it later hardened in Phase 7a). Rationale: attestation-key rotation is the one governance action that MUST never be admin-gated (it's the property the whole trust model rests on) and got the full threshold+timelock treatment; limit/pause tuning is lower-severity and was sequenced later rather than blocking Phase 2. **Follow-up required before production**: extend the timelocked-governance pattern (already built and tested for key rotation) to cover limit changes too, per docs/03-architecture.md.
2. **`rebalance_deposit`/`rebalance_withdraw` instructions are deferred**, not built this phase. `docs/05-reserve-accounting.md`'s requirement that rebalancing be structurally distinct from user settlements is therefore not yet implemented or tested (the "rebalance-vs-settlement separation" test category from the brief has no instruction to test against yet). Tracked as immediate next work.
3. **`RollingVolumeWindow` is a fixed-bucket window**, not a true sliding window — documented in `state.rs`/`limits.rs` as a conservative-but-imprecise simplification, consistent with what `docs/05-reserve-accounting.md` already flagged.
4. **Existing Solana GLC mint program (SPL Token vs Token-2022) not verified against a live address** — `initialize_reserve_vault` accepts whatever mint account is supplied; docs/12-management-decisions.md item 10 remains the open item.
5. **Dev/test environment gap, not a design issue**: the sandbox's installed SBF toolchain (Solana platform-tools' bundled cargo, 1.79) and host toolchain (rustc 1.85.0) both predate several transitive dependencies' current crates.io releases, which require newer cargo/rustc (edition2024 manifests, MSRV bumps). Resolved by precisely pinning ~15 transitive crates (blake3, indexmap, borsh, proc-macro-crate, zeroize, and their cascading dependents) to the same versions the old bridge's own `Cargo.lock` already used successfully — not arbitrary downgrades, but alignment with a proven-compatible graph for this exact Anchor/Solana version pairing. Recorded in `programs/glc-reserve-bridge/Cargo.toml` comments; the full pin set is in `Cargo.lock`. Host-side `cargo test` additionally requires running under the locally available `nightly` toolchain (`cargo +nightly test`) rather than the pinned 1.85.0 stable channel — nightly is dev/test-only, does not affect `rust-toolchain.toml`'s pin (which governs the SBF/lint build), and was already installed in this environment, not introduced. **Operator note**: `anchor build` (SBF) uses the pinned 1.85.0-adjacent toolchain via platform-tools and works as-is; `cargo test`/`cargo +nightly test` is needed for host-side unit/integration tests until this environment's rustc is upgraded past 1.88.

**Verification**: `anchor build` succeeds (`target/deploy/glc_reserve_bridge.so` produced, dev-only deploy keypair, gitignored). `cargo +nightly test --workspace` — 83 tests pass, 0 failed (45 program unit tests, 22 litesvm integration tests covering replay rejection, insufficient-reserve fail-closed, protected-minimum enforcement, per-transfer and rolling-volume limit enforcement, global/directional pause enforcement, threshold-attestation verification including unknown-signer and tampered-message rejection, attestation-key rotation timelock/threshold enforcement and post-rotation epoch invalidation; 16 shared-crate tests covering message encoding). No real HSM/KMS, no production keys, no mainnet interaction anywhere in this phase (constraint 8, 9). Old repo (`glc-solana-bridge`) re-verified untouched (clean `git status`) after this phase's work, including after an accidental-directory mishap during dependency debugging that was caught and reverted via `git checkout` before any commit.
