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

---

## 2026-08-14 — Phase 0/1: Goldcoin/Solana chain plumbing and reserve ledger

New `service/` workspace (own Cargo workspace root, per ADR-0001 convention repeated from the old bridge — its async/networking dependency graph must stay independent of the on-chain SBF build; root `Cargo.toml` gained `exclude = ["service"]`). Modules: `goldcoin/` (RPC client, indexer, deposit extraction), `solana/` (RPC client pinned to `finalized` commitment, account decoders, obligation-count-driven indexer), `ledger/` (reserve accounting + `bridge_requests` state machine, SQLite via rusqlite), `reconciliation/`.

**Scoping decisions made this phase, documented as they were made (per the "safest option, document, continue" instruction):**

1. **Canonical cross-chain identifiers, refined from docs/06-schema.md during implementation.** For the Goldcoin leg, the OP_RETURN binding encodes `bridge_requests.id` (a `u64` LE, first 8 of 32 bytes) rather than the recipient pubkey the old bridge used — a deposit must satisfy a *specific pre-existing reservation* under the reserve model, and a recipient-only binding would be ambiguous under concurrent requests to the same recipient. For the Solana leg, `WithdrawalObligation.index` is the canonical identifier (not a transaction signature): the PDA address is fully determined by the index, so obligations are discovered by comparing `BridgeConfig.obligation_count` against a locally persisted cursor and fetching the resulting PDA range directly — no `getSignaturesForAddress`/`getTransaction` history parsing needed. Schema updated accordingly: `bridge_requests` has both `(source_txid, source_vout)` and `source_obligation_index` unique-guard columns, one populated per direction.
2. **Bug caught before it shipped**: `BridgeRequest.recipient` was initially typed as a fixed `[u8; 32]`, copying the Goldcoin→Solana leg's shape. The Solana→Goldcoin leg's recipient is a variable-length (up to 64-byte) ASCII Goldcoin address, which a fixed 32-byte field would silently truncate. Changed to `Vec<u8>` before any dependent code was written against the wrong shape.
3. **Solana→Goldcoin has no pre-reservation correlation, by construction of the already-shipped program.** `deposit_to_reserve` (Phase 2) takes no reservation-id parameter, so unlike the Goldcoin leg, a Solana deposit cannot be matched to a pre-existing `AwaitingDeposit` request. The ledger instead folds every newly observed `WithdrawalObligation` retroactively: if capacity is available at fold time it's reserved and committed directly to `SourceFinalized` (Solana finality is a single instant at `finalized` commitment, unlike Goldcoin's depth ramp — so there is no `Confirming` phase for this leg either); if not, the deposit is still recorded (never dropped — it's real and irreversible) in `ManualReview` with capacity untouched. This is an honest gap relative to the "reserve first" ideal for this direction specifically. **Follow-up worth considering**: add a reservation-id parameter to `deposit_to_reserve` in a future program revision so this leg can get the same pre-reservation guarantee the Goldcoin leg has; out of scope for this phase (chain plumbing/ledger only, not a program redesign).
4. **Reconciliation classifies only `WithinTolerance`/`Breach` in this phase**; `InFlightExplained` is defined but not yet reachable — subtracting known in-flight settlement amounts before classifying a balance delta requires the settlement/broadcast tracking a later phase (signing clients, Phase 4) will add. Until then, any unexplained balance drop beyond a configured tolerance is conservatively treated as a breach, which is the fail-closed direction to be wrong in.
5. **Reconciliation only ever pauses, never auto-unpauses** (docs/09-runbook.md's asymmetric design) — verified by test, including that a pause survives a restart and a "recovered" balance reading does not clear it.
6. **Dev/test environment note, not a design issue** (continues the note from Phase 2): the same rustc/cargo version mismatch required `cargo +nightly test` for this workspace too; `service/Cargo.lock` needed no additional precise-pinning beyond what Cargo resolved automatically once the shared crate's transitive graph was already stable — no `litesvm`/SBF-adjacent dependencies are pulled into this workspace.
7. **No real Goldcoin or Solana node exercised this phase.** No `goldcoind` binary is available in this sandbox, and no live Solana cluster was used — all indexer logic is tested against trait-based mocks (same pattern the old bridge used, `GoldcoinRpc`/`SolanaRpc` traits with mock implementations), which exercise the tick/reorg/state-machine logic exhaustively but not real wire behavior. Real-node acceptance testing (Goldcoin v0.17.0-beta1 regtest + local Solana validator) remains Phase 6 per docs/11-testing-plan.md, unchanged from the standing plan.

**Verification**: `cargo +nightly test` in `service/` — 77 tests pass, 0 failed (62 unit tests across `goldcoin`/`solana`/`ledger`/`reconciliation` modules; 8 adversarial integration tests — replay rejection via UNIQUE constraints, concurrent-shaped reservation races, insufficient-reserve fail-closed, reconciliation breach containment without reversing committed state, late-deposit-after-expiry never silently credited, invalid-transition assertions; 7 restart-recovery integration tests using a real file-backed SQLite database — reservation/deposit-observed/reorg/Solana-fold/reconciliation-pause state all verified to survive a full process drop-and-reopen with no duplication and no loss). `cargo +nightly fmt --check` and `cargo +nightly clippy --all-targets` both clean. No production keys, no real chain endpoints, no mainnet interaction anywhere in this phase (constraint 9).

---

## 2026-08-14 — Phase 3: Goldcoin vault construction and payout building

Added to `service/`: `goldcoin::address` (base58check, P2PKH/P2SH codec), `goldcoin::tx` (raw transaction serialization, txid, legacy `SIGHASH_ALL`), `goldcoin::vault` (P2SH `M`-of-`N` multisig redeem script/address), `goldcoin::multisig` (partial-signature verification, scriptSig assembly), `goldcoin::coin` (deterministic UTXO selection, fee sizing), `goldcoin::payout` (payout planning + pre-signing conservation verification), and `signing::goldcoin_vault` (internal-custody signing client with independent re-derivation). Ledger schema extended (v2 migration, exercising the versioning machinery for the first time) with `vault_utxos`, `goldcoin_payouts`, `goldcoin_payout_inputs`, and the `bridge_requests` state machine now drives all the way to `Settled` for the Solana->Goldcoin direction.

**Research discipline**: before writing any address/script code, dispatched a research fork to extract exact byte-level facts (address version bytes, base58check algorithm, redeem-script opcodes, scriptSig assembly, coin-selection strategy, `vault_utxos` schema) from the old bridge's real-node-verified implementation, including the exact golden-vector redeem script/address pair (`QY9YcpypWD91BEZ37TjNHYoqrquhcnVBYV`) reused verbatim as a test vector. One research-fork mishap this session: an initial fork attempt returned a placeholder instead of its findings; caught immediately (the result was obviously not a report) and corrected by properly resuming the same fork via `SendMessage` rather than accepting the empty result — worth noting since it's the kind of failure that's easy to silently paper over.

**Bugs caught by the test suite before they shipped** (documented per the "test it" instruction — these are exactly why the tests exist):

1. **Base58 encode/decode mishandled all-zero-value input.** Both `base58_encode` and `base58_decode` initialized their big-number accumulator with a `[0]` placeholder digit that survived unmodified (and got emitted as a spurious extra character) whenever the encoded value was genuinely zero — e.g. encoding a single zero byte produced `"11"` instead of `"1"`, and decoding `"1"` produced `[0, 0]` instead of `[0]`. Caught by `base58_round_trips_arbitrary_bytes` covering an all-zero input. Fixed by initializing both accumulators empty. This would have produced a subtly wrong (extra leading zero byte) address or hash for any real hash160 value with enough leading zero bytes — a genuine correctness bug, not a style issue, caught before any address was ever derived from it.
2. **`vault_utxos`'s CHECK constraint was too strict for the `Spent` state.** `mark_goldcoin_payout_completed` marks spent UTXOs `Spent` while deliberately leaving `reserved_by` set (an audit fact: which request spent this outpoint) — but the original constraint (`(state = 'Reserved') = (reserved_by IS NOT NULL)`) required `reserved_by` to be NULL whenever `state != 'Reserved'`, rejecting exactly this. Caught immediately by the full-lifecycle integration test. Fixed by relaxing the constraint to only enforce the direction that matters (`state != 'Reserved' OR reserved_by IS NOT NULL`).

**Scoping decisions:**

1. **Payout transactions are built directly by this crate**, not via the Goldcoin RPC's `createrawtransaction`/`decoderawtransaction` round-trip the old bridge used. Once `goldcoin::tx::Transaction` exists with its own serialization, building locally is simpler and lets `verify_payout_tx` run against a transaction this crate fully controls before any RPC call — the old bridge's reasoning for going through the node (avoiding reimplementing serialization) no longer applies once that serialization exists for sighash computation anyway.
2. **The wire-format byte-order reversal between "display order" (used everywhere else in this crate) and "internal order" (required for raw transaction serialization) is implemented per standard Bitcoin/Litecoin-lineage convention, but is NOT independently verified against a real Goldcoin node in this environment** (none available). Flagged prominently in `goldcoin::tx` module docs as the one fact in this phase that most needs real-node confirmation before Phase 6 acceptance testing — unlike the address version bytes and script opcodes, which came from the old bridge's own real-node-verified golden vectors.
3. **Dev/test signing key posture, continued from Phase 2**: `signing::goldcoin_vault::DevVaultSigner` holds a plain in-memory secp256k1 key, explicitly documented as non-production. No HSM/KMS integration in this phase (docs/12-management-decisions.md item 2 remains open).
4. **`IndependentPayoutSource` re-derives from the same shared `Ledger`** every dev-harness signer uses, rather than from genuinely separate per-signer data sources — an honest simplification of the production design (where each custody domain would have its own Goldcoin RPC connection and ideally its own chain-state replica), documented explicitly in the module so it's never mistaken for real custody-domain independence. The *mechanism* (never accept a handed-in plan; only ever reconstruct one from source facts) is real and tested; the *data source* behind it is shared in this dev harness.
5. **No real Goldcoin node broadcast was exercised** — same standing gap as Phase 0/1, unchanged. `goldcoin::rpc` gained `list_unspent`/`import_vault`/`send_raw_transaction` wrappers (typed, matching the old bridge's real-node-verified quirks: `solvable` not `spendable` as the vault-UTXO filter, `-27`/`-25` broadcast-code normalization) but they are not called by any test in this phase — only the pure construction/verification/signing logic is exercised, against mocks and a real SQLite database.

**Verification**: `cargo +nightly test` in `service/` — 141 tests pass, 0 failed (120 unit tests, up from 77 with the addition of address/tx/vault/multisig/coin/payout/signing modules; 8 adversarial; 6 new full-lifecycle integration tests in `tests/goldcoin_payout_lifecycle.rs` covering the complete `SourceFinalized -> SettlementAuthorized -> DestinationSubmitted -> DestinationConfirmed -> Settled` path with exact 1:1 accounting, restart recovery at every step, idempotent broadcast/completion replay, and vault-UTXO double-spend prevention across a restart; 7 restart-recovery from Phase 0/1, still passing unchanged). `cargo +nightly fmt --check` and `cargo +nightly clippy --all-targets` both clean. On-chain workspace (Phase 2) re-verified still passing (83 tests). Old repo re-verified untouched. No production keys, no real chain endpoints, no mainnet interaction (constraint 9).

---

## 2026-08-14 — Phase 4: attestation signer group and orchestrator

Added to `service/`: `solana::ed25519` (ed25519-precompile instruction builder, self-referential offsets matching `programs/glc-reserve-bridge/src/verification.rs`'s parser), `solana::instructions` (hand-built `release_from_reserve`/`record_goldcoin_completion` encoders — exact discriminator/account-order match against the on-chain program source, no `anchor-lang` dependency introduced), `solana::confirm` (bounded transaction-confirmation polling, reused design from the old bridge's ADR-0030), `signing::attestation` (the internal ed25519 2-of-3 attestation signer group), and `orchestrator` (the tick loop wiring every prior module together end to end for the first time). `solana::rpc`'s `SolanaRpc` trait gained `get_latest_blockhash`/`send_transaction`/`get_signature_status`/`is_blockhash_valid`; `goldcoin::indexer`'s `GoldcoinRpc` trait gained `send_raw_transaction` (both already existed as inherent/wrapped methods — this phase is what first needed them mockable/genericized, matching the existing trait+mock discipline). Ledger schema v3 migration adds on-chain-completion tracking columns to `goldcoin_payouts` (`mined_height`, `onchain_completion_signature`, `onchain_completion_submitted_at`, `onchain_completed_at`), and the `Solana->Goldcoin` completion step is now split into `record_goldcoin_completion_submitted`/`mark_goldcoin_completion_confirmed` so a request can never reach `Settled` on this service's own say-so alone — only once the threshold-attested `record_goldcoin_completion` transaction is independently confirmed on Solana. Two new ledger methods, `record_release_submitted`/`mark_release_confirmed`, give the Goldcoin->Solana leg the equivalent state tracking (`bridge_requests.destination_txid` now also carries a 64-byte Solana signature for this direction, alongside its existing 32-byte-Goldcoin-txid use for the other direction).

**Design decisions made this phase:**

1. **Attestation, like vault signing, is independent re-derivation, never "sign what you're handed."** `signing::attestation::independently_attest_release`/`independently_attest_completion` reconstruct the canonical claim message from two genuinely separate reads every time: this service's own `Ledger` (its own confirmed observation of source-chain state) and a *live* `SolanaRpc` read of `AttestationKeySet`/`BridgeConfig`/`WithdrawalObligation` — epoch, reserve mint, and destination commitment are never cached or passed in, always fetched fresh. `independently_attest_completion` additionally cross-checks the on-chain obligation's `amount` against this service's own recorded payout amount and refuses (`ObligationAmountMismatch`) rather than attesting on disagreement — the same "never trust a single source" posture applied to the Solana side that Phase 3's `IndependentPayoutSource` already applied to the Goldcoin side.
2. **The orchestrator holds no threshold authority of its own.** It never produces a signature or attestation itself — it only sequences calls into the independent signer group (`attestation_threshold`-of-N calls to `signing::attestation`, `vault_threshold`-of-N calls to `signing::goldcoin_vault`) and submits whatever they jointly produce. This is a structural property, not a comment: there is no code path in `orchestrator.rs` that constructs a valid `release_from_reserve`/`record_goldcoin_completion`/payout transaction without first collecting that many independent signatures.
3. **Every chain-touching step is submit-then-poll across separate tick phases, never a single blocking call.** Earlier design drafts considered calling `solana::confirm::confirm_transaction` synchronously inside the settlement step; rejected in favor of the same poll-loop discipline the Goldcoin/Solana indexers already use, so one tick never blocks for up to a confirmation deadline waiting on one request while others sit idle. `solana::confirm` itself remains built and tested for a future direct-use case (e.g. an operator CLI) but the orchestrator doesn't call it.
4. **Per-request failure is isolated to that request, not the tick.** Every orchestrator phase loops over its eligible requests and collects failures into `TickReport.errors` rather than propagating the first error via `?` — a bad attestation, a broadcast conflict, or a malformed request stops only that request's progress this tick (retried next tick); it never stops other requests, other directions, or other phases (indexers, reconciliation, expiry) from running in the same sweep. Verified directly by a test where a reconciliation breach pauses the Solana reserve and reservation expiry still runs in the same tick.
5. **Goldcoin-reserve reconciliation is deliberately NOT wired this phase.** `reconciliation::reconcile` (built in Phase 0/1, never previously called by anything) is now driven for the Solana reserve every tick, using the reserve authority's SPL token account balance as the live observed balance — a clean, already-available read. The Goldcoin side needs a live vault UTXO scan (`RpcClient::list_unspent`, which already exists but isn't part of the mockable `GoldcoinRpc` trait), which is a real but separable piece of work; wiring it in without a mock/test path would mean shipping an untested reconciliation branch, so it's left as an explicit gap (`tick_reconciliation`'s doc comment) rather than faked.
6. **A crash between building and broadcasting a Goldcoin payout is a known, bounded gap.** `tick_goldcoin_payouts` skips any `SourceFinalized` request that already has a `goldcoin_payouts` row in ANY state (including a stuck `Built`/`Signed` left by an interrupted prior attempt) rather than guessing whether it's safe to resume or rebuild — consistent with "never silently retry something that might double-spend," but it does mean such a request needs operator attention rather than self-healing. Documented in `build_and_broadcast_payout`'s call site rather than papered over with a partial resume mechanism this phase didn't have time to make correct.
7. **Test harness bug caught before it shipped**: the first version of the orchestrator integration tests gave the Goldcoin/Solana sub-indexers and the orchestrator's own `Ledger` handle three *separate* `open_in_memory()` databases — every test still passed, because none of them happened to depend on indexer-folded state, but it silently meant the tests were not exercising the real production wiring (in production, the indexers and the orchestrator MUST share the same underlying database for a folded deposit to ever become visible to settlement). Caught on review before committing, not by a failing test — fixed by opening three independent connections onto the same temp-file-backed SQLite database (`Ledger::open`, WAL mode), which is the same "concurrent operators, not one shared in-process handle" concurrency model this ledger was already designed around, applied here in-process. Worth flagging explicitly since a green test suite did not catch this on its own.
8. **Dev/test key posture, continued from Phase 2/3**: `signing::attestation::DevAttestationSigner` holds a plain in-memory ed25519 `Keypair`, and the orchestrator's transaction-fee-payer `submitter` keypair is likewise a plain dev key generated at construction — both explicitly documented as non-production stand-ins. No HSM/KMS integration in this phase (docs/12-management-decisions.md item 2 remains open). No production/mainnet funds, keys, or infrastructure anywhere in this phase (constraints 8, 9).

**Verification**: `cargo +nightly test` in `service/` — 165 tests pass, 0 failed (144 unit tests, up from 120, including 3 new orchestrator integration tests covering the full Goldcoin->Solana release settlement across two ticks, the full Solana->Goldcoin payout-to-completion settlement across three ticks with exact 1:1 accounting, and a reconciliation breach that pauses the Solana reserve without aborting the rest of the tick; 8 adversarial and 7 restart-recovery from earlier phases, still passing unchanged; 6 `goldcoin_payout_lifecycle` integration tests updated in place for the new `record_goldcoin_completion_submitted`/`mark_goldcoin_completion_confirmed` split and `update_goldcoin_payout_confirmations`'s new `tip_height` parameter, still exercising the same restart/idempotency/double-spend properties). `cargo +nightly fmt --check` and `cargo +nightly clippy --all-targets -- -D warnings` both clean. On-chain workspace (Phase 2) untouched (`git status` clean on `programs/`, `shared/`, root `Cargo.toml`/`Cargo.lock`). Old repo re-verified untouched. No production keys, no real chain endpoints, no mainnet interaction anywhere in this phase (constraint 9).

---

## 2026-08-14 — Phase 5: operations (health/metrics, glc-admin, glc-audit)

Added `service/ops` (`metrics`, `indexer_status`, `reserve_health`, `health`, `collector`, `audit`) and two binaries, `service/src/bin/{glc-admin,glc-audit}.rs`. Ledger schema v4 adds `attestation_records` (frozen canonical attestation-claim message bytes + hash, captured at attestation-collection time) and `signature_grant_log` (signer-identity-only audit trail — both specified in docs/06-schema.md since Phase 0/1, unimplemented until this phase actually needed them). `orchestrator::Orchestrator` now freezes every collected release/completion message via `Ledger::record_attestation` and logs a `signature_grant_log` entry per contributing signer, and tracks per-indexer liveness (`ops::indexer_status::IndexerStatus`, updated from the same `TickOutcome`/`SolanaTickOutcome` every tick already produces) exposed via new `goldcoin_indexer_status()`/`solana_indexer_status()` accessors.

**Research discipline**: before writing any of this, dispatched a research fork to extract concrete facts from the old bridge's `relayer/src/ops/*`, `relayer/src/bin/{glc-admin,glc-audit}.rs`, and `relayer/tests/runbook_commands.rs` — exact file shapes, function signatures, the `PRAGMA integrity_check`/recompute-and-diff audit mechanism, and precisely which pieces are federation/mint-burn-specific versus reusable chain-agnostic mechanics. One process note: the fork's first turn returned only a stray internal remark instead of its actual findings (the same class of mistake logged in Phase 3's entry); caught immediately and corrected by resuming the same fork via `SendMessage` rather than accepting the empty result or spawning a new one, which would have lost its already-loaded file context.

**Design decisions made this phase:**

1. **`ops::health`/`ops::metrics`/`ops::indexer_status` are ported near-verbatim** (docs/01-reuse-inventory.md class A: hand-rolled Prometheus registry/encoder and health/metrics HTTP separation, chain-agnostic) — including the metrics module's documented regression test for the `as i64`-cast saturation bug the old bridge's own mutation testing caught (`1e20` rendering as `9223372036854775807`). `ops::health`'s invariant list is rewritten around this bridge's reserve model: `{goldcoin,solana}_reserve_invariant`/`_active` (wrapping `Ledger::check_invariant`/`is_paused`, not a wrapped-supply solvency formula), `no_manual_review_backlog`, and `goldcoin_indexer_not_halted` (the Solana indexer has no halt concept, so it only ever contributes gauges, never an invariant).
2. **`ops::indexer_status` closes a real, pre-existing blind spot, not a hypothetical one.** Before this phase, `orchestrator::Orchestrator::tick`'s `TickOutcome`/`SolanaTickOutcome` were per-call and discarded — nothing tracked whether an indexer was still making progress between ticks, the exact gap the old bridge's own operational history shows caused a halted-but-invisible indexer once. Wiring required zero changes to `goldcoin::indexer::Indexer`/`solana::indexer::SolanaIndexer` themselves: the orchestrator already receives every tick's outcome and now just also updates a shared `Arc<IndexerStatus>` from it.
3. **`ops::collector::OpsCollector` opens a fresh, independent `Ledger::open` connection per scrape** rather than sharing the orchestrator's own `&mut Ledger` — the same "concurrent operators, not one shared in-process handle" concurrency model already used throughout this ledger (WAL + `BEGIN IMMEDIATE`), applied here so a `/health` HTTP handler never needs write access to (or contention with) the orchestrator's tick loop. Reserve balances reported are therefore "as of the last reconciliation tick," not a fresh live chain read performed by the collector itself — documented explicitly in the module so it's never mistaken for a live figure.
4. **`ops::audit`'s recompute check is narrower than the old bridge's, on purpose, not by oversight.** The old bridge's `StoredClaim` persisted every scalar field a claim message was built from (including its validator-set epoch) and recomputed the *entire* message from them. This bridge's `attestation_records` persists only the frozen message bytes + hash, and the audit extracts and cross-checks only the fields that legitimately cannot change after the fact (txid/vout/amount/recipient for a release; obligation index/payout txid/height/amount for a completion) against `bridge_requests`/`goldcoin_payouts`'s current values — it deliberately does NOT re-verify the attestation-epoch or reserve-mint bytes embedded in a release message, because both are fetched live from Solana at attestation time and an attestation-key rotation legitimately changes the epoch afterward; re-deriving and comparing against *current* chain state would manufacture false positives on every rotation. This is a real, narrower, honestly-documented scope than a naive "port the whole recompute function" would have produced.
5. **`glc-admin` is a deliberately small, real subset of the old bridge's 24-subcommand CLI**, not a stub. Built: `status`, local ledger `pause`/`unpause` (this service's own admission gate, independent of the on-chain pause), `show-config` (live `BridgeConfig` decode), and `onchain-pause`/`onchain-unpause` (submits the admin-gated-immediate `set_paused` instruction — new `solana::instructions::set_paused`/`PauseScope` encoder, byte-for-byte matched against `programs/glc-reserve-bridge/src/instructions/admin.rs`). Not ported: the old bridge's staged multi-operator governance-approval commands, which depended on a P2P gRPC+mTLS transport between operator processes that this bridge has no equivalent of and does not need — this bridge's governance/attestation actions are already verified on-chain via the same ed25519-precompile path as settlement, so any custody-domain operator can sign the same action bytes out-of-band and one of them submits the bundle directly; no network is required. That simpler replacement is itself not yet built. Also not ported: mint/bootstrap subcommands (no mint in this design) and the Goldcoin vault sweep-to-fresh-vault compromise-response procedure (no on-chain/vault support built yet). All three gaps are named explicitly in `docs/09-runbook.md`'s new "Executable commands" section rather than left implicit.
6. **`docs/09-runbook.md` updated to match reality, not aspiration**, including rewriting its pre-existing "Rebalancing procedure" section (which referenced a `glc-admin rebalance-plan` command that has never existed — the underlying `rebalance_deposit`/`rebalance_withdraw` on-chain instructions remain Phase 2 scoping decision #2, still open) to state plainly that no procedure exists yet, rather than leaving a doc/binary mismatch for `runbook_commands.rs` to catch as a bug. `service/tests/runbook_commands.rs` ports the old bridge's doc/binary-consistency-check discipline: every `glc-admin <subcommand>` the runbook names must exist in the binary's dispatch match and vice versa, plus a test asserting the runbook keeps stating its own unbuilt-procedure gaps rather than silently claiming completeness.
7. **Dev/test key posture, continued**: `glc-admin onchain-pause`/`onchain-unpause` take an operator-supplied keypair file via `--keypair` — no key material is generated, stored, or defaulted by this phase's code. No HSM/KMS integration (docs/12-management-decisions.md item 2 remains open). No production keys, no real chain endpoints exercised, no mainnet interaction anywhere in this phase (constraint 9).

**Verification**: `cargo +nightly test` in `service/` — 212 tests pass, 0 failed (188 unit tests, up from 165, including full coverage of every new `ops::*` module — metrics rendering/escaping/saturation-regression, indexer-status halt/reorg/freshness semantics, reserve-health invariant/pause reporting, health-report invariant construction and status codes, collector scrape-time behavior including an unopenable-database 503-empty path, and audit self-consistency/field-mismatch detection via direct raw-SQL tampering of a temp-file-backed database; 8 adversarial, 6 payout-lifecycle, 7 restart-recovery from earlier phases unchanged; 3 new `runbook_commands` doc/binary-consistency tests). Both `glc-audit`/`glc-admin` binaries build and were smoke-tested directly (`glc-audit` against a fresh empty database: clean, exit 0; `glc-admin status`/`pause` against fresh and unconfigured databases; `glc-admin --help`; an invalid `--direction` correctly rejected with exit 1). `cargo +nightly fmt --check` and `cargo +nightly clippy --all-targets -- -D warnings` both clean. On-chain workspace untouched (`git status` clean on `programs/`, `shared/`, root `Cargo.toml`/`Cargo.lock`). Old repo re-verified untouched. No production keys, no real chain endpoints, no mainnet interaction anywhere in this phase (constraint 9).

---

## 2026-08-29 — Admin control plane (admin audit log, authenticated admin API, admin console)

Two commits on `feat-admin-control-plane`, based on main AFTER PR #43 (3% fee / 20,000 GLC per-transfer), PR #44 (fee-policy snapshots), and PR #45 (recipient ATA provisioning) — none of whose behavior this work touches:

1. **`admin_audit_log` schema migration** (originally v14; renumbered v15 when PR #47 took v14 upstream) — — append-only, per-attempt (refusals included), schema-level `CHECK`s on non-empty actor/action/note and the outcome enum; `Ledger::append_admin_audit`/`list_admin_audit` (keyset pagination, actor/action filters, limit clamped to 200). Closes the gap where pause/admission notes were last-write-wins fields and on-chain command notes were println-only.
2. **`service/src/admin_api`** — the authenticated admin listener (docs/27-admin-control-plane.md): separate `service.admin_bind_addr`, per-operator bearer tokens named by env var (`service.admin_operators`, remote-signer secret discipline, SHA-256 constant-time verify, Debug-redacting token type), bearer-only/cookie-free with outright rejection of Cookie/Origin-bearing requests. Holds no keys, never touches `crate::signing`, no execution path. UI-executable mutations reuse existing `Ledger` logic only — local pause/unpause, admission close/open (open path extracted verbatim into the shared `admin_api::guard::open_admission_guarded`, now the single implementation for both the CLI and HTTP), `resume_manual_review_sol_to_glc` called as-is, and the rebalance workflow. On-chain admin actions stay CLI-only with the operator-held keypair; `POST /cli-command` generates the exact `glc-admin` command (server-side GLC→6dp-atomic conversion, live old→new preview, placeholder RPC URL/keypair path, a drift-guard test against `glc-admin`'s dispatch table). The compile-time fee is exposed read-only (`GET /fee`) — no mutation route exists, per docs/20's new "Staged fee-change process (proposal)" section, which also records that any future rate must be appended to `HISTORICAL_FEE_BPS` so PR #44's snapshot validation keeps accepting it.

Docs in the same branch: docs/27 (new), docs/09-runbook.md "Admin API & admin UI" section plus a dated note that `rolling_volume_limit` is now 500,000 GLC/24h per direction on-chain (live `BridgeConfig` always authoritative), docs/06-schema.md entry, docs/20 fee-rate history + staged-process proposal, docs/21 dated addendum, api.rs module-doc sentence recording the deliberate boundary change, pilot config template block.

Separately: a new `glc-solana-reserve-bridge-admin-ui` repository (operator console; Next.js, server-side token-holding proxy, zero browser-held secrets, per-mutation confirmation modal with mandatory note, "UI executable" vs "CLI approval required" labeling; all limits/quota figures rendered from live API reads, never hardcoded).

**Verification** (after rebasing onto post-#45 main): `cd service && cargo +nightly fmt --check && cargo +nightly clippy --all-targets && cargo +nightly test` all green; root workspace `anchor build` + `cargo test` green; admin console `npm run verify` (typecheck + vitest + production build) green.

---

## 2026-09-02 — Reserve-withdrawal hardening (post-incident)

Branch `security/reserve-withdrawal-hardening`. Response to the reserve
withdrawal of 2026-09-02, in which an unauthorized operator with access to
an authenticated production shell used the legitimate `rebalance_withdraw`
workflow — pause, withdraw to an arbitrary destination, unpause — with
genuine admin signatures and genuine 2-of-3 attestations. Full analysis:
docs/29-reserve-withdrawal-hardening.md.

**The root cause was not cryptographic.** `rebalance_withdraw` required two
factors, and both were reachable from one host: the admin keypair file, and
the bearer tokens for the attestation signer endpoints. The signers were
blind oracles — `POST /v1/sign` took opaque bytes and a token, so the 2-of-3
threshold reduced to possession of two secrets from one filesystem. Given
that single effective factor, nothing bounded the result: the destination was
any token account of the reserve mint, no per-withdrawal or velocity limit
applied to this path, and the only cap (`protected_minimum`) is removable by
the same admin key.

**On chain.** New `RebalancePolicy` PDA holding a treasury-destination
allowlist, a dedicated per-withdrawal limit, and a dedicated rolling limit
with its own window; governed by threshold attestation plus (for every change
after creation) the governance timelock, never by the admin key.
`rebalance_withdraw` is retired — it returns `RebalanceWithdrawRetired`
before touching state — and is replaced by `treasury_withdraw` (exact-match
allowlist, both limits, `policy_version` bound into the claim) and
`refund_withdraw` (destination structurally DERIVED from the obligation's own
requester via `associated_token::authority`, amount must equal the obligation
exactly, obligation must still be `Pending`). Two new claim families,
`0x05`/`0x06`, each with a unique length. The refund nonce namespace
(`Ledger::SOLANA_REFUND_NONCE_DOMAIN`) is now enforced on chain rather than by
convention.

**Design decisions made this round:**

1. **The policy is its own PDA, not new `BridgeConfig` fields.**
   `BridgeConfig` has no reserved padding — the byte-layout table claimed a
   `reserved: [u8; 32]` the struct and `SPACE` never carried (corrected this
   round). Extending it would have meant reallocating a live account holding
   the bridge's entire governance state, for no benefit.
2. **The withdrawal budget's window lives inside `RebalancePolicy`, not in a
   `RollingVolumeWindow`.** `reset_rolling_volume_window` is admin-gated;
   reusing that account type would have handed a compromised admin a
   one-transaction reset of the limit that exists to contain a compromised
   admin. A test asserts the reset cannot reach it.
3. **Policy governance is threshold+timelock, matching attestation-key
   rotation, for the same reason.** An allowlist a single admin could edit is
   not an allowlist — the attacker would add their own account and then take
   the ordinary, fully-attested path. Initialization is threshold-gated but
   NOT timelocked: it can only move from "nothing permitted" to "these
   permitted", and a delay there would protect nothing.
4. **Executing a policy update does not reset the rolling window.** A
   governance change is not a budget top-up; otherwise a quorum could refill
   an exhausted budget by re-approving the policy it already has.
5. **The refund class could not use an allowlist and does not need one.** A
   depositor is a member of the public, so the destination is derived rather
   than listed — as tightly bound as an allowlist entry, without a list. The
   operator chooses which obligation to refund and nothing else.
6. **The retired instruction is a fail-closed stub, not a deletion.** Stale
   tooling and replayed pre-upgrade transactions get an error naming their
   replacement rather than an opaque `InstructionFallbackNotFound`, and
   `tests/incident_replay.rs` can present the exact transaction shape that
   used to succeed and prove no funds move. The rejection is the first
   statement in the handler, so the nonce it names is not burned.
7. **The CLI was renamed, not just changed.** `glc-rebalance-withdraw-solana`
   → `glc-treasury-withdraw`, with `--destination` removed outright.
   An operator with muscle memory gets "command not found" and reads the
   runbook, which beats a familiar command that now behaves differently.
8. **`signing::policy` ships the signer-side decision, not the signer.** This
   crate never holds signing keys and the HTTP shim stays each custody
   domain's own process — but the claim parser and the policy decision must
   be identical everywhere, so those ship here with 19 tests, including the
   incident payload being refused. `docs/28-signer-policy.md` is the operator
   companion. **Action-scoped credentials (the daemon's token authorizes
   settlement only) is the single change that closes the incident path even
   with a fully compromised host, and it requires no code from this
   repository.**

**Deliberately not changed**, each for a stated reason rather than by
oversight (docs/29 §7): `set_limit(ProtectedMinimum, 0)` stays
admin-immediate (settlement-path change, out of scope — and it now buys an
attacker nothing, asserted by test); pause stays admin-immediate (explicit
instruction; a policy decision, not a hardening one); the off-chain
`rebalance_requests` dual-control workflow is still not on the execution path
(needs a schema migration); refunds are still not once-only on chain (needs
`WithdrawalStatus::Refunded`, which changes a wire value four off-chain
decoders match on); `transfer_admin` still has no timelock (explicitly out of
scope).

**Verification**: root workspace `anchor build` + `cargo test` — 208 tests
pass, 0 failed (73 lib, and 12 incident-replay / 23 treasury-withdraw / 15
refund-withdraw / 19 rebalance-policy / 4 retirement tests new this round;
every pre-existing suite passes unmodified, which was the acceptance bar for
"no existing invariant weakened"). `shared/` 33 tests. `cd service && cargo
+nightly test` — 1007 tests pass, 0 failed (840 lib including 19 new
`signing::policy`, 30 `glc-treasury-withdraw`). `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` clean in both workspaces. **No
deployment, no production keys touched, no production state modified — the
migration in docs/29 §6 and RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md has not
been performed.**

