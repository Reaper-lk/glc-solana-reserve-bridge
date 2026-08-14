# Phase 6 checkpoint: real-node acceptance rehearsal

Scope: exercise the reserve bridge end-to-end against real, isolated
regtest/local-validator infrastructure (docs/13-phase6-readiness-audit.md),
in both directions, plus adversarial and recovery scenarios. No mainnet,
production funds, production keys, or production endpoints were used at
any point (see "Isolation" below).

## Tests / scenarios executed

All three real-node tests live in `service/tests/regtest_acceptance.rs`,
gated on `GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN`/a built on-chain program
`.so`/`solana-test-validator` on `PATH` (skip-not-fail if absent — see
`support::phase6_prereqs`). Run against a real `goldcoind` v0.17.0-beta1
regtest node and a real `solana-test-validator` with this repository's own
compiled Anchor program baked into genesis.

| # | Scenario | Result |
|---|---|---|
| 1 | GLC -> Solana release, full happy path (real Goldcoin deposit, real vault import, real attestation, real on-chain `release_from_reserve`, exact recipient balance, exact settled-liquidity accounting) | **PASS** |
| 2 | Solana -> GLC payout, full happy path (real `deposit_to_reserve`, real vault UTXO sync via `list_unspent`, real signed/broadcast Goldcoin payout, real confirmation depth, exact destination regtest balance, exact settled-liquidity accounting) | **PASS** |
| 3 | Double-release / replay rejection: a second, independently-reconstructed `release_from_reserve` for an already-claimed `(txid, vout)` is rejected by the **on-chain** `DepositClaim` PDA guard (not merely this service's own bookkeeping) | **PASS** |
| 3 | Reconciliation against the real post-settlement on-chain balance: `WithinTolerance`, no breach | **PASS** |
| 3 | Crash + restart recovery: a second, independent request is carried partway through settlement, the orchestrator is dropped mid-flight (simulated crash), a fresh orchestrator is rebuilt against the same on-disk ledger and the same real nodes, and settlement completes exactly once | **PASS** |
| 3 | 1:1 accounting across both settlements + the rejected replay attempt: recipient's real on-chain balance == exactly 2x the settled amount, no more, no less | **PASS** |

Additional required scenarios, already covered by existing mock/fixture-based
integration tests (not re-run against real nodes this round, but exercised
and passing as part of the full suite below):

- **Insufficient reserve** (fail-closed at request-creation time, never
  creates a liability): `tests/adversarial.rs::insufficient_reserve_at_creation_time_never_creates_a_liability`
- **Stale/reorg**: `tests/restart_recovery.rs::pre_finality_reorg_state_survives_restart`,
  `service/src/ledger/tests.rs::pre_finality_reorg_clears_source_binding_and_returns_to_awaiting_deposit`,
  `reorg_after_finality_must_never_be_called_it_is_a_caller_bug`
- **Signer-loss** (2-of-3 threshold: any one signer can be unavailable and
  settlement still proceeds; one signer alone can never authorize anything):
  `service/src/signing/goldcoin_vault/tests.rs::two_independent_signers_produce_an_assemblable_threshold`,
  `a_single_signer_alone_cannot_reach_threshold`,
  `tests/goldcoin_payout_lifecycle.rs::a_single_signers_partial_alone_can_never_authorize_a_payout`
- **Reconciliation breach behavior** (auto-pauses new reservations, never
  reverses already-committed ones, never auto-clears):
  `tests/adversarial.rs::reconciliation_breach_blocks_new_reservations_but_never_reverses_committed_ones`,
  `tests/restart_recovery.rs::reconciliation_pause_persists_across_restart_and_requires_an_operator_to_clear`

Full regression / quality gate (run after all Phase 6 fixes):

- `cargo +nightly test --lib`: **195 passed, 0 failed**
- `cargo +nightly test` (full default suite: ledger, adversarial, goldcoin
  payout lifecycle, restart recovery, runbook commands, regtest acceptance
  gated/skipped without real-node env vars): **all passed**
- `cargo +nightly test --test regtest_acceptance` with real-node env vars
  set, single-threaded: **3 passed, 0 failed** (~148s)
- `cargo +nightly fmt -- --check`: clean
- `cargo +nightly clippy --all-targets -- -D warnings`: clean

## Bugs discovered and fixed

Real-node testing caught real defects that every existing mock/unit test
missed, because several test fixtures shared the same wrong assumptions as
the production code they were meant to check.

1. **`decode_bridge_config` misdecoded `Option<Pubkey>`** (commit `163950c`).
   Assumed a fixed 33-byte slot for `pending_admin`; Borsh actually encodes
   it variable-length (1-byte tag, payload only if `Some`). Every field
   after it — including `reserve_token_mint` — was misread against a real
   on-chain account, causing `release_from_reserve` to reject with
   `AccountNotInitialized`. Fixed the decoder and every fixture that shared
   the wrong assumption; added a regression test pinning the `Some` case.

2. **No vault-UTXO sync from a live Goldcoin `listunspent` read** (commit
   `163950c`). Nothing in the orchestrator ever populated `vault_utxos`
   from the chain — every existing test seeded it directly, masking that
   coin selection for a Solana -> GLC payout would always fail against a
   real node. Added `list_unspent` to the `GoldcoinRpc` trait and a new
   `tick_vault_utxos` orchestrator phase that syncs live vault UTXOs every
   tick before payout building runs. This was production-blocking for the
   entire Solana -> GLC direction.

3. **Reconciliation false-breach after a legitimate settlement, permanent
   one-way pause** (commit `33fb152`). `mark_release_confirmed` and
   `mark_goldcoin_completion_confirmed` updated `reserved_liquidity`,
   `pending_obligations`, and `settled_liquidity_total`, but never
   `reserve_ledger.total_reserve_balance` — that was left for the next
   reconciliation pass to refresh. Reconciliation treats any drop between
   its cached balance and a fresh on-chain read, beyond tolerance, as an
   unexplained breach and pauses the reserve direction one-way (by design,
   never auto-clears — docs/09-runbook.md). So the very next reconcile
   after a real settlement saw the real balance already down by the
   released amount while its own cache was still stale, misclassified a
   completely routine, self-caused settlement as an anomaly, and
   permanently blocked all future reservations on that direction. Fixed by
   decrementing `total_reserve_balance` in the same transaction that
   records settlement, so the cache and the real chain state converge
   immediately rather than racing the next reconcile by one cycle. Also
   reordered `Orchestrator::tick()` so reconciliation runs after the
   settlement-confirmation phases, not before, for the same reason. Added
   a focused regression test at the ledger level
   (`mark_release_confirmed_decrements_total_reserve_balance_immediately`).
   This is the most significant bug found in Phase 6: a real, silent,
   unrecoverable-without-operator-intervention denial-of-service on the
   bridge's own liveness, triggered by entirely legitimate activity, that
   no mock-based test could have caught because every mock fixture
   configured its "expected" balance to already match reality at t=0.

4. Three test-harness-only defects, fixed but not production bugs:
   `solana-test-validator`'s fixed default gossip/TVU/TPU port range
   collided with an unrelated already-running validator on the same
   machine (fixed with explicit `--gossip-port`/`--dynamic-port-range`);
   the double-release test hardcoded `vout=0` instead of reading the real
   settled request's `source_vout` (Goldcoin's `fundrawtransaction` does
   not guarantee vout 0 — docs/goldcoin-rpc-notes.md), which made the
   "replay" instruction target a fresh, never-claimed output and falsely
   pass; and the real regtest node's relay policy rejected the test's fee
   rate as "insufficient priority" for a transaction spending freshly
   generated (low-priority) inputs.

5. Bootstrap funding in the real-node tests was awaited only at
   `confirmed` commitment while reconciliation reads at `finalized`, which
   lags `confirmed` by roughly 32 slots on a fresh single-node validator.
   Configuring the ledger's baseline balance / starting the orchestrator
   before that lag elapsed let reconciliation observe the pre-funding
   (zero) balance and misclassify it as a breach — a second, distinct way
   to trigger bug 3's symptom, from a startup race rather than a
   settlement race. Fixed with `support::wait_for_finalized_balance`,
   used before configuring the reserve in both real-node tests that fund
   it. This is a test-sequencing bug (a real deployment runbook would
   never start the service before verifying funding is finalized), noted
   as an operational hardening opportunity below.

## Commits (Phase 6, this repository, local only — nothing pushed)

- `6ce0ec8` — Phase 6 readiness audit: local infra, isolation, acceptance matrix
- `163950c` — Phase 6: both directions settle end-to-end on real nodes (bugs 1, 2)
- `33fb152` — Phase 6: real-node double-release, crash/restart, reconciliation coverage (bugs 3, 4, 5)

## Real-node evidence

- Real `goldcoind` v0.17.0-beta1 regtest node per test (own datadir, own
  RPC port, coinbase-matured spendable coin, real `importaddress`-based
  vault, real `fundrawtransaction`/`signrawtransaction`/`sendrawtransaction`
  deposits with request-binding `OP_RETURN` outputs).
- Real `solana-test-validator` per test (own ledger dir, randomized
  gossip/dynamic-port range, this repository's own compiled
  `glc_reserve_bridge.so` baked into genesis via `--upgradeable-program`,
  no deploy transaction or signature from the program's own authority
  ever required).
- Real on-chain program instructions exercised: `initialize`,
  `initialize_reserve_vault`, `deposit_to_reserve`, `release_from_reserve`
  (including a manually-signed adversarial replay attempt), attestation
  proof verification (ed25519 program), `DepositClaim` PDA replay guard.
  `record_goldcoin_completion` is exercised by the mock-based integration
  suite but not separately re-verified against a real node this round
  (its account/data shape was already pinned by unit tests in
  `service/src/solana/instructions.rs`, and it sits downstream of the
  already-real-node-verified Goldcoin payout path).
- Both directions settled with exact 1:1 accounting confirmed by reading
  real balances back from the real nodes (SPL token balance via
  `get_token_account_balance`, Goldcoin balance via
  `getreceivedbyaddress`), not just the ledger's own bookkeeping.

## Isolation confirmation

- All Goldcoin nodes: fresh regtest datadirs, `regtest=1`, loopback RPC
  only, thrown away (`tempfile::tempdir`) at test end.
- All Solana validators: fresh ledger dirs, explicit
  `http://127.0.0.1:<port>` URLs everywhere in this repository's code —
  the Solana CLI's global config (which defaults to `mainnet-beta`) is
  never read by any code path here. Randomized gossip/dynamic-port ranges
  avoid colliding with the separate, pre-existing, untouched `fed2`
  rehearsal environment for the old bridge.
- No production keys, no production Solana `$GLC` mint, no mainnet
  endpoint, no real GLC, no real SOL/SPL tokens touched anywhere in this
  phase.
- Nothing pushed, no PR opened, no merge, no deploy.

## Remaining gaps

- **Reconciliation has no cold-start grace period.** By design (module
  docs in `service/src/reconciliation/mod.rs`), *any* unexplained drop
  from the ledger's configured baseline is treated as a breach, with no
  exception for "we just started and haven't observed reality yet." Bug 5
  above shows this can misfire on a race between when funding lands and
  when reconciliation first runs, not just on genuine anomalies. The fix
  applied here (wait for finality before configuring the baseline) is the
  correct operational discipline for a real launch runbook, and is now
  enforced in the test harness — but the underlying design has zero
  margin for any other cold-start race that might exist and hasn't
  surfaced yet. Worth a deliberate design decision before launch: either
  keep the current zero-tolerance design and mandate the fund-then-verify-
  then-start sequencing in the runbook (docs/09-runbook.md already
  documents the asymmetric pause design; it should explicitly call out
  this startup-ordering requirement), or add a narrow, explicit
  first-observation exemption. Not fixed this round — flagged for an
  explicit decision rather than a hasty design change under time
  pressure.
- **Signer-loss and `record_goldcoin_completion`** are covered by existing
  mock/unit tests but not independently re-verified against real nodes in
  this phase (see "Real-node evidence" above). Lower priority per the
  explicit acceptance-priority ordering given for this phase, but worth
  closing before a production candidate declaration if time allows.
- **Single-node validator only.** No multi-validator, no real network
  latency/partition behavior, no actual Goldcoin peer-to-peer propagation
  (single node, no peers) exercised. Real testnet/devnet rehearsal (still
  non-production) would be the natural next step before any mainnet
  conversation.
- **No load/soak testing.** All scenarios are single- or dual-request;
  no concurrent-load, sustained-throughput, or long-duration soak run has
  been performed against real nodes.

## Security / custody gaps

- No new custody-model gaps identified this phase. The 2-of-3 internal
  threshold custody model (approved earlier) held under real-node
  adversarial testing: a single signer alone could not authorize a
  Goldcoin payout (unit-level), and the on-chain `DepositClaim` guard —
  not merely this service's own ledger state — is what actually stopped a
  real, independently-reconstructed replay attempt from moving a second
  time.
- Bug 3 (the reconciliation false-breach) was a liveness/availability
  defect (a legitimate, correctly-attested settlement could brick future
  reservations), not a custody or funds-safety defect — no path existed
  for it to move funds incorrectly, only to incorrectly refuse new ones.
  Recorded here because "fail-closed" behavior that fails closed for the
  wrong reason is still an operational risk worth tracking.
- Signing keys remain out of the repository in every path exercised
  (`DevVaultSigner`/`DevAttestationSigner` in-memory dev keys, generated
  fresh per test, never persisted).

## Deployment blockers

- None found that are architectural. The gaps above are operational
  (runbook sequencing, broader-network rehearsal, load testing) rather
  than defects in the bridge's core logic, and the two production-
  blocking defects found this phase (bugs 1 and 2) are fixed and covered
  by regression tests.
- Before any production conversation: real testnet/devnet rehearsal (not
  single-node), an explicit decision on the cold-start reconciliation
  grace-period question above, and the runbook update noted there.

## Did both directions actually settle end-to-end?

**Yes.** Both GLC -> Solana and Solana -> GLC settled completely against
real, independent nodes running this repository's own compiled program,
with exact 1:1 accounting confirmed by reading real balances back from
those nodes — not simulated, not mocked, not asserted against this
service's own bookkeeping alone.

## Verdict: REHEARSAL READY

Both directions settle end-to-end against real, isolated regtest/local-
validator infrastructure with exact 1:1 accounting. The on-chain replay
guard, crash/restart recovery, and post-settlement reconciliation all hold
under real-node adversarial testing. Two production-blocking defects
(Borsh decode bug, missing vault-UTXO sync) and one significant liveness
defect (reconciliation false-breach/permanent pause) were found and fixed
with regression coverage during this rehearsal — exactly the kind of
finding real-node acceptance testing exists to catch, and exactly why unit
tests alone were not sufficient to call this done.

Not yet a PRODUCTION-CANDIDATE: the remaining gaps above (cold-start
reconciliation margin as a deliberate design decision rather than a patch,
multi-node/broader-network rehearsal, load testing, closing the signer-
loss and `record_goldcoin_completion` real-node gaps) are the kind of
work a production candidacy would reasonably require before mainnet is
even discussed. None of them are known correctness defects — they are
scope not yet covered.
