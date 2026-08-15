# External Security Audit — Scope Document

Status: **draft, ready to be issued to an auditor once management schedules the engagement** (docs/12-management-decisions.md item 9). This document defines what an independent external reviewer should examine, what is explicitly out of scope, and — critically — which components cannot receive a *final* sign-off until certain production decisions are made, so an auditor's report can separate "code is sound" findings from "this still needs a production parameter before it can be trusted at that value" findings.

This is a scope document, not a self-assessment: it does not claim the system is secure, only what to look at and why each area matters. Where a known limitation already exists in this codebase's own documentation, it is named here explicitly rather than left for the auditor to discover — the same “don't smooth over open risk” discipline the rest of this doc set follows (docs/10-threat-model.md).

## 1. System summary

A **reserve-backed** (not wrapped, not minted) bidirectional bridge between an existing Goldcoin (GLC) and an existing Solana Token-2022 GLC mint. Both sides of the bridge move *real, pre-existing* GLC between a Goldcoin P2SH multisig vault and a Solana program-controlled reserve token account; the system never creates, mints, burns, or wraps a token, and never modifies token supply on either chain. A bridge settlement always nets a fixed 1% protocol fee (docs/20-bridge-fee.md); nothing else is asserted about the relationship between the two chains' balances beyond 1:1 underlying GLC denomination, decimal-converted (Goldcoin: 8 decimals; Solana GLC mint: 6 decimals, live-verified — docs/18-token-2022-support.md).

Two independently deployed components make up the system:

- **On-chain Solana program** (`programs/glc-reserve-bridge/`, Anchor) — owns the Solana-side reserve token account, `BridgeConfig`, and `WithdrawalObligation` PDAs; enforces pause state, rate limits, attestation-signature verification, and the cryptographic replay guard for Goldcoin→Solana settlements.
- **Off-chain Rust service** (`service/`) — a single daemon (`glc-bridge-daemon`) plus operator CLIs (`glc-admin`, `glc-audit`) that index both chains, run the bridge-request/rebalance/custody-transition state machines against a local SQLite ledger, produce independently-signed attestations and Goldcoin vault partial signatures, reconcile observed vs. expected reserve balances, and expose a read-only HTTP API plus an operator-only health/metrics endpoint.

## 2. Component inventory (for the auditor's own navigation)

| Component | Location | Language/framework |
|---|---|---|
| Solana program | `programs/glc-reserve-bridge/src/` | Rust, Anchor |
| — instructions | `programs/glc-reserve-bridge/src/instructions/*.rs` | |
| — account state | `programs/glc-reserve-bridge/src/state.rs` | |
| — signature/attestation verification | `programs/glc-reserve-bridge/src/verification.rs` | |
| — rate limiting | `programs/glc-reserve-bridge/src/limits.rs` | |
| — Token-2022 extension allowlist | `programs/glc-reserve-bridge/src/token_extensions.rs` | |
| — program-level test suite | `programs/glc-reserve-bridge/tests/*.rs` | `solana-program-test` |
| Reserve ledger + bridge-request/rebalance/custody state machines | `service/src/ledger/` | Rust, SQLite (rusqlite), one `BEGIN IMMEDIATE` transaction per mutation |
| Goldcoin chain plumbing (RPC, tx construction, multisig, deposit detection, indexer) | `service/src/goldcoin/` | Rust |
| Solana chain plumbing (RPC, account decoding, instruction building, obligation indexer) | `service/src/solana/` | Rust |
| Attestation + Goldcoin vault signing abstractions | `service/src/signing/` | Rust — trait-based (`VaultSigner`/`AttestationSigner`), dev implementations only; see §4.9 |
| Settlement orchestration (ties indexers, signing, reconciliation together) | `service/src/orchestrator.rs` | Rust |
| Reconciliation (observed vs. expected balance) | `service/src/reconciliation.rs` | Rust |
| Rebalancing engineering layer | `service/src/rebalance.rs`, rebalance methods in `service/src/ledger/mod.rs` | Rust |
| Read-only public bridge API | `service/src/api.rs` | Rust, `hyper` |
| Operator-only health/metrics | `service/src/ops/` | Rust |
| CLIs | `service/src/bin/glc-admin.rs`, `service/src/bin/glc-audit.rs`, `service/src/bin/glc-bridge-daemon.rs` | Rust |

Supporting design documents an auditor should treat as authoritative background (not scope items themselves, but load-bearing context): docs/02-trust-model.md, docs/03-architecture.md, docs/04-state-machines.md, docs/05-reserve-accounting.md, docs/06-schema.md, docs/10-threat-model.md, docs/18-token-2022-support.md, docs/20-bridge-fee.md.

## 3. In-scope review areas

Each area below names what to review, where it lives, and the specific risk questions this codebase's own design documents already flag as worth an independent second opinion — not an exhaustive checklist, but a starting point that should not be skipped.

### 3.1 Solana program

`programs/glc-reserve-bridge/src/`, all instructions (`initialize`, `deposit_to_reserve`, `release_from_reserve`, `complete_goldcoin_payout`, `reserve_vault` init, `admin` (`set_paused`), `governance` (attestation-key rotation)). Review account validation (PDA seeds/bumps, signer checks, `has_one`/`constraint` correctness), integer arithmetic (overflow/underflow, especially in `limits.rs`'s rolling-volume accounting), CPI safety (token transfers to/from the reserve ATA), and whether every mutating instruction actually enforces the invariants `state.rs`'s doc comments claim it does.

### 3.2 Token-2022 integration

`programs/glc-reserve-bridge/src/token_extensions.rs`, `service/src/solana/accounts.rs` (mint decoding + extension allowlist), docs/18-token-2022-support.md. The live Solana GLC mint is Token-2022 (confirmed against mainnet — docs/12-management-decisions.md item 10), carrying `MetadataPointer`/`TokenMetadata` today. Review: is the extension allowlist re-checked on every reserve-touching instruction (not just at `initialize`) so a future extension change to the mint (e.g., a transfer fee or transfer hook added later) is rejected rather than silently mis-handled? Does the program correctly compute and account for any extension-imposed transfer effects (e.g., would silently break 1:1 accounting if a transfer-fee extension were ever added)? Is the legacy-SPL-Token vs. Token-2022 program ID pinned per configured reserve, not assumed globally?

### 3.3 Reserve accounting

docs/05-reserve-accounting.md, `service/src/ledger/mod.rs` (capacity/reservation logic), on-chain `state.rs`. Review the core invariant — `total_reserve_balance ≥ protected_minimum + reserved_liquidity` — for every code path that can change either side of it, including concurrent-reservation handling (SQLite's `BEGIN IMMEDIATE` as the sole concurrency guard — is that actually sufficient, or does it merely serialize without preventing a logic-level race?), reservation expiry/release, and the confirmed-rebalance balance adjustment (§3.13). Specifically check: can any code path reserve or release capacity without holding the invariant check inside the same transaction?

### 3.4 Decimal conversion

`service/src/amount_conversion.rs`. Goldcoin is 8 decimals; the Solana GLC mint is 6 decimals (live-verified, not assumed — see docs/18). Review every narrowing conversion (8→6) for exact-division-only behavior (no silent rounding that could create or destroy value across the two ledgers) and every widening conversion (6→8) for correctness. Confirm the minimum representable amount in each direction is enforced consistently between the off-chain quote/creation path and whatever the on-chain program itself would accept.

### 3.5 The 1% bridge fee

docs/20-bridge-fee.md, `service/src/amount_conversion.rs` (`compute_fee`, `BRIDGE_FEE_BPS`), `service/src/ledger/types.rs` (`RequestAmounts`), `service/src/api.rs` (quote endpoint). The fee rate must be a fixed, compiled-in protocol constant, never client-suppliable — confirm no code path (API, ledger, on-chain instruction) accepts a caller-provided fee/gross/net triple as authoritative rather than recomputing it server-side. Confirm `gross == fee + net` is enforced as an invariant everywhere fee-bearing amounts are persisted or attested, not just at creation time.

### 3.6 State machines

docs/04-state-machines.md (bridge-request lifecycle), plus the rebalance and custody-transition state machines added in this phase (`service/src/ledger/types.rs`, `service/src/ledger/mod.rs`). Review every state transition function for: correct preconditions (can a transition be forced from an unexpected source state?), atomicity (does a "crash between two effects" scenario ever leave the ledger in a state the rest of the system doesn't know how to interpret?), and whether every terminal state is actually unreachable from every other terminal state.

### 3.7 Replay protection

docs/10-threat-model.md §"Replay" (already documents the asymmetry below), `programs/glc-reserve-bridge/src/state.rs` (`WithdrawalObligation`/deposit-claim PDA), `service/src/ledger/schema.rs` (Solana-signature `UNIQUE` constraint). **Known, already-documented asymmetry the auditor should weigh most heavily**: Goldcoin→Solana replay protection is on-chain and cryptographically enforced (a PDA's mere existence is the guard); Solana→Goldcoin replay protection is a database `UNIQUE` constraint plus each Goldcoin vault signer's independent re-verification before signing — a single point of failure in the bridge service's own DB layer has no cryptographic backstop on that leg. Confirm the compensating control (independent re-derivation by ≥2 vault-signer custody domains) is actually structurally required by the signing code, not just documented as expected operator behavior.

### 3.8 Attestation

`service/src/signing/attestation.rs`, `programs/glc-reserve-bridge/src/verification.rs`, docs/02-trust-model.md. Review the canonical message construction both signers and the on-chain verifier independently re-derive (does every signer actually recompute the message from first principles — fee, amounts, recipient — rather than trusting a value already stored, per docs/20-bridge-fee.md's "never trust a stored fee" discipline?), the threshold-verification logic on-chain, and whether a signature over one action type (`release` vs. `completion`) can ever be replayed as authorization for the other.

### 3.9 Signing / custody boundary

`service/src/signing/signers.rs` (the `VaultSigner`/`AttestationSigner` trait abstraction added this phase), `service/src/signing/goldcoin_vault.rs`, `service/src/signing/attestation.rs`. **This is a boundary, not a completed custody implementation** — the traits are designed so a production HSM/KMS-backed signer can be plugged in without changing settlement logic, but only `DevVaultSigner`/`DevAttestationSigner` (in-memory, dev/test-only key material) exist today; see §5. Review: does the trait surface leak any path by which private key material could cross it (it should not — signers accept a canonical payload and return only a signature and public identity)? Is signer-call timeout/error handling actually fail-closed (a signer timeout or rejection must never be silently treated as "signed")?

### 3.10 Daemon / orchestrator

`service/src/orchestrator.rs`, `service/src/daemon.rs`. This is the component that ties indexing, signing, and reconciliation into one tick loop. Review: can a partial failure mid-tick (e.g., one of several required attestation signers times out) leave a request in an inconsistent state, or is every effect either fully applied or fully rolled back? Does a crash-and-restart mid-settlement resume correctly from persisted ledger state alone (no in-memory-only state the daemon depends on to avoid double-signing or double-broadcasting)? `service/tests/restart_recovery.rs` is the existing test suite for this property — the auditor should independently assess whether its scenarios are actually exhaustive, not just trust that they pass.

### 3.11 Goldcoin transaction construction

`service/src/goldcoin/tx.rs`, `service/src/goldcoin/coin.rs` (UTXO selection), `service/src/goldcoin/payout.rs`, `service/src/goldcoin/multisig.rs`, `service/src/goldcoin/vault.rs`. Review raw transaction serialization, sighash computation, UTXO selection (does it ever double-spend a UTXO across two concurrently-built payouts?), change-output correctness, and P2SH multisig redeem-script construction/signature ordering. This is hand-rolled Bitcoin-family transaction code (Goldcoin is a Bitcoin fork) — an auditor experienced with UTXO-chain transaction construction should independently verify sighash correctness against the protocol's own consensus rules, not just this codebase's own golden-vector tests (inline `mod tests` in `service/src/goldcoin/tx.rs` and `service/src/goldcoin/payout.rs`).

### 3.12 Reconciliation

`service/src/reconciliation.rs`, docs/05-reserve-accounting.md. Review the classification logic (within-tolerance vs. breach), whether the itemized in-flight tolerance calculation can ever be gamed to mask a real unexplained drop as an expected one, and the auto-pause-on-breach behavior (confirm it is genuinely automatic and fail-closed, and that a subsequent healthy reconciliation cycle never auto-clears a pause it didn't cause — docs/09-runbook.md's asymmetric pause discipline).

### 3.13 Rebalancing

`service/src/rebalance.rs`, rebalance methods in `service/src/ledger/mod.rs`, `service/src/bin/glc-admin.rs` (`rebalance-*` subcommands). This phase's engineering layer never mints/burns/wraps/creates tokens and never autonomously moves funds — `record_rebalance_executed` only ever records a `tx_reference` as evidence of a transfer an operator already authorized and executed through real custody tooling outside this system. Review: is that boundary structurally enforced (no code path anywhere constructs, signs, or broadcasts a fund-moving transaction on the rebalance path), or only true by convention? Confirm the confirmed-rebalance balance adjustment happens atomically with the state transition (a crash between the two would misclassify the next reconciliation tick).

### 3.14 Key rotation / vault sweep tooling

`service/src/ledger/mod.rs` (custody-transition methods), `service/src/bin/glc-admin.rs` (`custody-*` subcommands). Same non-custodial boundary as rebalancing — this tooling records evidence of a rotation/sweep executed out of band, never performs one. Review the two extra gates this state machine adds beyond rebalancing: (a) approvals cannot begin until the new identity is independently verified (`verify_new_identity` is a required precondition, not advisory), and (b) execution evidence cannot be recorded until the relevant reserve(s) are already paused (enforced in `record_custody_transition_executed`, not just documented). Confirm both are actually unbypassable, not merely the CLI's default behavior.

### 3.15 Pause / emergency behavior

`programs/glc-reserve-bridge/src/instructions/admin.rs` (on-chain `set_paused`), `service/src/ledger/mod.rs` (`set_paused`/`is_paused`, the service's own local admission gate), `service/src/goldcoin/indexer.rs` (post-finality reorg auto-pause), `service/src/reconciliation.rs` (breach auto-pause). Two independent pause layers exist (on-chain, admin-gated-immediate; and the service's own local ledger gate) — review whether they can disagree in a way that leaves a real settlement path open when an operator believes the bridge is fully paused. Confirm no automatic un-pause path exists anywhere in the codebase for any trigger (this is a deliberate, load-bearing asymmetry — verify it holds, don't just take the doc comments' word for it).

### 3.16 API security

`service/src/api.rs` (public read-only bridge API), `service/src/ops/` (operator-only health/metrics). Confirm the public API genuinely never exposes signing material, admin operations, or infrastructure detail (RPC URLs, database paths) — review every field of every response type against this claim, not just the module doc comment asserting it. Confirm client-supplied fee/amount fields in `POST /transfers` cannot influence the server-computed fee (the existing test `client_supplied_fee_fields_in_the_request_body_are_silently_ignored` in `service/src/api/tests.rs` is a starting point, not a substitute for independent review). Note there is currently **no authentication/TLS termination at this layer** by design (docs comment in `service/src/api.rs`: run behind a reverse proxy) — confirm this is an acceptable posture for the deployment model chosen, and that no endpoint that should be operator-only is reachable on the public listener.

### 3.17 Key rotation (protocol-level, on-chain)

`programs/glc-reserve-bridge/src/instructions/governance.rs` (`propose/execute/cancel_attestation_key_rotation`, timelocked). Review the timelock enforcement (can it be bypassed by any account-substitution or reentrancy-style attack?), and whether a rotation proposal in flight can be front-run or griefed by an unrelated account. Cross-reference against the off-chain custody-transition tooling in §3.14 — the two are related but distinct: the on-chain instruction is what actually changes the program's trusted attestation pubkeys; the off-chain tooling is bookkeeping/audit trail for the human process around it.

### 3.18 Upgrade authority

`programs/glc-reserve-bridge/src/instructions/initialize.rs` (upgrade-authority-gated `initialize`), docs/12-management-decisions.md item 3. **Currently unresolved as a management decision, and the auditor should treat this as a first-class finding area, not a footnote**: the program is deployed as an upgradeable BPF program; no timelock or threshold-custody control has yet been placed around the upgrade authority itself, independent of the reserve-custody scheme. An attacker (or a compromised single upgrade-authority key) who can upgrade the program can bypass every other control this document set describes. Review what — if anything — currently constrains who can upgrade the deployed program, and flag this as blocking final sign-off regardless of code quality elsewhere (see §5).

### 3.19 Threat model review

docs/10-threat-model.md in full. The auditor should independently re-derive this threat model rather than only checking the code against it — confirm no threat class is missing (e.g., MEV/front-running on Solana instructions, Goldcoin mempool-visibility timing attacks on the vault, supply-chain risk in the dependency tree) and that the stated mitigations for each listed threat actually hold under adversarial testing, not just code review.

### 3.20 Tests

`service/tests/`, `service/src/**/tests.rs`, `programs/glc-reserve-bridge/tests/`. The auditor should assess test *quality*, not just coverage: do adversarial tests actually attempt the attack they claim to defend against (e.g., real double-spend attempts, real replay attempts), or do they only exercise the happy path with an inverted assertion? Are restart/recovery tests (`service/tests/restart_recovery.rs`) genuinely simulating a crash (dropping and reopening state) rather than a clean shutdown? Is the real-node acceptance suite (`service/tests/regtest_acceptance.rs`) run against genuinely independent Goldcoin/Solana test infrastructure, not mocks dressed up as integration tests?

### 3.21 Production deployment assumptions

docs/12-management-decisions.md in full, docs/09-runbook.md. Review every place this codebase currently defers a real value to configuration rather than asserting one (reserve sizing, confirmation depths, rate limits, reservation TTL, custody-domain composition) and confirm the *mechanism* for setting each is sound (config validation, no unsafe defaults, fail-closed on a missing/malformed value) even though the *values themselves* are explicitly out of this audit's scope until management sets them (see §5).

## 4. Out of scope

- **Goldcoin and Solana protocol/consensus-layer security** — this audit is of the bridge's own code, not of the underlying chains it settles against.
- **Third-party dependencies' internal security** (Anchor, `rusqlite`, `hyper`, the Solana SDK, etc.) beyond confirming this codebase uses them correctly and pins reasonable versions — a supply-chain audit of those projects themselves is a separate engagement.
- **Physical/operational security** of wherever production keys eventually live (HSM/KMS vendor security, personnel vetting, physical access controls) — this is inherently unknowable until the custody-domain decision (docs/12 item 2) is made, and is a different kind of audit (operational/procedural, not code) in any case.
- **Frontend/UI code** — no bridge UI exists in this repository yet; `service/src/api.rs` is the read-only surface it will eventually consume.
- **Load/performance testing** — this scope is about correctness and security, not capacity planning.

## 5. Components that cannot receive final sign-off yet

An auditor can and should review the *design* and *code* of everything below today. What cannot happen yet is a final "this is safe to run in production at these values" sign-off, because the values or components themselves don't exist yet. Reviewing the code path now (with dev/test values) and re-reviewing once real values are set is the recommended approach, per docs/12-management-decisions.md's own item 9 ("scope and schedule this once Phase 2–4 code exists and before any production-funds decision").

| Item | Blocked on | Reference |
|---|---|---|
| Production signer implementation (HSM/KMS-backed `VaultSigner`/`AttestationSigner`) | Vendor selection; no HSM/KMS integration exists — only `DevVaultSigner`/`DevAttestationSigner` (in-memory dev key material) | docs/12 item 2, `service/src/signing/signers.rs` |
| Custody-domain composition (which cloud accounts/HSM vendor/personnel hold each of the 3 threshold shares) | Organizational decision, not yet made | docs/12 item 2 |
| Program upgrade authority final control | Management decision between immutable, threshold-custodied, or timelocked upgrade authority; **none of the three is currently implemented** — the program is deployed upgradeable with a single authority key today | docs/12 item 3 |
| Confirmation/finality depths (Goldcoin deposit, Goldcoin payout, Solana) | Requires real Goldcoin hashrate/reorg data review with whoever owns Goldcoin infrastructure operationally | docs/12 item 4 |
| Reserve sizing (`target_reserve`/`warning_reserve`/`critical_reserve`/`protected_minimum`, per direction) | Requires actual/estimated production volume data | docs/12 item 5 |
| Rate limits (per-transfer and rolling-volume caps) | Joint security/product decision | docs/12 item 6 |
| Reservation TTL and rebalance cadence policy | Product/ops tradeoff decision | docs/12 item 7 |
| Refund/compensation process for `Failed` requests | Product/support-operations process, not yet defined | docs/12 item 8 |

Everything else in §3 — program logic, ledger invariants, decimal conversion, fee accounting, state machines, replay protection, attestation, the signing/custody *boundary* (as opposed to a specific production implementation behind it), orchestration, Goldcoin transaction construction, reconciliation, the rebalancing and custody-transition *engineering layers* (as opposed to specific production custody executing behind them), pause behavior, the public API, and the on-chain key-rotation instruction — is reviewable in full today against the dev/test configuration this codebase already runs under, and an auditor's findings there should not be blocked on the items in the table above.

## 6. Supporting materials for the auditor

- This entire `docs/` directory, read in order for full context; docs/00-executive-summary.md is the fastest orientation.
- `IMPLEMENTATION_LOG.md` (repository root) for a chronological record of what was built when and why, useful for understanding why a given design choice was made rather than an alternative.
- Full test suites, runnable directly:
  - Off-chain service: `cd service && cargo test --all-targets` (unit tests inline per module, plus `service/tests/*.rs` integration suites — adversarial scenarios, restart/recovery, daemon smoke tests, and `regtest_acceptance.rs`, which requires real local Goldcoin regtest + Solana test-validator nodes per docs/13-phase6-readiness-audit.md).
  - On-chain program: `cd programs/glc-reserve-bridge && cargo test` (via `solana-program-test`).
  - Quality gates already enforced in this codebase and worth re-running independently: `cargo +nightly fmt --check`, `cargo +nightly clippy --all-targets -- -D warnings`.
- `service/src/bin/glc-admin.rs --help` and `service/src/bin/glc-audit.rs --help` for the full operator CLI surface, cross-checked against docs/09-runbook.md by an automated doc/binary consistency test (`service/tests/runbook_commands.rs`) — an auditor reviewing the runbook can trust it was not written and then allowed to drift from the actual binary.

## 7. Suggested engagement shape

Not a commitment, a starting point for whoever schedules this (docs/12 item 9 names this as still open): a two-track review — one reviewer/team on the Solana program (§3.1, §3.2, §3.7 on-chain half, §3.17, §3.18) given its immutability-adjacent risk profile once deployed, and one on the off-chain service (§3.3–§3.16, §3.19–§3.21) given its size. Both tracks should read docs/10-threat-model.md and docs/02-trust-model.md first, since several findings will only make sense in light of the trust model's already-accepted tradeoffs (e.g., the SOL→GLC replay-guard asymmetry in §3.7 is a known, accepted-for-now design choice, not an oversight to "discover" — the value an audit adds there is testing whether the *compensating* control actually holds, not re-flagging the asymmetry itself).
