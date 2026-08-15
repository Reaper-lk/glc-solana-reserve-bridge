# Token-2022 checkpoint

Continuation from docs/17-p1-checkpoint.md item 13.1, the one flagged
architecture-level blocker: whether to build Token-2022 support into the
on-chain program. Approved 2026-08-14 as a security-critical compatibility
upgrade, without changing the approved reserve-backed 1:1 architecture.
Full detail is in docs/18-token-2022-support.md; this is the checkpoint
report against the eleven items requested.

## 1. Token-2022 implementation status

**Complete.** The on-chain program (`programs/glc-reserve-bridge`) uses
`anchor_spl::token_interface` throughout — `InterfaceAccount<Mint>`/
`InterfaceAccount<TokenAccount>`/`Interface<TokenInterface>` — accepting
either legacy SPL Token or Token-2022 per configured reserve, structurally
pinned: `initialize_reserve_vault` captures whichever program actually
owns the admin-supplied mint into `BridgeConfig.reserve_token_program`,
and every later `deposit_to_reserve`/`release_from_reserve` call
constrains its `token_program` account to that stored value
(`address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram`).
Substituting the other legitimate SPL program for an already-configured
reserve is a structural, on-chain-enforced rejection, not a convention.

The off-chain service (`service/`) mirrors this: `verify_reserve_mint_token_program`
accepts either program and reports which one it found; ATA derivation
(`accounts::associated_token_address`) and every hand-built instruction
encoder (`solana/instructions.rs`) take the token program explicitly
rather than assuming `spl_token::ID`.

`preserve transfer_checked semantics` — unchanged: `token_interface::
transfer_checked` is Anchor's own re-export, building a standard
`transfer_checked` instruction against whichever program was actually
supplied; this bridge's `deposit_to_reserve`/`release_from_reserve` still
call it the same way, just through the interface type instead of the
legacy-only one.

## 2. Exact extensions found on the canonical GLC mint

Unchanged from the prior read-only mainnet verification
(docs/17-p1-checkpoint.md §1-6), re-stated here for completeness:
**`MetadataPointer`** and **`TokenMetadata`** only. Both extensions'
`authority`/`updateAuthority` fields are `null` — frozen, cannot be
changed by anyone, including the mint's own (also `null`/renounced) mint
authority.

## 3. Extension safety classification

Full table in docs/18-token-2022-support.md. Summary: `MetadataPointer`
and `TokenMetadata` (mint) and `ImmutableOwner` (token account) are the
only extensions accepted — an explicit allowlist, not "everything except
a denylist." Every other Token-2022 extension type currently defined is
classified in `token_extensions::classify` as either Unsafe (would alter
transfer/accounting behavior: transfer fees, transfer hooks, permanent
delegate, confidential transfers/mint-burn, non-transferability,
interest-bearing, default-account-state) or Irrelevant (reviewed, no
bearing on transfer/accounting, but not added to the active allowlist
since the canonical mint doesn't carry them). A future Token-2022
extension type not yet reviewed fails closed as unsupported by
construction. Checked on every reserve-touching call
(`initialize_reserve_vault`, `deposit_to_reserve`, `release_from_reserve`),
on the mint and on every token account each instruction touches — not
just once at onboarding.

## 4. Decimal/conversion policy

**A real, severe bug was found and fixed during this work, not merely a
documentation gap.** Goldcoin's native chain uses 8 decimals; the
canonical Solana GLC mint uses 6. Code that passed a raw atomic amount
from one chain straight through to the other — at the release-claim
message, the `release_from_reserve` instruction, and the Solana->Goldcoin
payout-plan construction — treated the two as if identical. With the real
decimals actually differing, this was a 100x error: Goldcoin->Solana
releases would have moved 100x too much GLC; Solana->Goldcoin payouts
would have paid out 1/100th of what was deposited.

Fixed with a single canonical policy (`service/src/amount_conversion.rs`):
widening conversions (more destination decimals) are always exact;
narrowing conversions are exact only when the source amount's low-order
digits beyond the destination's precision are zero, and are **rejected**
otherwise — never rounded or truncated, since either direction of
rounding would create or destroy GLC entitlement. Solana decimals are
never hardcoded at any conversion call site; every caller reads the
reserve mint's live decimals. Full policy and exactly which four call
sites it's wired into: docs/18-token-2022-support.md.

**Known remaining gap, flagged not fixed:** `Ledger::create_request`/
`fold_sol_deposit`'s capacity checks compare a request's amount directly
against the *opposite* chain's reserve balance with no unit conversion.
This does not move funds incorrectly (the fix above already prevents
that), but it does mean capacity is checked overly conservatively in one
direction and overly permissively in the other whenever the two chains'
decimals differ. Fixing it changes what unit reserve balances and the
public API's declared amount are denominated in, system-wide — flagged as
a reserve-sizing/API-contract decision requiring your input, not resolved
unilaterally.

## 5. Tests added and results

- On-chain workspace: **90 tests, 0 failed** (up from 79) — 11 new in
  `programs/glc-reserve-bridge/tests/token2022_adversarial.rs`: real
  Token-2022 vault init with a benign extension (accepted), with an
  unsupported extension alone or mixed with a benign one (rejected), real
  legacy SPL vault init (also accepted), non-admin and double-init
  rejection, a full real Token-2022 deposit + release settling 1:1, and
  three substitution attacks against a real Token-2022 reserve (the other
  legitimate program in either direction, a real-but-unconfigured mint, a
  token account for a different mint) — all fail closed. Plus the 22
  passing tests from the on-chain Token-2022 migration itself
  (`token_extensions.rs`'s own 11 unit tests, folded into the 90).
- Off-chain service workspace: **271 lib tests + 29 integration tests
  across 7 files, 0 failed** (up from 257 lib tests at the last
  checkpoint). New/changed: `amount_conversion.rs`'s 10 unit tests
  (boundary, remainder-rejection, round-trip-across-10,001-amounts,
  overflow); `solana::accounts` gained Token-2022 mint
  acceptance/extension-rejection tests; `solana::instructions` gained
  Token-2022 ATA-derivation/token-program-pinning tests; every test
  fixture across `orchestrator`, `signing::attestation`,
  `signing::goldcoin_vault`, `solana::indexer`, `api`, `daemon` that
  constructs a fake on-chain account was updated for the new
  `BridgeConfig.reserve_token_program` field and, where the conversion fix
  touches it, the correct converted amounts.
- `cargo fmt --check` / `cargo +nightly fmt --check` (on-chain / service):
  clean.
- `cargo clippy --all-targets -- -D warnings` (both workspaces): clean.
- `cargo deny check` (both workspaces): `advisories ok, bans ok, licenses
  ok, sources ok` — no new unaddressed advisory from the new
  `spl-token-2022` service dependency (an existing `entrypoint`-symbol
  collision with an older transitive copy pulled in by
  `spl-associated-token-account` was resolved via `features =
  ["no-entrypoint"]`, not suppressed).

## 6. Real-node results

**All 3 tests pass, genuinely exercising the real 8-vs-6 decimal
mismatch**, not a degenerate case: `service/tests/regtest_acceptance.rs`'s
throwaway Solana mint was changed from 8 decimals to 6 (the canonical
mint's real value), specifically so these tests would have failed loudly
if the conversion fix were wrong, rather than accidentally passing because
both chains happened to agree.

- `glc_to_sol_release_settles_end_to_end_on_real_nodes` — real Token-2022
  throwaway mint (carrying a real `MetadataPointer` extension, matching
  the canonical mint's actual shape), real regtest Goldcoin deposit, real
  Solana release, recipient's real SPL balance asserted equal to the
  live-mint-decimals-converted amount.
- `sol_to_glc_payout_settles_end_to_end_on_real_nodes` — real Token-2022
  deposit, real Goldcoin payout broadcast and mined, destination's real
  regtest balance asserted equal to the Goldcoin-native-converted amount.
- `double_release_crash_restart_and_reconciliation_on_real_nodes` — real
  Token-2022 settlement, on-chain `DepositClaim` replay rejection (with
  the replay attempt rebuilding the exact converted claim, so it
  genuinely exercises the replay guard rather than failing for an
  unrelated reason), post-settlement reconciliation, and full
  crash/restart recovery for a second request — all against the real
  6-decimal mint.

## 7. Commits created

All local, nothing pushed, nothing merged, no PR opened:

1. `33635e7` — On-chain Token-2022 support for the reserve mint
2. `c2317bf` — Off-chain Token-2022 support: mint verification, ATA
   derivation, instruction encoders
3. `3b7d071` — Fix cross-chain amount conversion: Goldcoin (8 decimals)
   vs Solana GLC (live decimals)
4. `6a5c760` — Correct `DepositClaim.amount`'s doc comment
5. `9684ac4` — Adversarial Token-2022 test matrix for
   `initialize_reserve_vault` and the pinned-program invariant
6. `51ee3e9` — Document Token-2022 support

## 8. Remaining P1 work

Reviewed against docs/17-p1-checkpoint.md §12's list; nothing on it was
safely closeable this round without infrastructure or a decision this
session doesn't have:

- Broader-network rehearsal (multi-node, real testnet) — needs
  infrastructure not available here.
- Load/soak testing — not attempted.
- Real-node verification of signer-loss and `record_goldcoin_completion`
  specifically — still only mock/unit tested.
- Dedicated post-finality-reorg detection/auto-pause path — still not
  backed by dedicated code.
- Docker build verification — sandbox still has no Docker daemon access.

## 9. Remaining production/security decisions

Unchanged from docs/17-p1-checkpoint.md §13, plus one new item from this
round:

1. HSM/KMS/custody-vendor selection.
2. Solana program upgrade-authority posture.
3. Production reserve sizes and transfer/rate limits.
4. **New:** how `bridge_requests`/reserve-capacity accounting should be
   denominated when the two chains' decimals differ (item 4 above) — a
   reserve-sizing/API-contract decision, not a bug fix.
5. External security audit scope/timing (docs/12 item 9).
6. Connecting the existing API to the old bridge frontend.

## 10. Updated completeness percentages

| Area | docs/17 (P1 checkpoint) | Now | Why it moved |
|---|---|---|---|
| Core bridge software | ~82% | **~90%** | The single blocking incompatibility flagged in docs/17 — this bridge could not interoperate with the real GLC token at all — is closed. A second, independently severe bug (the decimal-mismatch fund-movement error) was found and fixed in the same pass, before it could reach a real settlement. |
| Test/rehearsal completeness | ~68% | **~74%** | 11 new on-chain adversarial tests exercising real Token-2022 accounts against a genuinely executable bundled program (not a legacy-shaped stand-in); real-node suite now exercises the actual 8-vs-6 decimal mismatch instead of a degenerate equal-decimals case. |
| Production operational readiness | ~55% | **~55%** | Unchanged — no ops/deployment work this round. |
| UI completeness | ~15% | **~15%** | Unchanged. |
| **Overall mainnet readiness** | ~45% | **~58%** | The confirmed, specific blocker from docs/17 (Token-2022 incompatibility) is resolved, and a second real fund-safety bug that would have surfaced only against the real mint's real decimals is fixed and covered by real-node tests. Custody/HSM, external audit, broader-network rehearsal, and the newly flagged capacity-accounting-unit decision remain open. |

## 11. Technical compatibility with the real Solana GLC token

**Yes.** This bridge is now technically compatible with the real,
existing Solana GLC token
(`Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump`, Token-2022, 6 decimals,
`MetadataPointer`+`TokenMetadata` only) — verified against a real
Token-2022 program (litesvm's bundled `spl_token_2022` for on-chain
adversarial coverage; a real `solana-test-validator` for the real-node
acceptance suite), not merely asserted. Nothing was minted, burned,
wrapped, or otherwise created — every settlement moves existing GLC
between pre-funded reserves, 1:1, exactly as approved. No mainnet
transaction was submitted, no production key was used or generated, and
nothing was pushed, opened as a PR, or merged.
