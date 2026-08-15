# P1 checkpoint: real mint verification, Goldcoin mainnet addressing, P1 engineering

Continuation from docs/16-p0-checkpoint.md. Scope: Task 1 (verify the real
Solana GLC mint, read-only, mainnet), Task 2 (resolve the Goldcoin mainnet
address blocker), Task 3 (continue independently-completable P1 work). No
mainnet transactions submitted, no wallet/private key used for
verification, no production funds touched, nothing pushed/merged/deployed.

## 1-6. Verified Solana mint

Read-only `getAccountInfo` against `api.mainnet-beta.solana.com` for
`Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump` — no transactions, no
wallet:

| Field | Value |
|---|---|
| Mint account exists | Yes |
| **Token program** | **spl-token-2022** (`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`) — **not** legacy SPL Token |
| Decimals | 6 |
| Current supply | 978,182,574.793857 GLC (978182574793857 raw) |
| Mint authority | None (renounced) |
| Freeze authority | None (renounced) |
| Extensions | `metadataPointer` + `tokenMetadata` only (on-chain name="GOLDCOIN", symbol="GLC"; both `authority`/`updateAuthority: null` — frozen). No transfer-fee, transfer-hook, permanent-delegate, non-transferable, or confidential-transfer extension. |
| `transfer_checked` correctness | The instruction family itself is correct for any SPL-family mint, but must be issued against the **Token-2022** program for this specific mint — this program currently issues it against legacy SPL Token only |
| **Compatibility with this program** | **Not compatible as built.** See below. |

**Two real, independent findings, one fixed, one flagged as a decision
point:**

1. **Fixed**: `GLC_DECIMALS: u8 = 8` was hardcoded in both
   `release_from_reserve`/`deposit_to_reserve`'s `transfer_checked` calls.
   The real mint uses 6 decimals — `transfer_checked` validates its
   `decimals` argument against the mint account and errors on any
   mismatch, so every release/deposit against the real mint would have
   failed on-chain, independent of the token-program question. Fixed to
   read `ctx.accounts.reserve_mint.decimals` directly from the mint
   account already held and constrained by each instruction, rather than
   a build-time guess — correct for whatever a configured mint's actual
   decimals are, no rebuild needed if the mint ever changes. Regression
   test added (`release_uses_the_real_mints_decimals_not_a_hardcoded_constant`,
   pins a 6-decimal mint specifically). Full on-chain suite (68 tests) and
   the real-node suite (5 tests) pass against the rebuilt program.
   Commit `3041140`.

2. **Flagged, not fixed**: this bridge's on-chain program only supports
   the legacy SPL Token program (`anchor_spl::token::Token`/`Account<Mint>`
   typed constraints, `transfer_checked` targeting that program
   specifically). The real mint is Token-2022. Adding Token-2022 support
   is a genuine on-chain program change with real security-review
   implications — Token-2022's broader extension surface (transfer fees,
   transfer hooks, permanent delegate, confidential transfers) is exactly
   the kind of thing that could silently break the 1:1 reserve invariant
   if the wrong extension were ever present on a configured mint, even
   though *this specific* mint's current extensions are benign. This is
   treated as the "architecture-changing decision" carve-out from your
   instructions, not implemented unilaterally. `accounts::verify_reserve_mint_token_program`
   (docs/16) already fails closed correctly on this mint today — that is
   the right behavior until a decision is made, not a bug to silently
   work around. Commit `98e4f77` extended that check to identify a
   Token-2022 owner specifically (rather than a generic "wrong program"
   message) and to report the mint's real decimals/supply/authorities
   even in the rejection case, for a clearer operator-facing error.

`reserve_token_mint` remains the single, already-centralized config field
this flows through (docs/16) — grepping production code for hardcoded
mint constants outside config still returns nothing.

## 7-8. Goldcoin mainnet P2PKH/P2SH parameters — blocker resolved

No Goldcoin source code was available locally to read `chainparams.cpp`
from directly — the build tree that once held it (referenced by stale
`.d` dependency files in `/tmp/*-build/`) was already gone, only compiled
`.o` object files remained. The real, authoritative `goldcoind` binary
(`/home/reaper/tools/goldcoind`, the same one Phase 6's real-node testing
already uses) was used as the source of truth instead: an isolated,
network-disabled mainnet session (`-connect=0 -listen=0 -dnsseed=0`, no
peers, no chain sync — `getnewaddress`/`createmultisig` are pure local
key/script math, needing no blockchain state) produced real addresses,
independently decoded and verified:

| | Version byte | Prefix | Verification |
|---|---|---|---|
| Mainnet P2PKH | `0x20` (32) | `E` | Real `getnewaddress` + `validateaddress`; base58check-decoded, checksum confirmed against the real `scriptPubKey` hash160 |
| Mainnet P2SH | `0x32` (50) | `M` | Real 2-of-3 `createmultisig`; `hash160(redeemScript)` independently recomputed from the real redeem script bytes and confirmed to match the decoded address payload |
| Testnet P2PKH | `0x6f` (111) | `m`/`n` | Real, separate isolated `-testnet` session — confirmed identical to what this codebase already had pinned for regtest |
| Testnet P2SH | `0x3a` (58) | `Q` | Same testnet session, same confirmation |

Checksum algorithm: standard double-SHA256, first 4 bytes — confirmed
identical to Bitcoin's (already what this codebase implemented). Compressed
public-key handling and multisig P2SH construction (`OP_2 <pk1> <pk2>
<pk3> OP_3 OP_CHECKMULTISIG`, hash160'd, base58check-encoded) were already
correctly implemented for regtest and needed no changes — only the
version bytes were network-specific.

`goldcoin::address::Network` (Mainnet/Testnet) is now threaded explicitly
through every encode/decode call site with no default — `MultisigVault::new`/
`from_redeem_script_hex`, the payout-destination P2PKH decode in
`signing::goldcoin_vault`, `OrchestratorConfig`, and `config.rs`'s
`goldcoin.network` field (now accepts `"regtest"`/`"testnet"`/`"mainnet"`,
still fails closed on anything else). Golden-vector tests added for both
new mainnet addresses alongside the existing testnet/regtest ones. Commit
`7fe9084`.

## 9. P1 functionality completed

- **cargo-deny + basic CI** (commit `645dcdf`). Both Cargo workspaces
  (on-chain `programs/`+`shared/`, and the off-chain `service/`) now pass
  `cargo deny check` (advisories/licenses/bans/sources) cleanly, with
  every ignored advisory individually triaged and documented with a
  specific reason — none blanket-suppressed. Two direct-dependency
  findings are flagged as genuine P2 items rather than dismissed:
  `libsecp256k1` 0.6 (unmaintained upstream, used for real Goldcoin vault
  signing) and the `ed25519-dalek` 1.0.1 advisories pulled in via
  `solana-sdk`'s own pin (this bridge's attestation signing runs on top
  of `solana_sdk::signature::Keypair`, so this is on the real signing
  path, not incidental). A `.github/workflows/ci.yml` runs
  fmt/clippy/test/deny for both workspaces. Real-node tests are
  deliberately not run in CI (no real `goldcoind`/`solana-test-validator`
  on a standard runner; they already self-skip rather than fail when
  absent).
  - **Side finding**: `rust-toolchain.toml`'s pinned stable 1.85.0 no
    longer satisfies the `service/` workspace's own dependency tree's
    MSRV (reqwest's transitive `icu_*`/`time`/`serde_with` versions now
    need rustc 1.86-1.88). Nightly has been a necessary workaround for
    this throughout Phase 6/P0/P1 work, not a deliberate feature
    requirement — confirmed by testing: the on-chain workspace builds
    fine on the pinned stable toolchain, only `service/` doesn't.
    Documented in the CI workflow's own comments.
- **Deployment manifest** (commit `ba76f73`). `docker/Dockerfile`: a
  multi-stage build (nightly for the reason above, minimal debian-slim
  runtime, non-root user), config/keys always mounted at runtime, never
  baked in. `docker/` was previously empty. The actual `docker build`
  could not be run in this session's sandbox (no access to the Docker
  daemon socket, no passwordless sudo; a systemwide permission change was
  not made to work around it) — documented plainly in `docker/README.md`
  rather than claimed as verified. The build command itself
  (`cargo build --release --bin glc-bridge-daemon`) is the same one
  exercised natively throughout this work.
- **Outbound alerting** (commit `1b13b18`). `ops::alerting`: polls
  `Ledger::is_paused` for both reserve directions on its own interval,
  independent of the tick loop (a pause can be set by `reconciliation`
  inside a tick, or by an operator's own `glc-admin onchain-pause`
  outside any tick at all — the persisted ledger state is the one place
  guaranteed to see either). POSTs a JSON webhook notification on the
  `false -> true` transition only (edge-triggered, not repeated noise for
  a condition already known and not yet cleared). Wired into
  `glc-bridge-daemon` behind an optional `service.alert_webhook_url`
  config field, matching how the bridge API is optional behind
  `api_bind_addr`.
- **Backup/restore tooling** (commit `8b56172`). Three scripts, tested
  end to end against a real schema-valid ledger database (backup with
  integrity check, restore with a real overwrite-refusal check, and the
  full `run-audit-cron.sh` pipeline producing a clean `glc-audit` exit 0):
  `scripts/backup-ledger.sh` (safe online SQLite `.backup`, never a plain
  file copy), `scripts/restore-ledger.sh` (verifies
  `PRAGMA integrity_check` before installing, refuses to clobber an
  existing destination), `scripts/run-audit-cron.sh` (the cron/systemd-
  timer entry point tying the two together with `glc-audit`, exit code is
  `glc-audit`'s own for direct scheduler-failure wiring). Documented in
  `docs/09-runbook.md`'s executable-commands list.

## 10. Tests and results

- `cargo +nightly test --lib` (service): **257 passed, 0 failed** (up
  from 250 at the start of this round)
- `cargo +nightly test` (service, full default suite, all binaries):
  all passed
- `cargo test` (on-chain workspace, pinned stable toolchain — no
  `+nightly` needed here): **68 passed, 0 failed** (12 new: 1 decimals
  regression test)
- Real-node suite (`GOLDCOIND_BIN`/`GOLDCOIN_CLI_BIN` set), rerun after
  every change that touched the on-chain program or address encoding:
  **5 passed, 0 failed**, repeatedly, including after the on-chain
  program rebuild for the decimals fix
- `cargo +nightly fmt -- --check` / `cargo fmt -- --check` (both
  workspaces): clean
- `cargo +nightly clippy --all-targets -- -D warnings` / `cargo clippy
  --all-targets -- -D warnings` (both workspaces): clean
- `cargo deny check` (both workspaces): `advisories ok, bans ok, licenses
  ok, sources ok`
- `scripts/backup-ledger.sh`/`restore-ledger.sh`/`run-audit-cron.sh`:
  manually exercised end to end against a real ledger produced by
  `glc-admin status --db <fresh path>` (confirmed this side effect
  creates a real schema-valid database) — all three behaved as documented

## 11. Local commits created (this round, all local, nothing pushed)

- `3041140` — Fix hardcoded `GLC_DECIMALS`: read decimals from the mint, not a guess
- `98e4f77` — Verify the real Solana GLC mint; richer diagnostics on rejection
- `7fe9084` — Resolve the Goldcoin mainnet address blocker
- `645dcdf` — P1: dependency/supply-chain hygiene and basic CI
- `ba76f73` — P1: deployment manifest for glc-bridge-daemon
- `1b13b18` — P1: outbound alerting on reserve-pause events
- `8b56172` — P1: backup/restore tooling for the ledger, wired to glc-audit

## 12. Remaining P1 blockers

None that were independently completable and safe were skipped outright,
but several remain genuinely open, most needing infrastructure or scope
beyond what was safe/practical this round:

- **Broader-network rehearsal** (multi-node, real testnet rather than
  single-node regtest/localnet) — needs infrastructure not available
  here.
- **Load/soak testing** — not attempted this round; everything to date
  remains single- or dual-request/short-duration.
- **Real-node verification of signer-loss and `record_goldcoin_completion`**
  specifically — still only covered by mock/unit tests (docs/14), not
  newly added this round.
- **Dedicated post-finality-reorg detection/auto-pause path** — the
  threat model's claim that this is automatic is still not backed by
  dedicated code (docs/15 §9); only the generic balance-drop check would
  incidentally catch it, untested for this specific scenario.
- **Docker build verification** — written, not run (sandbox limitation,
  see above).

## 13. Information/decisions still required from you

1. **Whether to build Token-2022 support into the on-chain program.**
   This is the one genuinely blocking, architecture-level decision:
   without it, this bridge cannot interoperate with the real, existing
   Solana GLC token at all. The current mint's own extensions (metadata
   only, both frozen) would not themselves break the 1:1 invariant, but
   building general Token-2022 support is a real engineering and
   security-review commitment (the extension surface that *could* break
   it), not a quick change — recommend scoping it as its own dedicated
   piece of work with a security review before merging, not folded into
   routine P1/P2 execution.
2. Same open items carried from docs/16: the custody-domain/HSM-vendor
   decision, the program upgrade-authority posture decision, real
   production values for confirmation depths/reserve sizing/rate
   limits/reservation TTL, and how to connect the existing API to the
   old bridge frontend.
3. If you want the Docker image build actually verified, either grant
   this session's user Docker daemon access (e.g. `docker` group
   membership, which needs a fresh session to take effect) or run
   `docker build -f docker/Dockerfile -t glc-bridge-daemon .` yourself
   and report back.

## 14. Updated completeness percentages

| Area | docs/16 (P0 checkpoint) | Now | Why it moved |
|---|---|---|---|
| Core bridge software | ~78% | **~82%** | Two real, independently-verified-then-fixed/resolved correctness gaps closed: the decimals-hardcoding bug (would have broken every real transfer) and the Goldcoin mainnet address blocker (no valid production address could be derived before this). Token-2022 incompatibility with the actual GLC mint remains a real, unresolved gap capping this below the high 80s. |
| Test/rehearsal completeness | ~65% | **~68%** | New real-node negative test (wrong-token-program refusal), a decimals regression test proven against a non-default-decimals mint, and mainnet/testnet golden-vector address tests verified against real node output. No new *category* of real-node scenario (signer-loss, broader rehearsal) added this round. |
| Production operational readiness | ~35% | **~55%** | The single largest jump this round: CI, dependency hygiene, a deployment manifest, outbound alerting, and tested backup/restore tooling all landed. Still missing: HSM/KMS, a real verified Docker build, a dashboard, multi-node/broader-network rehearsal. |
| UI completeness | ~15% | **~15%** | Unchanged — no new API/UI work this round; the P0 API stands as-is. |
| **Overall mainnet readiness** | ~35% | **~45%** | Real progress on two fronts that matter concretely for mainnet (address correctness, config/CI/ops maturity), but the Token-2022 incompatibility with the actual GLC mint is now a *confirmed*, specific, real blocker rather than an open question — and it sits alongside the still-open custody/HSM, external-audit, and broader-rehearsal items from docs/16. |

Nothing was pushed, no PR opened, no merge, no deploy, no mainnet
transaction submitted, no production keys used or generated, and the
approved 1:1 reserve architecture was not changed. The one Token-2022
carve-out is reported as a decision point, not silently resolved either
way.
