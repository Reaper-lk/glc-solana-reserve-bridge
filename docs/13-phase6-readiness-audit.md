# Phase 6 Readiness Audit

Performed 2026-08-14, before any Phase 6 rehearsal work. Covers exactly what this environment can and cannot support for an isolated, valueless, regtest/localnet acceptance rehearsal — no mainnet, no production funds, no production keys, no production endpoints anywhere in this phase.

## 1. Local infrastructure available

| Component | Status | Detail |
|---|---|---|
| Goldcoin Core daemon (`goldcoind`) | **Available, real binary** | `/home/reaper/tools/goldcoind`, v0.17.0.0-beta1 (statically linked ELF, not stripped). Matches the exact version cited throughout this repo's docs as the Phase 6 target. |
| `goldcoin-cli` | **Available** | `/home/reaper/tools/goldcoin-cli`, matching version. |
| `goldcoin-tx` | Available | Not expected to be needed (this bridge builds/signs transactions itself, not via node RPC round-trips — see `goldcoin::tx`). |
| Solana CLI / test validator | **Available** | `solana-test-validator 2.1.21`, `solana-cli 2.1.21`, `solana-keygen` — all Agave, matching `solana-sdk = "2.1"`/`solana-client = "2.1"` in `service/Cargo.toml`. |
| Anchor CLI | **Available** | `anchor-cli 0.31.1`, matching `programs/glc-reserve-bridge`'s pinned Anchor version. |
| Rust toolchains | **Available** | rustc/cargo 1.85.0 stable (matches `rust-toolchain.toml`, used for the SBF build) plus `nightly` (used for `service/`'s host-side tests per DEVELOPMENT.md — transitive deps require rustc 1.88+). |
| Docker | Present but **inaccessible** (`permission denied` on the daemon socket; no passwordless sudo) | Not needed — the plan below runs `goldcoind`/`solana-test-validator` as plain host processes, exactly like the old bridge's own real-node test harness did. |
| On-chain build artifact | **Fresh** | `anchor build` re-run this session; `target/deploy/glc_reserve_bridge.so` (527,560 bytes) and its keypair (`pubkey` = `BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY`, matching `declare_id!`) confirmed current. On-chain workspace test suite (99 tests: 45 program unit + 22 litesvm integration + 16 shared golden-vector, per its own `cargo test --workspace`) still passes. |
| Disk / memory / CPU | **Ample** | 29G free disk, 13G free RAM, 8 cores. |

**Pre-existing unrelated processes found and left untouched**: a `fed2/` rehearsal directory (`/home/reaper/fed2`, outside both git repos) has six live `goldcoind` regtest processes and one live `solana-test-validator` running the OLD bridge's compiled `glc_bridge.so` at program id `G4eMFXcJLzFd7RnZSC9S9Z71tCiCMits9cgaJ65Zz7sS`, on ports in the `221xx`/`222xx`/`22899` ranges. This is leftover state from unrelated prior manual work on the old federated bridge, not part of this repository or this rehearsal. **Not stopped, not modified, not reused** — this phase's own `goldcoind`/`solana-test-validator` instances use freshly allocated OS-assigned ports (`TcpListener::bind("127.0.0.1:0")`, same technique the old bridge's own test harness used) specifically to avoid any collision or interference.

**Safety note on the Solana CLI's default config**: `solana config get` reports `RPC URL: https://api.mainnet-beta.solana.com` — the global CLI default is mainnet. This phase never invokes the bare `solana` CLI against that default; every RPC client (in Rust, via `solana_client`/this crate's `RealSolanaRpc`) is constructed with an explicit `http://127.0.0.1:<port>` URL, and any ad hoc `solana-keygen`/`solana` CLI invocation used for smoke-checking during this phase must always pass `--url http://127.0.0.1:<port>` explicitly. The global config file is never modified.

## 2. What needs to be started, rebuilt, configured, or generated

- A fresh, throwaway `goldcoind -regtest` node: new `tempfile::tempdir()` datadir, OS-assigned free RPC/P2P ports, throwaway single-process credentials, `-txindex=1` (mandatory per `docs/goldcoin-rpc-notes.md`), bound to `127.0.0.1` only. Torn down (RPC `stop`, then kill on timeout) at the end of the rehearsal process.
- A fresh `solana-test-validator`: new ledger tempdir, OS-assigned free RPC/faucet ports, `--bind-address 127.0.0.1`, our program baked directly into genesis via `--upgradeable-program <program_id> <path/to/glc_reserve_bridge.so> <upgrade_authority_pubkey>` (no deploy transaction needed — confirmed working pattern from the old bridge's own `local_validator_e2e.rs`). Torn down (kill) at the end.
- Fresh, in-process, non-persisted dev keypairs: one Solana `upgrade_authority`/admin keypair, `attestation_threshold`-of-3 `DevAttestationSigner` ed25519 keypairs, 2-of-3 `DevVaultSigner` secp256k1 keypairs, one Solana fee-payer/submitter keypair, one or more recipient keypairs — all `Keypair::new()`/equivalent, generated fresh per rehearsal run, never written to a committed file.
- A throwaway SPL Token mint standing in for the Solana GLC token (docs/12-management-decisions.md item 10 — the real mint's exact address/program is still an open decision; this rehearsal cannot and must not assume one). Created fresh on the local validator via `spl_token::instruction::initialize_mint2` (no new dependency — `spl-token` is already in `service/Cargo.toml`), minted an initial valueless supply into the reserve vault's associated token account.
- Program bootstrap: `initialize` (creates `BridgeConfig`/`AttestationKeySet`/both `RollingVolumeWindow`s) and `initialize_reserve_vault` (binds the throwaway mint, creates the reserve token account) — **new instruction encoders needed**, since neither existed before this phase (only the ongoing-operation instructions — `release_from_reserve`, `record_goldcoin_completion`, `set_paused` — were hand-built so far). These are one-time bootstrap actions, not part of the orchestrator's steady-state loop, so they're being added to `solana::instructions` as small, independently tested encoders (same discriminator/account-order discipline as the existing ones), reusable later by a real launch bootstrap procedure too.
- A Goldcoin P2SH 2-of-3 vault address, registered with the regtest node (`importaddress` for the address, then for the redeem script with the `p2sh` flag — the exact two-call sequence `docs/goldcoin-rpc-notes.md` confirms is required for `solvable: true`), and funded with valueless regtest coin via `generatetoaddress` (block subsidy, confirmed 10,000 GLC/block on regtest in the old bridge's own real-node testing).
- A single `service/tests/`-level acceptance-test binary that owns the whole rehearsal: starts both nodes, bootstraps the program and the vault, then drives `orchestrator::Orchestrator::tick` in a loop against the real endpoints (replacing the mock `GoldcoinRpc`/`SolanaRpc` implementations used everywhere else in this codebase's test suite with the real `RpcClient`/`RealSolanaRpc`), asserting on real ledger/chain state at each step.

## 3. Exact binaries and configuration

- `goldcoind`/`goldcoin-cli`: `/home/reaper/tools/goldcoind`, `/home/reaper/tools/goldcoin-cli` (v0.17.0.0-beta1). Supplied to the test harness via `GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN` environment variables — same opt-in, skip-not-fail convention the old bridge's `relayer/tests/regtest_indexer.rs` used, so the same test suite still passes cleanly in any environment where these binaries aren't present.
- Goldcoin regtest flags (verified facts, `docs/goldcoin-rpc-notes.md`, reused verbatim): `-regtest -datadir=<tmp> -daemon=0 -printtoconsole=0 -rpcuser=<throwaway> -rpcpassword=<throwaway> -rpcport=<free> -port=<free> -rpcbind=127.0.0.1 -rpcallowip=127.0.0.1 -bind=127.0.0.1 -fallbackfee=0.0001 -txindex=1`.
- `solana-test-validator`: on `PATH` (Agave 2.1.21, matching `solana-sdk`/`solana-client` major version already pinned in `service/Cargo.toml` — no version-skew risk). Flags: `--reset --quiet --ledger <tmp> --rpc-port <free> --faucet-port <free> --bind-address 127.0.0.1 --upgradeable-program BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY <repo>/target/deploy/glc_reserve_bridge.so <upgrade_authority_pubkey>`.
- Both endpoints are addressed only as `http://127.0.0.1:<port>` for their whole lifetime; the global `solana config`/`~/.config/solana` is never read or written by this phase's own code (only ambient tools like a stray manual `solana-keygen pubkey` invocation might touch it, and those never select a cluster).

## 4. Mainnet isolation guarantees

- Both node processes bind `127.0.0.1` only, on OS-assigned free ports never fixed in advance — no listener is ever reachable off-host, and no fixed port risks collision with a real service.
- Every keypair used (admin/upgrade authority, attestation signers, vault signers, fee payer, recipients, the throwaway SPL mint) is generated fresh in this rehearsal's own process memory; none is loaded from `~/.config/solana/id.json` or any committed file, and none is ever written to disk outside a `tempfile::tempdir()` that is deleted when the test process exits.
- The SPL token used stands in for GLC but is a throwaway mint created on the local validator for this run only — never the real (as yet undecided, docs/12-management-decisions.md item 10) production Solana GLC mint.
- The Goldcoin coin used is regtest-native block-subsidy coin, which has no exchange value and cannot exist on any other chain.
- The on-chain program deployed is this repository's own build (`target/deploy/glc_reserve_bridge.so`, program id `BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY`), baked into a throwaway validator's genesis — this program id has never been deployed to any real cluster, so there is no possibility of confusion with a live deployment.
- Nothing in this phase's plan invokes `git push`, opens a PR, deploys to any non-local cluster, or writes to any path outside this repository plus `tempfile::tempdir()` locations.

## 5. Phase 6 acceptance matrix

Both directions are exercised through the full state machine using the real `goldcoin::indexer::Indexer`/`solana::indexer::SolanaIndexer`/`orchestrator::Orchestrator` code paths already unit- and mock-tested in Phases 0–5 — this phase's job is confirming that code against real node/validator behavior, not re-deriving the logic.

**Goldcoin → Solana (release)**
1. Reserve a request, deposit valueless regtest coin to the vault with the correct OP_RETURN binding, mine to `confirmation_depth`, confirm the real indexer promotes it to `SourceFinalized`.
2. Confirm the attestation signer group independently re-derives and signs the release claim against the real chain state (live `AttestationKeySet`/`BridgeConfig` reads).
3. Confirm the orchestrator submits a real `release_from_reserve` transaction (ed25519-precompile proof + instruction) and it lands, moving the request to `Settled` with `settled_liquidity` incremented by exactly the deposited amount.
4. Confirm the recipient's real SPL token account balance increased by exactly that amount.

**Solana → Goldcoin (payout)**
1. Submit a real `deposit_to_reserve` obligation on the local validator; confirm the real Solana indexer folds it via `obligation_count` diffing.
2. Confirm the Goldcoin vault-signer group independently re-derives and signs a real payout transaction (coin selection against real `listunspent`-sourced vault UTXOs), and the orchestrator broadcasts it to the real regtest node.
3. Mine to the required confirmation depth; confirm the real indexer/orchestrator observes it and moves the payout to `Confirmed`.
4. Confirm the completion attestation is independently re-derived and submitted as a real `record_goldcoin_completion` transaction, confirms, and the request reaches `Settled`.
5. Confirm the destination Goldcoin address's real regtest balance increased by exactly the requested amount.

## 6. Adversarial / recovery scenarios planned

- **Crash/restart**: kill and restart the orchestrator process mid-flight at several points (after deposit observed, after attestation submitted but before confirmation, after payout broadcast but before confirmation) using the real file-backed ledger; confirm no double-processing and forward progress resumes correctly.
- **Replay**: re-submit an already-processed Goldcoin deposit / already-folded Solana obligation; confirm the ledger's `UNIQUE` constraints and idempotent ledger methods reject/no-op it.
- **Duplicate/double-release**: attempt to trigger a second `release_from_reserve` for the same source deposit; confirm the on-chain `DepositClaim` replay guard and the off-chain ledger both refuse it.
- **Insufficient reserve**: configure a reserve balance below what a request needs; confirm the request is never accepted (capacity check) and, if it becomes true only after acceptance, confirm reconciliation/settlement fails closed rather than releasing anyway.
- **Stale/reorg**: use `invalidateblock`/mine-a-competing-chain on the real regtest node to reorg a deposit before and after `SourceFinalized`; confirm pre-finality reorgs safely reopen the request and post-finality reorgs are treated as the documented breach condition (never silently absorbed).
- **Signer loss**: run the attestation/vault-signer collection with only `threshold - 1` signers available; confirm settlement cannot proceed; confirm it resumes once `threshold` is available again.
- **Reconciliation**: deliberately desynchronize the ledger's cached reserve balance from the real on-chain/on-Goldcoin balance; confirm `reconciliation::reconcile`'s breach classification fires and pauses the affected direction against real chain reads, and that it never auto-unpauses.
- **Recovery**: after a pause (reserve-triggered or reconciliation-triggered), confirm `glc-admin`'s local/on-chain pause commands correctly reflect and clear the state, and that in-flight requests continue settling while new reservations are correctly refused.

## 7. Missing prerequisites

None that block starting the rehearsal. The one real, pre-existing gap — no `goldcoind` binary available in earlier sessions of this environment — turned out to be resolved: a real, correctly-versioned (v0.17.0-beta1) binary already exists locally at `/home/reaper/tools/`. Everything else needed (Solana toolchain, Anchor, a fresh on-chain build, disk/memory/CPU) is already present. The only net-new work is the bootstrap instruction encoders (`initialize`/`initialize_reserve_vault`) and the acceptance-test harness itself, both addressed in this phase's implementation.
