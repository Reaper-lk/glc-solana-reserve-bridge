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
any token account of the reserve mint, no velocity limit applied to this
path, and the only cap (`protected_minimum`) is removable by
the same admin key.

**On chain.** New `RebalancePolicy` PDA holding a treasury-destination
allowlist — and nothing else: no per-withdrawal ceiling, no rate limit, no
rolling budget, so a single withdrawal may move the whole reserve to the
allowlisted treasury. The allowlist is governed by threshold attestation
plus (for every change after creation) the governance timelock, never by
the admin key. `rebalance_withdraw` is retired — it returns
`RebalanceWithdrawRetired` before touching state — and is replaced by
`treasury_withdraw` (exact-match allowlist, `policy_version` bound into the
claim) and
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
2. **No amount bound on the withdrawal path, deliberately.** An amount
   cap, a rate limit or a rolling budget would restrict legitimate treasury
   operations — which must be able to move the whole reserve when custody
   demands it — without adding a bound an attacker has to defeat, because
   the allowlist already fixes the only place the funds can go.
   `protected_minimum` stays the one on-chain floor; the amount check that
   matters is each custody domain's own ceiling (`docs/28-signer-policy.md`
   §3), held where a compromised bridge host cannot reach it.
3. **Policy governance is threshold+timelock, matching attestation-key
   rotation, for the same reason.** An allowlist a single admin could edit is
   not an allowlist — the attacker would add their own account and then take
   the ordinary, fully-attested path. Initialization is threshold-gated but
   NOT timelocked: it can only move from "nothing permitted" to "these
   permitted", and a delay there would protect nothing.
4. **The refund class could not use an allowlist and does not need one.** A
   depositor is a member of the public, so the destination is derived rather
   than listed — as tightly bound as an allowlist entry, without a list. The
   operator chooses which obligation to refund and nothing else.
5. **The retired instruction is a fail-closed stub, not a deletion.** Stale
   tooling and replayed pre-upgrade transactions get an error naming their
   replacement rather than an opaque `InstructionFallbackNotFound`, and
   `tests/incident_replay.rs` can present the exact transaction shape that
   used to succeed and prove no funds move. The rejection is the first
   statement in the handler, so the nonce it names is not burned.
6. **The CLI was renamed, not just changed.** `glc-rebalance-withdraw-solana`
   → `glc-treasury-withdraw`, with `--destination` removed outright.
   An operator with muscle memory gets "command not found" and reads the
   runbook, which beats a familiar command that now behaves differently.
7. **`signing::policy` ships the signer-side decision, not the signer.** This
   crate never holds signing keys and the HTTP shim stays each custody
   domain's own process — but the claim parser and the policy decision must
   be identical everywhere, so those ship here with 19 tests, including the
   incident payload being refused. `docs/28-signer-policy.md` is the operator
   companion. **Action-scoped credentials (the daemon's token authorizes
   settlement only) is the single change that closes the incident path even
   with a fully compromised host, and it requires no code from this
   repository.**

**Deliberately not changed**, each for a stated reason rather than by
oversight (docs/29 §7): there is no amount cap, rate limit or rolling
budget on `treasury_withdraw` (the allowlist is the whole on-chain policy —
capping HOW MUCH would only constrain legitimate treasury operations, and
the amount check lives in each custody domain's own ceiling per
docs/28-signer-policy.md §3); `set_limit(ProtectedMinimum, 0)` stays
admin-immediate (settlement-path change, out of scope — though it is now
the only on-chain amount floor on a withdrawal, which strengthens the case
for governing it); pause stays admin-immediate (explicit
instruction; a policy decision, not a hardening one); the off-chain
`rebalance_requests` dual-control workflow is still not on the execution path
(needs a schema migration); refunds are still not once-only on chain (needs
`WithdrawalStatus::Refunded`, which changes a wire value four off-chain
decoders match on); `transfer_admin` still has no timelock (explicitly out of
scope).

**Verification**: root workspace `anchor build` + `cargo test` — 207 tests
pass, 0 failed (72 lib, and 12 incident-replay / 22 treasury-withdraw / 14
refund-withdraw / 21 rebalance-policy / 4 retirement tests new this round;
every pre-existing suite passes unmodified, which was the acceptance bar for
"no existing invariant weakened"). `shared/` 33 tests. `cd service && cargo
+nightly test` — 1049 tests pass, 0 failed, 2 ignored (real-node acceptance)
(849 lib including 19 new `signing::policy`, 29 `glc-treasury-withdraw`,
36 `glc-rebalance-policy`). `cargo fmt --check` and
`cargo clippy --all-targets -- -D warnings` clean in both workspaces. **No
deployment, no production keys touched, no production state modified — the
migration in docs/29 §6 and RESERVE_EMERGENCY_WITHDRAWAL_RUNBOOK.md has not
been performed.**


---

## 2026-09-10 — Per-route bridge fees; the rate allowlist removed

**Decision**: the bridge fee stops being one number and becomes one number
PER EXECUTABLE ROUTE, stated in the config's `[fees]` table and validated
by numeric range alone.

**Why**: two problems, one cause. First, `GlcToSol`, `SolToGlc`,
`GlcToRhn` and `RhnToGlc` are separate commercial terms and could not be
priced separately. Second — and this is what made it urgent — the single
global rate was leaking into routes it did not describe:
`POST /transfers` priced a `GlcToRhn` transfer at the Solana rate and
snapshotted that rate onto the row, `GET /quote` quoted every direction at
the Solana rate so a Robinhood quote disagreed with the request it became,
and `GET /robinhood/limits` reported the Solana rate as the fee
`robinhood::fold` charges. The Robinhood fold path was already reading
`[robinhood.policy].fee_bps`, which is precisely what made the
disagreement possible.

**Shape**: `crate::fees::RouteFees` — one rate per route, resolved once at
config load and never defaulted afterwards. `RouteFees::fee_bps(route)`
FAILS CLOSED for an unpriced executable route; there is no per-chain
default and no global constant behind it at request time. The set of
routes that must be priced is derived from `Route::ALL` +
`Route::as_direction()`, so a future executable route is priced by adding
one line to `[fees]` and nothing else, and a config that forgets it
refuses to boot. `SolToRhn`/`RhnToSol` cannot be priced at all: a fee for
a route with no settlement machinery would be a price on a path that
cannot move value, refused in the type and in the config parser.

**Backward compatibility**: `[fees]` is optional. Absent, a documented
load-time fallback reproduces the previous economics exactly — Solana
routes at the compiled-in `BRIDGE_FEE_BPS`, Robinhood routes at
`[robinhood.policy].fee_bps` — so an unmodified production config keeps
loading and keeps pricing identically. No schema migration. The fallback
runs once, at load, materialising an explicit table; after that there is
no fallback anywhere.

**Second decision, same day: `HISTORICAL_FEE_BPS` removed from every
runtime path.** The allowlist of previously-charged rates (100/600/300
bps) meant a fee was partly a code artefact — moving a route to 4%
required editing and releasing the binary for a value that lives in a
config file. It is replaced by range validation:
`amount_conversion::compute_fee_at_bps` accepts `0..=10_000` bps (above
that `net = gross - fee` underflows on `u64`, i.e. the net entitlement
would be negative), and `fees::RouteFees` accepts `0..=9_999` (at exactly
100% the route delivers nothing on every transfer). `0` is allowed and
means the route is free.

**What was given up, explicitly**: the allowlist also caught a ledger row
rewritten wholesale and consistently to a different rate. The protection
that remains — and that was always the real one —
is `verify_fee_breakdown`: stored gross, rate, fee and net must reconcile
exactly, and every settlement is built from the freshly recomputed figures
rather than the stored ones. The wholesale-rewrite case now rests on the
ledger's own access control and the audit trail, which are the same
defences already standing between an attacker with database write access
and the destination address. docs/20-bridge-fee.md records the trade-off
and marks the old "fee stays a compile-time constant" process superseded.

**Operator surface**: `glc-admin fees-show` (per-route table, plus where
each rate came from) and `glc-admin fees-set --route <ROUTE>
(--fee-percent X | --fee-bps N) --note TEXT [--execute]`, dry run by
default. The edit changes exactly one key and PROVES it: the candidate
file is reloaded by the real config parser and every other route's
resolved rate is compared against what it was, with the edit refused if
any of them moved. Timestamped backup, atomic rename, comments preserved.
The first edit on a config with no `[fees]` section creates it COMPLETE,
seeded from the rates already in force, and lists the seeded keys.
`scripts/bridge-admin.sh` wraps it route-first. A fee change needs a
daemon restart and nothing on chain: `GlcRobinhoodBridge` stores no fee —
its `Limits` struct is minimums, maximums, rolling limits and a protected
minimum, and nothing else.

**Verification**: `cd service && cargo +1.94.1 test` — 2424 pass, 0
failed, 2 ignored (real-node acceptance). `cargo +1.94.1 fmt -- --check`
and `cargo +1.94.1 clippy --all-targets -- -D warnings` clean.
`scripts/tests/bridge-admin-test.sh` — 172 pass, 0 failed. `shellcheck
--severity=style` clean on both shell files. **No deployment, no
production config or ledger touched, no production state modified.**

---

## 2026-09-10 — The local `RobinhoodReserve.paused` gate becomes reachable

**Decision**: add one audited operator command,
`glc-admin robinhood-local-pause --db PATH --paused <true|false>
--note TEXT`, for `reserve_ledger.paused` on the `RobinhoodReserve` row —
and deliberately do NOT reach that row by widening `pause`/`unpause`'s
`--direction`.

**Why**: the flag had always been READ and was never WRITABLE. It is a
term of `InboundAdmissionGates`, so it is a term of the `available`
verdict `GET /chains` publishes for `GlcToRhn` and of every
`fold_robinhood_deposit`. But both write surfaces —
`glc-admin.rs`'s `parse_reserve_direction` and the admin API's own —
parse `goldcoin|solana` and reject everything else, while
`audited_set_local_pause` and `Ledger::set_paused` underneath them have
always accepted `ReserveDirection::RobinhoodReserve`. A row left at
`paused=1` therefore closed `GlcToRhn` with the custody contract
unpaused, both `routeEnabled` flags true, a healthy signer quorum, a
holding reserve invariant and spare capacity — and no supported way to
clear it. `RhnToGlc` was unaffected throughout, exactly as its
destination reserve predicts, and that asymmetry is what made the shape
of the problem visible.

**Why a dedicated command rather than a third `--direction`**: not
because the flag is different. It is the same column, written through the
same `Ledger::set_paused`, audited in the same shape — which is why both
paths now go through one `admin_api::audited_local_pause_with` rather
than each spelling the wiring out. It is separate because UNPAUSING it is
guarded and unpausing the other two is not. `GoldcoinReserve`'s pause is
the vault-sweep and refund emergency stop whose documented recovery step
is an unconditional `glc-admin unpause`, and `SolanaReserve`'s is what
`crate::quota` engages automatically on rolling-volume exhaustion; adding
a refusal to either would change a documented production recovery path.
Adding one here changes nothing, because nothing could reach this flag
before. Widening `parse_reserve_direction` was rejected for a second
reason: it is shared with `close-admission` and `rebalance-propose`, and
neither should silently acquire a Robinhood spelling.

**Naming**: `robinhood-local-pause`, with a single
`--paused <true|false>`, matching its nearest neighbour
`robinhood-governance-pause --paused <true|false>`. This binary can set
four different things an operator calls "the Robinhood pause", so which
one is being set belongs in the command NAME, not in a flag value.

**Pausing is unconditional; unpausing is guarded.** An emergency stop is
never refused, however bad the reserve looks — the same asymmetry
`close-admission` and `route-admission-close` already have. Clearing the
flag runs, with no override: the three reserve-safety checks
`open-admission` runs, plus the availability evaluator itself re-asked
with the pause bit cleared. If a CAPACITY/liquidity gate would still
refuse `GlcToRhn`, the unpause is refused and names the gate and the
confirmed headroom. That second step is
`InboundAdmissionGates::route_blocker` — the same function `GET /chains`
and the folds call — not a restated inequality, so this command's verdict
cannot drift from what the public API publishes. A remaining OPERATOR
switch (route admission, reserve admission) is deliberately not a
refusal: those are separate, deliberately-set gates with their own
audited commands.

**One refactor, no behaviour change**: the three checks
(`check_invariant`, `check_utxo_liquidity_for_admission`,
`check_liquidity_buffer_for_admission`) were duplicated between
`guard::open_admission_guarded` and `guard::open_route_admission_guarded`
and are now one `reserve_safety_checks(ledger, reserve, refusal_prefix)`
that all three guards call. Refusal messages are byte-identical to what
each guard printed before; the prefix parameter is what preserves them.
Extracting it is the point — a new guard that ran a weaker subset is
precisely the drift this closes.

**Scope, enforced and printed**: the command writes exactly one column of
one row. It takes `--db` and nothing else — no `--config`, no
`--rpc-url`, no `--keypair`, no `--execute` — so it cannot construct an
RPC client, load an authorization key or submit a transaction. It does
not touch the contract's `depositsPaused`/`payoutsPaused` or
`routeEnabled`, `bridge_routes` enablement, `route_admission`,
reserve-wide Goldcoin admission, `GoldcoinReserve`/`SolanaReserve.paused`
or `config.toml`. `--note` is mandatory, every invocation appends an
`admin_audit_log` row (refusals included, inside the same atomic scope),
repeated calls are idempotent, and the output prints before/after, the
audit id, `Affected scope: GlcToRhn local reserve gate only.` and that
`RhnToGlc` is not controlled by this flag.

**Status surfaces**: `robinhood-status` and `robinhood-reserve` now print
a shared legend under their `paused` line naming the flag, the command
that sets it, and the four flags it is not — reading that line as "the
Robinhood leg is paused" is the misread that made this hard to diagnose.
`--help` and docs/09-runbook.md carry the same separation table.

**Deliberately NOT in this change**: the admin API's `POST /pause` still
rejects `robinhood`. The audited function is ready for a handler; wiring
it is a separate decision about the HTTP surface, not something to
smuggle in behind a CLI fix.

**Verification**: `cd service && cargo +1.94.1 test` — 2506 pass, 0
failed, 2 ignored (real-node acceptance). `cargo +1.94.1 fmt --all --
--check` and `cargo +1.94.1 clippy --all-targets -- -D warnings` clean.
New coverage: `service/tests/robinhood_local_pause.rs`, 19 tests driving
the real audited path and the real binary. **No deployment, no production
config or ledger touched, no production state modified; the new command
was never run against the live ledger.**

---

## 2026-09-10 — `GET /stats` publishes the Robinhood reserve (the third reserve)

**The gap.** `GET /stats` published `goldcoin_reserve` and
`solana_reserve` only. The Robinhood reserve had been a fully
authoritative `reserve_ledger` row since the Phase-F/G work — `glc-admin
robinhood-status`, `glc-admin robinhood-reserve` and `GET
/robinhood/reserve` all read it — but nothing on the public aggregate
endpoint exposed it, so a UI rendering "the bridge's reserves" from
`/stats` could not show the third one at all. Inherited, not introduced:
docs/30-robinhood-network-phase1.md records the deliberate Phase-1
decision to leave `/status` and `/stats` untouched, and the reserve row
arrived later without that decision being revisited.

**The change.** One additive field, `BridgeStats::robinhood_reserve`, of
a new `RobinhoodReserveStats`. Read through
`robinhood::admin::reserve_report` — the same projection the operator
commands print, not a second reading of `reserve_ledger` that could
drift from it. `available_capacity` is `confirmed_admission_headroom`,
which delegates to `Ledger::available_capacity`, so it is the same
formula the Goldcoin and Solana entries already report.

**Why it is not a `ReserveStats`.** `ReserveStats`'s fields are
non-optional, and this reserve has a state the other two do not: it may
not exist. Its row is created only when a `[reserve.robinhood]` section
is present. For a direction with no row,
`is_paused`/`available_capacity`/`settled_liquidity` all raise
`ReserveNotInitialized`, so a third non-optional `ReserveStats` built
with `?` would have turned the WHOLE endpoint into a 500 on exactly the
deployments the field was added for — taking the two working reserves
down with it. So: a `ledger_availability` discriminator plus nullable
figures, the same absent-is-not-zero encoding `GET /robinhood/reserve`
established. Never `0`, which would claim an empty reserve exists.

**Field names deliberately mirror `ReserveStats`** (`paused`,
`available_capacity`, `settled_volume_atomic`, `accrued_fees_atomic`) so
a client can feed this to the renderer it already has for the other two,
having only unwrapped the nulls.

**Settled volume was not invented.** `mark_robinhood_payout_settled`
advances `settled_liquidity_total` on the `RobinhoodReserve` row for
every `GlcToRhn` payout reaching the configured confirmation depth, and
it is read here through the identical `Ledger::settled_liquidity`
accessor the other two entries use. A test drives a real request through
the real observation/confirmation/finality/settlement transitions and
pins the non-zero figure `/stats` then reports.

**Where a fee lands, stated because the obvious reading is wrong.**
`accrued_fees_atomic` on this row holds `RhnToGlc` fees, not `GlcToRhn`
ones: a fee accrues on the reserve where it was WITHHELD, i.e. the
source side (docs/20-bridge-fee.md). A settled `GlcToRhn` payout
therefore leaves this figure at `0` while `goldcoin_reserve`'s rises —
asserted as such rather than papered over.

**Encoding.** Decimal strings in canonical 8dp atomic units via the
existing `AtomicU64`/`AtomicI64` (docs/31, table updated). The
`production_stats` fixture carries a configured Robinhood entry whose
settled volume is `2^53 + 1` — the smallest integer a JavaScript double
cannot represent — so a regression to a bare number fails loudly on the
third reserve too. The cross-DTO string guard gained the not-configured
payload and now accepts `null` alongside string: its subject is an
atomic amount arriving as a NUMBER, and a non-optional `AtomicU64` can
never be null, so nothing is weakened for those fields.

**Backwards compatible.** `goldcoin_reserve` and `solana_reserve` are
byte-for-byte what they were; a test pins their exact historical key
sets.

**Deliberately NOT in this change**: no reserve, route, admission,
contract, settlement, fee or migration behaviour; no new endpoint; no
change to `GET /reserve`, `GET /status` or `GET /robinhood/reserve`; no
`glc-admin` change. `/stats` remains read-only.

**Open item — the public UI.** The public UI's schema could not be
checked from this machine: the frontend docs/15-post-phase6-audit.md
points at (`/home/reaper/glc-solana-bridge-ui`) no longer exists here,
and the only UI checked out (`/opt/glc-bridge-admin/app`, the ADMIN UI)
does not consume `/stats`. The shape above matches the existing public
Robinhood DTO; whether the UI's `/stats` schema needs a matching field
added is unverified and must be confirmed against the UI repo. Per
docs/31's deploy order, the UI schema goes first.

**Verification**: `cd service && cargo +1.94.1 test` — 2514 pass, 0
failed, 2 ignored (real-node acceptance); 2506 before, +8 new tests. `cargo +1.94.1 fmt --check`
and `cargo +1.94.1 clippy --all-targets -- -D warnings` clean. **No
deployment, no production config or ledger touched, no production state
modified.**

---

## 2026-09-11 — Phase H: `SolToRhn` and `RhnToSol` become executable (disabled)

Both Solana↔Robinhood routes now have settlement machinery: each is one
existing inbound half joined to one existing outbound half (`SolToRhn` =
Solana deposit indexer → Robinhood payout engine under contract route `0x03`,
closed out by `record_goldcoin_completion`; `RhnToSol` = Robinhood deposit
indexer → Solana `release_from_reserve`, closed out by `executeSettlement`
under route `0x04`). Full design in docs/35-solana-robinhood-routes-phase-h.md.

**Decisions:**

1. **No contract change.** Both deployed contracts already model every
   required operation (audited in docs/35 §2); the Solana program's opaque
   destination payload, opaque `(txid, vout)` replay key and opaque payout
   record are used as the opaque values they are. Nothing under `programs/`,
   `shared/` or `contracts/` changed.
2. **Route selection on the Solana leg is by destination spelling** (`0x` +
   40 hex ⇒ Robinhood), structurally unambiguous because `0` is not base58,
   and active only when `SolToRhn` is priced — an unpriced deployment folds
   exactly as before.
3. **`Direction` gains two variants; `Route::as_direction` is total.** The
   pre-Phase-H tests pinning "unspellable" were re-pinned to "disabled by
   default on every gate", which is the invariant that actually protects
   production now.
4. **Accounting moves at destination finality, `Settled` only at source
   close-out.** Fee accrues on the source reserve (`Direction::source_reserve`).
5. **`[fees]` backward compatibility:** a cross route may be unpriced only
   while disabled; enabling one unpriced refuses to load. The no-`[fees]`
   fallback carries nothing forward for them.
6. **Route-scoped admission** (schema v27) for both cross routes, seeded OPEN;
   the settlement loop gates each cross route on its own while the Goldcoin
   pair keeps its both-or-neither rule.
7. **Both routes stay closed**: `default_enabled = false`, `bridge_routes`
   seeded `0`, every config template omits the flags, contract flags `false`.
8. **Schema numbered v27, after main's v26.** `main` merged the
   `TreasuryWithdraw` rebuild of `robinhood_transactions` (PRs #81–#83) as
   v26 while this phase was in flight; that migration is carried here
   verbatim and this phase's widening became v27, written against v26's
   exact DDL (nullable `route`, the `route IS NULL OR …` arms preserved).
9. **Reconciliation's in-flight term retires at the debit, not at
   `Settled`.** The cross routes are the first directions to sit in
   `DestinationConfirmed` after their destination reserve was debited;
   `pending_destination_settlement_amount` now counts a
   `DestinationConfirmed` row only for the Goldcoin-bound directions
   (`Direction::destination_debited_at_destination_confirmed`), so a
   routine cross-route settlement can never explain away a second, genuine
   loss of its own size. Behaviour for the four pre-existing directions is
   unchanged and pinned.

**Verification:** `cargo +nightly fmt -- --check` clean; `cargo +nightly
check --all-targets` clean; `cargo +nightly clippy --all-targets -- -D
warnings` clean; full `cargo +nightly test --no-fail-fast` on the branch
rebased onto `main` at PR #84: lib 2334 passed, bins 89 passed,
integration 220 passed (2 `#[ignore]`d real-node soak checks, unchanged),
doctests 0 — 2643 passed, 0 failed.

**Real-node acceptance (2026-09-11, `tests/cross_route_real_node_acceptance.rs`):**
both routes end to end on a real `solana-test-validator` (compiled program)
and a real `anvil` (compiled `GlcRobinhoodBridge`, governance through the
service's own 2-of-3 session), test tokens only — settlement, both
refunds, contract-disabled refusal without a nonce, reconciliation of both
reserves inside the `DestinationConfirmed` window, restart mid-flight in
each direction. Details in docs/35 §13. The rehearsal found and fixed a
pre-existing Solana refund defect: `execute_refund` bundled every signer's
signature plus the ATA creation into one transaction that, since the
2026-09-02 refund claim, exceeded the 1232-byte packet limit and was refused
by the node; `collect_attestations` now stops at the threshold and the ATA
creation is its own preceding transaction. The repository's pre-existing
`devnet_refund_rehearsal` reproduced the defect and passes with the fix. New coverage: decimal round-trips and both exactness refusals, both folds with
every park reason, settlement bookkeeping through `Settled`, cross-route
resume/refund guards, the settlement engine end to end for both routes on the
mock node (distinct authorizations from the Goldcoin pair, per-route gating,
contract-disabled refusal, revert parking), the orchestrator for the Solana
halves (`RhnToSol` fold+release binding `(tx_hash, log_index)`, `SolToRhn`
completion binding the EVM tx hash, dropped-completion recovery, classification
on/off), API quotes and `/chains`, schema v27 upgrade/idempotence, KMS and
governance route widening with unchanged defaults, the v26 -> v27 upgrade
(v26's route-less `TreasuryWithdraw` shape and `ux_robinhood_tx_rebalance`
surviving the v27 rebuild), and reconciliation on both cross-route reserves
(the in-flight term retiring at the debit; a second drop of a settled
request's size breaching; the debit predicate pinned for all six
directions).

## 2026-09-12 — Wallet uniqueness: one rolling 24-hour window per wallet, on every route

The rolling-24h rule that existed on the two Goldcoin-bound routes (a Goldcoin
recipient once per day from any inbound route; a Solana `requester` once per
day on `SolToGlc`; a Robinhood `depositor` once per day on `RhnToGlc`) now
applies to BOTH wallets of EVERY route, through one mechanism
(`service/src/ledger/wallet_window.rs`). Full operator description in
docs/09-runbook.md "Wallet uniqueness".

**Existing logic found and kept.** Three near-identical queries in
`ledger/mod.rs` (`recipient_rate_limit_blocker_created_at`,
`source_wallet_rate_limit_blocker_created_at`,
`rhn_source_wallet_rate_limit_blocker_created_at`), the shared
`RECIPIENT_RATE_LIMIT_WINDOW_SECS`/`RATE_LIMIT_EXCLUDED_STATES_SQL_IN`, the
strict-predecessor resume rule, the auto-resume filter, the refund
whitelists and `GET /recipients/{sol,rhn}-to-glc/eligibility`. All of it is
preserved in behaviour; the three queries became one
(`Ledger::wallet_window_blocker_created_at`, parameterized by chain and role),
and the three public accessors are thin wrappers over it.

**Decisions:**

1. **Scope is per wallet on its chain, across the routes sharing that chain
   in that role** — the existing destination decision applied uniformly.
   A strict superset of a per-route check; windows never pool across chains.
2. **One source column.** `bridge_requests.source_wallet` (schema v28, with
   `ix_bridge_requests_source_wallet_window`), written by every fold and by
   the Goldcoin deposit observation; backfilled from `requester` (Solana)
   and the linked non-reorged observation's `depositor` (Robinhood).
   Goldcoin-sourced rows that predate v28 stay NULL. No state, amount, note
   or reserve figure is touched by the migration.
3. **Goldcoin-sourced routes enforce the source twice.** `POST /transfers`
   accepts an optional, canonicalized `source_address` (checked and stored,
   so the window is consumed from admission; `429` with `blocked_reasons`
   and `retry_after` when busy); the indexer then traces every input's
   prevout script (`Indexer::trace_funding_wallets`, one
   `getrawtransaction` per input, tick fails on an unservable prevout) and
   `record_glc_deposit_observed_from` checks every traced wallet, plus the
   destination, against every OTHER request — parking under the explicit
   reason with the same refundable evidence an amount mismatch records.
   Input 0's wallet is recorded. The destination re-check is what makes a
   late deposit to a destination reused meanwhile park rather than pay.
4. **Reasons.** `wallet_source_24h_limit` / `wallet_destination_24h_limit`
   are the only spellings written; `recipient_rate_limited` /
   `source_wallet_rate_limited` on existing rows are recognized by every
   list (`is_wallet_window_manual_review_reason`) so no park loses an exit.
   One error variant, `LedgerError::WalletWindowActive { role, chain,
   wallet, retry_after }`, replaces the three route-specific ones.
5. **Cross-route resume re-checks the windows** (`resume_wallet_windows`,
   shared with the inbound body); a row with no `source_wallet` fails
   closed. Auto-resume keeps its Goldcoin-bound scope.
6. **API.** `GET /routes/{route}/eligibility?source=&destination=` for all
   six routes (per-leg verdicts, reasons, reopen instants; each address
   validated as its chain's type). The two older endpoints are unchanged in
   shape and vocabulary. Admin manual-review listing now reports both
   windows for every direction.
7. **Not touched:** requests 4037/4038 (`RhnToSol`, same source and
   destination inside 24h — exactly the shape this rule now parks at fold)
   are not read, rewritten or reclassified; no production database was
   opened. The new indexer test fixtures create requests at wall-clock
   time because the window is anchored on `created_at`.

**Verification:** `cargo +nightly fmt -- --check` clean; `cargo +nightly
clippy --all-targets` clean; `cargo +nightly test --no-fail-fast` — see the
PR for the final tally.
