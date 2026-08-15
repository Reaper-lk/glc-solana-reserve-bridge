# Local Development

## Toolchain

- Rust (host, pinned): 1.85.0 — see `rust-toolchain.toml`.
- Anchor CLI: 0.31.1
- Solana CLI / Agave: 2.1.21

Verified locally against this exact pairing (see `Anchor.toml`, `rust-toolchain.toml`).

## Building the program

```
anchor build
```

Produces `target/deploy/glc_reserve_bridge.so` and a dev-only deploy keypair at
`target/deploy/glc_reserve_bridge-keypair.json` (gitignored — never a production key).

## Running tests

Host-side unit tests (pure logic: `shared/`, `limits.rs`, `validation.rs`, `verification.rs`,
`state.rs` layout tests) and litesvm-based integration tests (full instruction behavior) both
run via `cargo test`, but **require the `nightly` toolchain** in this environment, not the
pinned 1.85.0 stable channel:

```
cargo +nightly test --workspace
```

Why: several of Anchor/Solana's transitive dependencies have since published releases requiring
a newer cargo/rustc (edition2024 manifests, raised MSRV) than 1.85.0 provides. `Cargo.lock`
precisely pins the affected crates to versions that predate those requirements — the same
resolved versions the reference bridge repository's own lockfile uses for this Anchor/Solana
pairing — so the *dependency graph itself* builds fine under 1.85.0; only the causally
unrelated compiler-version floor on the crates.io index forces a newer host rustc to even
resolve/download them. `nightly` was already installed in this environment and is used for
test execution only; it does not change `rust-toolchain.toml`, which still governs the
production SBF build via `anchor build`.

If a future environment ships rustc >= 1.88 as its default, `cargo test` will work without the
`+nightly` override — no code or lockfile change required.

See `IMPLEMENTATION_LOG.md` for the full decision record of this and other implementation-phase
choices.

## The off-chain service (`service/`)

A separate Cargo workspace (its own `service/Cargo.toml`, excluded from the root workspace —
see the root `Cargo.toml`'s `exclude` comment). Build and test from within `service/`:

```
cd service
cargo +nightly test
```

The same `+nightly` requirement applies here (see above) — this workspace's `Cargo.lock`
resolved cleanly without needing the precise-pin treatment the on-chain workspace's litesvm
dev-dependency required, since nothing SBF-adjacent is pulled in.

## What is intentionally not built yet

See `IMPLEMENTATION_LOG.md`'s Phase 2 and Phase 0/1 entries. Not yet implemented: timelocked
governance for limit/pause changes (currently admin-immediate), `rebalance_deposit`/
`rebalance_withdraw` instructions, attestation signing clients, the settlement orchestrator,
Goldcoin vault/payout construction, and operator tooling (CLI/health/metrics endpoints). Do not
assume any of these are bugs by omission — they are sequenced into later phases.
