# Load/Soak Test Harness

Built for docs/22-production-readiness-review.md's P1-3 ("No load/soak testing has been performed"). Closes the harness portion of that item — see "What P1-3 still needs" at the end of this document for exactly what remains.

## What this is

A deterministic, seeded workload generator and end-of-run invariant checker, driven against the same real regtest Goldcoin node + real `solana-test-validator` infrastructure `tests/regtest_acceptance.rs` already uses — no mocks, no new bridge states, no new APIs, no invented settlement semantics. The engine lives in `service/tests/support/load_harness.rs`; the two `#[tokio::test]` entry points are in `service/tests/load_soak_harness.rs`.

It exists to answer a question no other test in this repository answers: what happens to accounting correctness under *sustained, concurrent, bidirectional* traffic, not one or two sequential requests?

## Prerequisites (same as `regtest_acceptance.rs`)

- `GOLDCOIND_BIN` / `GOLDCOIN_CLI_BIN` environment variables pointing at a real Goldcoin Core daemon/CLI (v0.17.0.0-beta1 in this environment: `/home/reaper/tools/goldcoind`, `/home/reaper/tools/goldcoin-cli`).
- `solana-test-validator` on `PATH` (Agave, matching `service/Cargo.toml`'s pinned `solana-sdk`/`solana-client` major version).
- `target/deploy/glc_reserve_bridge.so` built (`anchor build` from the repo root, or `cargo build-sbf --sbf-out-dir target/deploy` from `programs/glc-reserve-bridge`).

Without all three, both tests print a `skipping:` line to stderr and pass trivially (never fail) — see `support::phase6_prereqs`. This is intentional and matches every other real-node test in this repository: `cargo test` stays green in an environment without these binaries.

## Running it

```
export GOLDCOIND_BIN=/path/to/goldcoind
export GOLDCOIN_CLI_BIN=/path/to/goldcoin-cli
cd service

# Smoke profile — runs in CI/local verification, ~80-90s wall clock.
cargo +nightly test --test load_soak_harness smoke_load_profile_completes_with_healthy_accounting -- --nocapture

# Soak-profile wiring check — a SHORT instance of the soak profile,
# proving the profile itself is correctly wired end-to-end. #[ignore]d by
# default so it never runs as part of a normal `cargo test`.
cargo +nightly test --test load_soak_harness soak_profile_wiring_short_duration_smoke -- --ignored --nocapture
```

Both print `report.summary()` to stderr — pass `--nocapture` to see it even on success.

## Profiles

`LoadProfile::smoke()` and `LoadProfile::soak(duration)` are both plain, fully-`pub`-field constructors — override anything by struct-update syntax:

```rust
let profile = LoadProfile {
    concurrency: 16,
    pacing_interval: Duration::from_millis(500),
    ..LoadProfile::soak(Duration::from_secs(4 * 3600))
};
```

| Field | Meaning |
|---|---|
| `duration` | Wall-clock cap on the workload-generation phase (measured from when generation actually starts, not from the start of node bootstrap). |
| `target_requests` | Stop generating once this many total requests have been issued, even before `duration` elapses. `None` for `soak()` — duration-bound only, the natural choice for a genuine soak. |
| `concurrency` | Max requests simultaneously in a non-terminal (reservation-active) state before the generator pauses issuing more — this is what actually exercises concurrent capacity/reservation pressure. See "What 'concurrency' means" below. |
| `pacing_interval` | Minimum spacing between successive new-request submissions. |
| `tick_interval` | How often `Orchestrator::tick` runs. |
| `mine_every_n_ticks` | How often one Goldcoin regtest block is mined — the only way confirmations/payout finality advance on a regtest node with autogeneration disabled. |
| `drain_timeout` | After generation stops, how long to keep ticking/mining waiting for every issued request to reach a terminal state before giving up and reporting the rest as stuck. |
| `seed` | Seeds the workload's direction/amount choices (see "Determinism" below). |
| `glc_to_sol_weight` / `sol_to_glc_weight` | Relative traffic mix between directions. |
| `glc_to_sol_amount_range` / `sol_to_glc_amount_range` | Random amount bounds, in each direction's native atomic units. |
| `initial_solana_reserve` / `initial_goldcoin_reserve` | Starting reserve balances — must comfortably exceed the run's total possible settled volume or requests legitimately start failing with `InsufficientLiquidity` partway through, which defeats the point of a sustained-*successful*-traffic run. `soak()`'s defaults size this from `duration`/`pacing_interval`; override for a different-shaped workload. |

### What "concurrency" means here

Many requests can be simultaneously in flight through the bridge's own state machine (reserved-but-not-yet-settled) — that is what `concurrency` bounds. Raw *submission* to the two chains is issued from a single sequential driver loop, not fanned out across OS threads: `deposit_to_reserve` requires reading the live `BridgeConfig.obligation_count` and using it in the same transaction (a stale read fails the on-chain seeds constraint by construction), and the regtest node's wallet is a single shared resource. Serializing submission is simpler and realistic — a real relayer's own request intake is a single pipeline too, even though many settlements progress concurrently behind it.

### Determinism

Which direction and what amount each issued request uses is deterministic given `seed` (a small seeded splitmix64 generator defined directly in `load_harness.rs`, not the `rand` crate, so the exact sequence is reviewable and stable across crate-version drift). Real wall-clock timing — confirmation latency, exact tick interleaving with real `goldcoind`/`solana-test-validator` subprocess scheduling — is **not** bit-reproducible between runs, because it depends on real subprocess and OS scheduling. "Deterministic" means the intended workload is reproducible, not that two runs produce byte-identical timelines.

### Amount-representability constraint (GlcToSol)

With the fixed 1% bridge fee (`BRIDGE_FEE_BPS = 100`) and the 8-decimal (Goldcoin canonical) → 6-decimal (canonical mint) narrowing conversion, `net = gross - gross/100`; for `net` to itself be exactly representable at 6 decimals requires `gross` to be a multiple of 10,000 canonical atomic units — worked out directly:

- `fee = gross / 100` (exact, since `BRIDGE_FEE_BPS = 100` means `bps/10000 = 1/100`).
- `net = gross - fee = gross - gross/100 = 99·(gross/100)`.
- For `net` to be a multiple of 100 (the 8→6-decimal scale), and `gcd(99, 100) = 1`, `gross/100` must itself be a multiple of 100 — i.e. `gross` must be a multiple of 10,000.

`GLC_TO_SOL_REPRESENTABLE_STEP = 10_000` in `load_harness.rs` encodes this; `Rng::gen_range_step` only ever generates multiples of it. A real API/UI layer would need the same rounding before ever creating a request — this is real protocol behavior, not a harness workaround. (If the fee rate or decimals pair ever changes, re-derive the step the same way.)

## Traffic generated

Real, fully-materialized bridge requests through the real code paths, not synthetic HTTP hammering:

- **GlcToSol**: `Ledger::create_request` (via a second `Ledger` connection to the same on-disk file, matching the real API-layer/daemon-loop split) followed by a real regtest deposit transaction (`RegtestNode::send_deposit_with_binding`) carrying the correct OP_RETURN binding, to a small round-robin pool of 4 pre-created, pre-finalized recipient token accounts.
- **SolToGlc**: a real, freshly-created throwaway depositor keypair, airdropped, given an ATA, minted the exact amount, then a real signed `deposit_to_reserve` transaction — exactly what a real user's wallet would submit.
- `Orchestrator::tick` (the real orchestrator, unmodified) drives both directions' settlement every `tick_interval`; a Goldcoin block is mined every `mine_every_n_ticks`.

## Invariants checked

All computed from the ledger's own state after the run (a fresh read, not shadow bookkeeping the harness maintains independently):

- **Reserve invariant** (`Ledger::check_invariant`) holds for both `SolanaReserve` and `GoldcoinReserve`.
- **No duplicate settlement / duplicate entitlement**: no two `Settled` GlcToSol requests share a `(source_txid, source_vout)`; no two `Settled` SolToGlc requests share a `source_obligation_index`. (Structurally enforced by DB `UNIQUE` constraints already — this is a harness-level regression check on top, not the primary defense.)
- **Fee accounting**: for each reserve, the sum of `fee_amount_atomic` across every `Settled` request whose fee accrues to that reserve equals `Ledger::accrued_fees`. Fees accrue to the **source** reserve of a direction, not the destination (docs/20-bridge-fee.md: "the fee remains on the source side where it was collected") — the harness computes this the same way the ledger itself does, not by re-deriving a shadow formula.
- **Replay/idempotency**: one settled GlcToSol request's exact recorded deposit observation is re-delivered directly against the ledger; the outcome must be `AlreadyRecorded` or `NoMatchingRequest` (both are safe, "no re-processing" outcomes — which one occurs depends on real timing, since the request has almost certainly progressed well past `DepositObserved`/`Confirming` by the time the post-run check runs) with zero change to `settled_liquidity`/`reserved_liquidity` either way.
- **Stuck-request detection**: every issued request id is checked against its final ledger state; anything still in an active (reservation-holding) state at the drain deadline is reported by id, direction, state, and age.

## Statistics collected (`LoadRunReport`)

Requests issued per direction, requests rejected at creation (insufficient liquidity, paused, or non-representable amount), SolToGlc submission failures (with up to 5 sampled error strings), unresolved SolToGlc submissions (deposited on-chain but never folded into a request within the drain window), final per-direction state histograms, stuck requests, expected vs. observed accrued fees per reserve, settled liquidity, the invariant/duplicate/replay results above, every tick's accumulated errors, and reconciliation breach count out of total reconciliation ticks. `LoadRunReport::summary()` renders all of it as one line; `LoadRunReport::accounting_healthy()` is the single pass/fail signal for the invariants (deliberately separate from `stuck_requests.is_empty()`, which is its own, separately meaningful signal — see below).

## Success/failure criteria

- `accounting_healthy()` must be `true` — every invariant above held.
- `stuck_requests` should be empty for a profile sized to fully drain within its `drain_timeout` (the smoke profile is sized this way and asserts it). A soak profile run for real over hours might reasonably end with a handful of requests mid-flight if traffic generation only just stopped — that is not itself a failure, and the report's `stuck_requests` list is exactly the mechanism for identifying and investigating each one by id.
- A small number of transient reconciliation breaches (currently allowed up to 2 in both test entry points) is expected and tolerated — see the next section. A larger or persistent number, or any breach that correlates with a permanently stuck request or a `stuck_requests`/`accounting_healthy` failure, is a real finding to investigate.

### Known, pre-existing gap this harness surfaces (not introduced by it)

`reconciliation::Classification` has three variants — `WithinTolerance`, `InFlightExplained`, `Breach` — but `InFlightExplained` is, per its own doc comment, "not yet produced by this phase's logic." Reconciliation therefore has no tolerance today for a payout that is broadcast but not yet folded into this service's own "settled" bookkeeping at the instant a reconciliation tick happens to run — a real, if narrow, timing window under concurrent settlement that a `Breach` classification (which auto-pauses the affected reserve) can catch. Sequential single-request tests never exercise this window; this harness does, because it runs many requests concurrently. This is a genuine, documented, pre-existing production-readiness gap (part of P1-3's own "why it matters" reasoning about reconciliation-timing bugs only appearing under sustained load) — not something this harness invented, and not fixed here. It is exactly the kind of finding a load/soak harness exists to surface.

## Defects found and fixed while building this harness

Building and genuinely running this harness against real infrastructure (previously never actually exercised in any session — every prior "passing" real-node test run was silently *skipped*, not run, because `GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN` were never set) surfaced four real, pre-existing defects, all fixed:

1. `tests/support/mod.rs`'s `create_throwaway_token2022_mint`/`mint_to` called the **legacy** `spl_token` crate's instruction builders with the Token-2022 program id — those builders' own `check_program_account` rejects any id but `spl_token::ID`. Fixed by calling `spl_token_2022`'s own builders (identical instruction encoding, permissive `check_spl_token_program_account`).
2. `service/src/solana/instructions::initialize` was missing the `upgrade_timelock_seconds` argument the on-chain `initialize` instruction gained when the upgrade-timelock mechanism was built — a complete off-chain/on-chain encoding mismatch that made program bootstrap impossible. Fixed by adding the argument (one caller, `tests/support/mod.rs::bootstrap_program`, updated to match).
3. **The bigger one**: `record_goldcoin_completion` signed and verified against `obligation.amount` (the GROSS Solana-side deposit) instead of the NET Goldcoin amount actually paid out — the two differ by exactly the 1% fee, so every SolToGlc completion failed on-chain with `SignatureMessageMismatch`. This program has no fee policy of its own (the fee is off-chain policy, docs/20-bridge-fee.md); it cannot derive the net figure itself. Fixed by adding a caller-supplied `amount: u64` argument to `record_goldcoin_completion`, matching `release_from_reserve`'s existing, audited pattern exactly (a caller-supplied amount verified only via the already-checked threshold signature, never trusted on its own). Touched the on-chain program (`programs/glc-reserve-bridge/src/instructions/complete_goldcoin_payout.rs`, `src/lib.rs`), the off-chain encoder (`service/src/solana/instructions.rs`), and its one caller (`service/src/orchestrator.rs`).
4. Several harness-only bugs (not bridge defects): reading `BridgeConfig.obligation_count` via `RealSolanaRpc` (always `finalized`) instead of the same `confirmed`-commitment client used to submit and wait for the prior transaction, causing a stale-index seeds-constraint failure on the second+ SolToGlc submission in a run; not waiting for recipient ATAs to reach `finalized` commitment before the orchestrator's first `release_from_reserve` attempt; the generation-loop deadline being computed from before node bootstrap instead of after it; and the fee-accounting check initially assuming fees accrue to the *destination* reserve instead of the *source* reserve.

None of these were introduced by this harness — they were pre-existing, and simply had never been exercised by any actually-executed real-node test before now, because the required environment variables were never set in any prior session.

## What P1-3 still needs

**Fully closed by this work**: the harness itself — deterministic short-run validation, invariant checking, reporting, both required profiles, documentation. **Not closed, and not claimed to be**: a genuinely representative multi-hour sustained run. This session ran the smoke profile (~80-90s wall clock, single-digit request counts) and a short instance of the soak profile (~75s) — real, but not remotely a multi-hour rehearsal. What remains, per docs/22's own P1-3 acceptance criteria ("a multi-hour sustained bidirectional run against real regtest/localnet infrastructure with zero accounting drift and zero missed reconciliation breach"):

- An actual multi-hour execution of `LoadProfile::soak(...)` at a realistic sustained rate, ideally on infrastructure that isn't competing with other concurrent sandbox usage.
- A decision on how long a soak run is considered sufficient before launch — explicitly called out in docs/22 as a judgment call for the project owner, not something this harness or session can decide.
- Whatever follow-up the reconciliation in-flight-timing gap above warrants, once observed at real multi-hour scale (does the breach rate stay low and self-heal, or compound?).
