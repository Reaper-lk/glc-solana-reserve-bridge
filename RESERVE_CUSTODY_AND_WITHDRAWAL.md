# Reserve Custody and Withdrawal — Goldcoin ↔ Solana Reserve Bridge

Inspection-only document. Every claim below is backed by a citation to the actual code, config, or CLI as it exists at commit `4b7a28008c0457f36157b7ce56510a3974fc77ec` (`launch-candidate`, merged to `upstream/main` as `61fdd2b`). No source was modified to produce this document. Where the codebase does not implement something asked about, this document says so explicitly rather than describing a plausible-sounding mechanism that isn't real.

---

## 1. Goldcoin L1 reserve custody

**What exact address/type is used for the Goldcoin reserve vault?**
A P2SH (Pay-to-Script-Hash) `M`-of-`N` multisig address. Struct: `MultisigVault` in `service/src/goldcoin/vault.rs:59-63`. The redeem script is the standard `OP_N <pubkey_1..N> OP_M OP_CHECKMULTISIG` form (`build_redeem_script`, same file); the address is `base58check_encode(network.p2sh_version(), hash160(redeem_script))` (`MultisigVault::new`, `service/src/goldcoin/vault.rs:65-77`).

**How is that address derived?**
Constructed fresh at daemon startup, not stored anywhere: `MultisigVault::new(config.operators.vault_pubkeys.clone(), config.operators.vault_threshold, config.goldcoin.network)` — `service/src/bin/glc-bridge-daemon.rs:132-138`. The address is a deterministic hash of the `N` vault public keys and the threshold `M` from `config.toml`'s `[operators]` section. Change any of those inputs and the address changes.

**Is there any single private key or seed phrase?**
No. A P2SH multisig address has no private key of its own — it is a hash of a script. Spending requires the redeem script plus `M` valid signatures, one per distinct participating pubkey. `service/src/goldcoin/vault.rs:1-14`'s module doc: "no single key may authorize a release" (mirrors the Solana program's on-chain guarantee, see §2). `MIN_THRESHOLD: u8 = 2` (`service/src/goldcoin/vault.rs:23`) is enforced at vault construction — a 1-of-N vault is refused outright.

**How does the 2-of-3 signer setup control funds?**
`operators.vault_threshold` (approved pilot value: 2) determines `M` in the `M`-of-`N` script. Every outgoing payout is built and signed input-by-input by exactly `vault_threshold` independent signers, each of which **independently re-derives** the entire payout plan from its own view of chain/ledger state before signing — it is never handed a pre-built plan to blindly co-sign (`IndependentPayoutSource`/`independently_sign`, `service/src/signing/goldcoin_vault.rs:19-27, 213-252`). Orchestration: `Orchestrator::build_and_broadcast_payout` collects partial signatures from `self.vault_signers[..threshold]` (`service/src/orchestrator.rs:668-726`) before assembling and broadcasting.

**Which signer keys/roles are required to authorize an outgoing Goldcoin transaction?**
`vault_threshold` (2 of the configured `vault_pubkeys`, currently 3) distinct vault signers, each independently signing every input of the transaction. No admin signature, no attestation-signer signature, and no Solana-side key is involved in authorizing a Goldcoin-side payout. Orchestrator module doc: "every release and every completion record requires collecting `attestation_threshold`... and `vault_threshold` independent... signatures... The orchestrator itself holds no threshold authority" (`service/src/orchestrator.rs:18-21`).

**Where are those signer keys stored?**
Depends on `operators.mode` (`service/src/config.rs:238-244`, doc comment: `"dev"` or `"production"`, defaults to `"dev"`, "a real production deployment must set this explicitly, never rely on the default"). In `dev` mode: local plaintext key files via `vault_key_paths` (hex-encoded secret key on disk — see `DevVaultSigner`, `service/src/signing/goldcoin_vault.rs:70-80`, explicitly documented as "a **non-production stand-in**... never used for anything but local dev/test"). In `production` mode: remote signer endpoints via `vault_remote_signers` (`RawRemoteSigner { endpoint_url, expected_pubkey }`, `service/src/config.rs:272-`), each a network call to wherever that custody domain's HSM/KMS actually holds the key — this repository never generates, holds, or has access to that key material. **As of this commit, no real HSM/KMS backend implementation exists** — the module doc for `goldcoin_vault.rs` states plainly: "No real HSM/KMS backend is implemented in this phase; building one against the trait is a distinct, later, explicitly-approved piece of work (docs/12-management-decisions.md item 2)." `production` mode requires *something* implementing the `VaultSigner` trait at each configured endpoint — that something is external to this repository.

**What happens if one signer server is lost?**
The remaining 2 (of 3) still meet `vault_threshold = 2` — payouts continue to function normally. This is the entire point of an `M`-of-`N` threshold below `N`.

**What happens if two signer servers are lost?**
Only 1 signer remains, below `vault_threshold = 2`. `build_and_broadcast_payout` requires signatures from `self.vault_signers[..threshold]` — it cannot complete with fewer than `threshold` configured signers available (`service/src/orchestrator.rs:673, 708`). **No Goldcoin payout can be built or broadcast.** Existing reserve funds are not lost or moved — they simply cannot be spent until custody is restored.

**Is there any recovery path?**
Two on-chain-adjacent mechanisms exist, both operating at the **key set** level (i.e., who the signers are), not by bypassing the threshold:
- `glc-admin custody-propose --kind vault-sweep --old-identities ... --new-identities ... --new-threshold N` (`docs/09-runbook.md`'s "Executable commands" list, `service/src/bin/glc-admin.rs`) — but this **only records evidence of a rotation/sweep already performed through real custody tooling outside this system**; it does not itself sign or broadcast anything (see next question). It requires `required_approvals` identities to approve (`glc-admin custody-approve`), and is gated: "fails unless... both reserves are already paused (`attestation-rotation`, since attestation authorizes both bridge directions)" (`docs/09-runbook.md` line 30-area, "Executable commands"). If 2 of 3 vault signers are genuinely lost (keys destroyed, not just servers offline), there is **no cryptographic way to reconstruct the lost private keys** — recovery in that case depends entirely on whatever out-of-band backup/HSM-recovery procedure the custody domain itself has (not part of this codebase). If the servers are merely offline/lost but the underlying key material is recoverable from a backup, restoring that backup restores signing capability with no code-level action needed.
- If genuinely below threshold with no recoverable backup: **there is no automated on-chain recovery.** The vault is a plain multisig; there is no admin override, no timelock-bypass, no "reset signers" instruction anywhere in this codebase for the Goldcoin side (Goldcoin has no program layer at all — see `docs/02-trust-model.md`'s asymmetry note, referenced in `deposit_to_reserve.rs:9-13`). Funds at that specific vault address become **permanently unspendable** unless the lost keys are recovered.

**Can the reserve be manually withdrawn or swept?**
See §5 — **manual reserve withdrawal is not currently implemented** as a first-class, fund-moving command. The `glc-admin rebalance-*`/`custody-*` commands are evidence-recording state machines, not transaction builders (see next question).

**Which CLI/API/admin command performs that action?**
None performs the actual fund movement. `glc-admin rebalance-record-executed`/`custody-record-executed` explicitly document: "records evidence of a real transfer already authorized and executed through real custody tooling **outside this system**... this command (and this entire service) never constructs, signs, or broadcasts a fund-moving transaction itself" (`docs/09-runbook.md`, "Executable commands" list, items for `rebalance-record-executed` and `custody-record-executed`). A real Goldcoin withdrawal today would have to be built and broadcast by hand (or by external tooling) using the vault's redeem script and `M` real signatures — not through any command this repository ships.

**What safeguards, pause checks, timelocks, or approvals apply?**
For the off-chain evidence/approval workflow that exists: `glc-admin rebalance-propose` → `rebalance-approve` (× `required_approvals`) → external real transfer → `rebalance-record-executed` → `rebalance-confirm` (`docs/09-runbook.md` "Rebalancing procedure," `service/src/ledger/mod.rs` rebalance state machine). For custody transitions: `custody-propose` → `custody-verify-identity` (mandatory before any approval) → `custody-approve` × N → `custody-record-executed` (which itself enforces the reserve-already-paused precondition in code, not just documentation — `docs/09-runbook.md` line 30). None of this constitutes an on-chain or cryptographic safeguard on the actual Goldcoin multisig spend itself — the real safeguard for a Goldcoin payout is the `M`-of-`N` script itself (whoever holds `M` keys can always spend, regardless of what this service's ledger says).

---

## 2. Solana GLC reserve custody

**Reserve token account architecture.**
An Associated Token Account (ATA), created once via the `initialize_reserve_vault` instruction:
```rust
#[account(
    init,
    payer = admin,
    associated_token::mint = reserve_mint,
    associated_token::authority = reserve_authority,
    associated_token::token_program = token_program,
)]
pub reserve_token_account: InterfaceAccount<'info, TokenAccount>,
```
`programs/glc-reserve-bridge/src/instructions/reserve_vault.rs:58-64`. `admin` pays the one-time account-creation rent.

**Confirm the GLC-Solana mint.**
`Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump` — confirmed identical across `service/config.pilot-template.toml:28`, `service/src/bin/glc-mainnet-bootstrap.rs:295` (the bootstrap example command), `docs/22-production-readiness-review.md:1559`, `docs/18-token-2022-support.md:53`, and `service/src/solana/accounts.rs:423`. Token-2022, 6 decimals (`docs/18-token-2022-support.md`, `docs/22-production-readiness-review.md:91`).

**Who/what owns or controls the reserve token account?**
Its SPL-level `authority` field is the `reserve_authority` PDA (constraint above). The account itself is never controlled by any human-held key.

**Is its authority a PDA? Confirm no private key.**
Yes. `#[account(seeds = [SEED_RESERVE_AUTHORITY], bump)] pub reserve_authority: UncheckedAccount<'info>` with the doc comment: "**CHECK: data-less PDA, sole authority over the reserve token account. Address is fully constrained by seeds; no keypair exists for it (constraint 8: signing keys never stored in the repository — there is nothing to store).**" (`programs/glc-reserve-bridge/src/instructions/reserve_vault.rs:52-56`). Seed: `SEED_RESERVE_AUTHORITY = b"reserve_authority"` (`programs/glc-reserve-bridge/src/constants.rs:16`). This is a structural, not policy-level, guarantee: `Pubkey::find_program_address` by construction searches for an address off the ed25519 curve, for which no private key can exist.

**How can GLC-Solana be transferred OUT of the reserve? Which instruction(s)?**
Exactly one instruction: `release_from_reserve` (`programs/glc-reserve-bridge/src/instructions/release_from_reserve.rs`, exposed at `programs/glc-reserve-bridge/src/lib.rs:185`). Confirmed exhaustively — every instruction in the program (`grep "pub fn " lib.rs`) is: `initialize`, `initialize_reserve_vault`, `set_paused`, `set_limit`, `transfer_admin`, `accept_admin`, `propose_attestation_key_rotation`, `execute_attestation_key_rotation`, `cancel_attestation_key_rotation`, `release_from_reserve`, `deposit_to_reserve`, `record_goldcoin_completion`, `accept_upgrade_authority`, `propose_upgrade`, `execute_upgrade`, `cancel_upgrade`. Only `release_from_reserve` moves value out of `reserve_token_account`. There is **no other path** — no sweep, no admin-withdraw, no emergency-drain instruction exists anywhere in `programs/glc-reserve-bridge/src/`.

Mechanism, in order (`release_from_reserve.rs:126-208`):
1. `require!(!config.paused, ...)`, `require!(!config.release_paused, ...)` — pause checked first.
2. `enforce_transfer_amount` — min/max per-transfer bounds.
3. `enforce_protected_minimum(reserve_token_account.amount, config.protected_minimum, amount)` — checked against the **live** on-chain balance, before attestation is even verified, so a release that could never be fulfilled "costs nothing to reject" (module doc, line 20).
4. Attestation check: the transaction must carry an immediately-preceding ed25519-precompile instruction over the canonical `release_claim_message` (`glc_reserve_bridge_shared::claim`); `count_unique_attestation_signers` (`programs/glc-reserve-bridge/src/verification.rs`) counts how many *distinct current* attestation keys signed it, and `require!(signer_count >= key_set.threshold, ...)`.
5. Rolling-volume check, recorded only after every prior check passes.
6. `token_interface::transfer_checked` CPI, signed via `invoke_signed` with the PDA's own seeds (`signer_seeds`) — the program signs on the PDA's behalf; no private key is ever used.
7. `deposit_claim` PDA is created at `(SEED_DEPOSIT_CLAIM, txid, vout)` — its `init` constraint makes a second release for the same Goldcoin deposit structurally impossible (replay guard).

**Which admin/governance/signers must authorize it?**
`submitter` (any fee payer — "confers no authority," `release_from_reserve.rs:47-49`) submits the transaction, but the only real authorization is the ed25519 threshold-attestation check in step 4 above: `key_set.threshold` (approved pilot value 2) distinct attestation-key signatures. **`admin` is not a signer on this instruction at all** — admin cannot authorize a release directly under any circumstance in the current program.

**Emergency withdrawal/recovery mechanism?**
None exists as a distinct instruction. If the attestation key set becomes unable to meet its own threshold (see §6), the only recovery route is a program **upgrade** that adds new logic (e.g., an admin-gated force-reset of `AttestationKeySet`) — see §2's "upgrade authority" answer below and the Disaster Recovery Matrix (§6).

**What happens if the bridge program is paused?**
`set_paused(scope, paused)` (`programs/glc-reserve-bridge/src/instructions/admin.rs:61-`), admin-gated, three scopes: `Global` (sets `config.paused`), `Release`, `Deposit`. `release_from_reserve` hard-rejects with `BridgeError::BridgeGloballyPaused`/`BridgeError::ReleaseDirectionPaused` at the very first lines of the handler if either is set (`release_from_reserve.rs:132-134`). Funds are not moved, not at risk — pause only blocks new releases (and, per `Deposit` scope, new deposits); it does not touch existing reserve balances.

**What happens if the program upgrade authority is revoked?**
The program becomes immutable — no future `execute_upgrade` CPI can succeed (Solana's own BPF-upgradeable-loader semantics; this program's `execute_upgrade` still calls `bpf_loader_upgradeable::upgrade` — `programs/glc-reserve-bridge/src/instructions/upgrade_timelock.rs`). This has **no effect on reserve funds or normal operation** — `release_from_reserve`, `deposit_to_reserve`, pause, and attestation rotation are all independent of upgrade authority. The only cost is losing the ability to ever fix a bug or add a recovery instruction later.

**What happens if the admin key is lost?**
Every admin-gated instruction becomes permanently unusable with that key: `set_paused`, `set_limit`, `transfer_admin`, `propose_upgrade`/`cancel_upgrade` (proposing/cancelling only — `execute_upgrade` is "permissionless once eta has passed," `upgrade_timelock.rs:32-37`), `initialize_reserve_vault`'s one-time call (already used). A two-step handoff exists (`transfer_admin` → `accept_admin`, `programs/glc-reserve-bridge/src/lib.rs:150-159`) — but it requires the *current* admin to sign first, so it cannot help after the admin key is already lost with no successor pre-designated. **Reserve funds themselves remain safe** (admin cannot move them directly — see above) but the bridge loses its emergency-pause capability and its ability to change limits or propose upgrades. Recovery would require a program upgrade (needs upgrade authority, a *different* key) to install a new admin-recovery path — the same last-resort pattern as a lost attestation threshold.

**Can the reserve ever become permanently inaccessible?**
Yes, under a specific, narrow condition: if the attestation key set drops below its own threshold (2 of 3 keys genuinely lost, unrecoverable) **and** no functioning upgrade authority exists to install a fix, the reserve tokens remain on-chain, fully solvent, but **no instruction in the current program can move them out** — `release_from_reserve` is structurally the only exit, and it structurally requires the threshold. This is a deliberate design tradeoff (no single point of failure can steal funds) with the mirror-image cost (losing threshold-many keys with no upgrade path can freeze funds, not steal them). Not a bug — a documented consequence of "no single key may authorize a release" (`vault.rs:13`, `release_from_reserve.rs`'s attestation model) — but a real operational risk to weigh before funding.

---

## 3. SOL fee payer / operational wallet

**Which keypair pays transaction fees for normal bridge payouts?**
The `submitter` — `Config::load_submitter` (`service/src/config.rs`), loaded from `operators.submitter_key_path`, and used as the `submitter: Signer<'info>` account on `release_from_reserve` (`release_from_reserve.rs:47-49`, "Any fee payer; funds the claim account's rent. Confers no authority — a valid threshold attestation is the only authority.").

**Submitter, admin, deployer, or another account?**
The **submitter**, a distinct role from all of the above. Explicitly documented (this session's earlier investigation, corroborated by the code above): "not a custody authority — nothing else derives trust from which key this is."

**How much SOL does that wallet need operationally?**
Not specified anywhere in code as a fixed number — it needs enough to cover: (a) the transaction fee for every `release_from_reserve` call it submits, and (b) the one-time rent for each new `deposit_claim` PDA it creates (`payer = submitter` at `release_from_reserve.rs:60-66`, `space = DepositClaim::SPACE`). No config field or on-chain check enforces a minimum submitter balance — this is purely an operational/monitoring concern.

**Does the reserve token account itself need SOL?**
Only the one-time rent-exempt minimum balance, paid once at creation by `admin` (`init, payer = admin`, `reserve_vault.rs:58-64`). No ongoing SOL top-ups are needed for a rent-exempt account under normal Solana rules.

**Does the PDA need SOL?**
No. `reserve_authority` is declared `#[account(seeds = [...], bump)] UncheckedAccount` with no `init` anywhere — it is never created as a funded account with data; it exists only as a derived address the program signs with via `invoke_signed`. It is not required to hold any lamports.

**When a user sends GLC-Solana TO the reserve, who pays the fee?**
The user. `deposit_to_reserve.rs:30-34`: `pub user: Signer<'info>` — "signs its own transfer and pays the obligation record's rent."

**When the bridge sends GLC-Solana OUT, who pays the fee?**
The `submitter` (see above) — the user/recipient pays nothing for this leg.

**Which account pays account-creation/rent costs if an ATA/PDA-associated account needs creation?**
- Reserve token account (once, at setup): `admin` (`reserve_vault.rs:58-64`).
- `deposit_claim` PDA (once per Goldcoin deposit released): `submitter` (`release_from_reserve.rs:60-66`).
- `withdrawal_obligation` PDA (once per Solana deposit): `user`, per `deposit_to_reserve.rs:30-34`'s "pays the obligation record's rent."
- The **recipient's own token account** on a release is explicitly **not** created by this program: `recipient_token_account` uses a plain `associated_token::...` constraint with no `init`/`init_if_needed` — "Must already exist (no `init_if_needed`): matches the old bridge's owner decision that the recipient's ATA is a precondition, not something a stranger's release transaction creates and charges rent for on the recipient's behalf" (`release_from_reserve.rs:104-110`). A release to a recipient without an existing ATA for the reserve mint will fail.

**What happens when the fee payer runs out of SOL?**
Not specially handled in code — this is ordinary Solana behavior: any transaction where `submitter` is the fee payer (i.e., every `release_from_reserve` call) will fail to be signed/submitted/land once its balance can't cover the fee (+ any new-account rent). Existing reserve funds are unaffected — they simply cannot be released until the submitter wallet is topped up. No monitoring/alert specifically for submitter balance was found in this codebase (see §8 recommendations).

---

## 4. Initial reserve funding

**Goldcoin L1 reserve.**
No dedicated "fund reserve" command exists. Per `docs/09-runbook.md`'s "Startup/commissioning sequencing (cold start)": *"Fund the reserve (send GLC to the Goldcoin vault address...)"* — an ordinary Goldcoin transaction to the multisig address described in §1, sent via whatever wallet already holds the funds. Verification (documented, real command): `goldcoin-cli listunspent <vault_min_confirmations> 9999999 '["<vault address>"]'`, confirming the `solvable` entries sum to the funded amount (`docs/09-runbook.md`, "Verifying step 2 without running the daemon"). The vault address itself is obtained by running the daemon (which logs `vault_address = %vault.address()` at startup, `glc-bridge-daemon.rs:139`) or by independently computing it from the same 3 pubkeys + threshold via `MultisigVault::new`.

**Solana reserve.**
Same pattern: *"transfer the reserve mint's tokens into the Solana reserve token account"* (`docs/09-runbook.md`, same section) — an ordinary SPL/Token-2022 transfer to the ATA address computed from `(reserve_mint, reserve_authority PDA)`, sent via whatever wallet already holds the tokens. Verification: poll the reserve token account balance at `finalized` commitment until it matches the transferred amount (same runbook section). **The reserve token account must already exist** (created once via `initialize_reserve_vault`, §2) before this transfer.

**Do not invent commands that do not exist:** there is no `glc-admin fund-reserve`, no `glc-mainnet-bootstrap --fund`, and no on-chain "deposit as admin" instruction. Funding is an ordinary wallet-to-address transfer on each chain, using whatever wallet software already holds the funding source — exactly as `docs/09-runbook.md` describes it, no more, no less.

**Addresses that should NEVER receive reserve funds:**
- **Solana program ID** (`bdUmuB79BUngf9Dd1ZRN3U3xBJMpsixpHaeC9Z3rta4`) — this is a program executable account, not a token account; it cannot hold or account for SPL tokens sent to it in any useful way.
- **GLC-Solana mint address** (`Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump`) — sending tokens "to the mint" is not a meaningful operation; the mint is not a token account.
- **Deployer wallet** — the deployer keypair pays for and authorizes `initialize`/`initialize_reserve_vault` (`glc-mainnet-bootstrap --deployer-keypair`, `service/src/bin/glc-mainnet-bootstrap.rs`); it becomes `BridgeConfig.admin` at that point (see §7) but is never itself a reserve token account, and the architecture nowhere designates it to receive or hold reserve funds.
- **Admin wallet** — same reasoning; `admin` only ever signs governance/pause instructions (§2), never receives reserve tokens.
- **Submitter wallet** — explicitly "not a custody authority" (§3); it should hold only its own small operational SOL float, never reserve GLC.
- The only correct destinations are: the Goldcoin multisig vault address (§1) and the Solana reserve token account ATA owned by `reserve_authority` (§2) — nothing in the architecture designates any other address as a valid reserve destination.

---

## 5. Normal reserve withdrawal / operator recovery

**Manual reserve withdrawal is not currently implemented.**

Evidence: `programs/glc-reserve-bridge/src/events.rs:110-113` — *"NOTE: rebalance_deposit/rebalance_withdraw instructions (and their corresponding ReserveRebalanced event, docs/05-reserve-accounting.md) are deliberately deferred past this initial Phase 2 pass — see IMPLEMENTATION_LOG.md. Added back when those instructions land."* No such instructions exist anywhere in `programs/glc-reserve-bridge/src/instructions/` (confirmed by the exhaustive instruction list in §2). The `glc-admin rebalance-*` and `custody-*` CLI commands are off-chain evidence-recording state machines only — see §1's "Which CLI/API/admin command performs that action?" for the exact citation that they never construct, sign, or broadcast a real transaction.

The only code-level way GLC-Solana leaves the reserve today is `release_from_reserve`, which is gated by the same attestation-threshold mechanism as a normal bridge payout — it has no "operator withdrawal to an arbitrary address" mode; `recipient`/`recipient_token_account` are bound into the signed attestation claim message itself (`release_claim_message`, `release_from_reserve.rs:167-175`), so a withdrawal to an operator-chosen address would require the attestation signers to sign a claim for that specific destination, same as any other release — not a separate, lesser-gated path.

**Exact code changes that would be needed** for a real, intentional operator-withdrawal mechanism:
1. **On-chain (Solana)**: a new instruction (e.g., `rebalance_withdraw`) in `programs/glc-reserve-bridge/src/instructions/`, structurally similar to `release_from_reserve` but for operator-initiated rebalancing rather than bridge settlement — needs its own authorization model (the `events.rs` note references this as already named/planned but not built), its own account context, and a `ReserveRebalanced` event (referenced but not yet defined in `events.rs`).
2. **On-chain (Goldcoin)**: Goldcoin has no program layer (`docs/02-trust-model.md`'s asymmetry note) — a real withdrawal there is inherently just "build and broadcast a normal multisig-spend transaction," which is possible today with the existing `MultisigVault`/signing primitives (§1) but has no purpose-built CLI wrapper; building one would mean a new `glc-admin`/standalone tool that builds a payout transaction to an arbitrary (operator-approved) destination and drives the same `M`-of-`N` independent-signing flow `orchestrator.rs` uses for bridge payouts.
3. **Off-chain**: wiring the existing `rebalance-propose`/`approve`/`record-executed`/`confirm` state machine (already built, `service/src/ledger/mod.rs`) to actually invoke the new on-chain instruction / new Goldcoin-withdrawal tool above, instead of only recording evidence of an external transfer.

Until built, the step-by-step runbook the user requested (prerequisites / authorization / exact instruction / destination validation / verification / reconciliation / pause requirements / audit logging) **cannot be written truthfully as a real operational procedure** — doing so would describe a mechanism that does not exist. What *does* exist today, safely, for an intentional, evidenced reserve transfer:
1. Pause the relevant reserve (`glc-admin onchain-pause`/`glc-admin pause`, per §1's custody-transition precondition).
2. Stage `glc-admin rebalance-propose`/`custody-propose` and collect the configured approvals.
3. Execute the real transfer entirely outside this system, using whatever real custody tooling holds the relevant keys (the `M` Goldcoin vault signers directly, or a manually-constructed Solana transaction if `release_from_reserve`'s attestation path is used for a bridge-adjacent purpose).
4. Record it (`rebalance-record-executed`/`custody-record-executed`) and confirm (`rebalance-confirm`) so reconciliation doesn't misclassify the change as a breach.
This is real, working tooling — but it evidences a transfer performed by hand, not one this system executes.

---

## 6. Disaster recovery matrix

| Scenario | Reserves safe? | Normal bridging continues? | Reserve funds recoverable? | Required recovery action |
|---|---|---|---|---|
| Bridge core server (the daemon host) lost | Yes — no key material with fund-moving power lives only on this host by design (submitter is a fee payer, not custody; see §3) | No — nothing settles until the daemon is restarted somewhere | Yes, once restarted | Restore/redeploy `glc-bridge-daemon` against the same DB/config; see cold-start sequencing (§4) to avoid a false reconciliation breach |
| Submitter key lost | Yes (§3: "confers no authority") | No — no fee payer for `release_from_reserve` | Yes | Generate/assign a new submitter keypair, fund it with SOL, update `submitter_key_path` |
| Admin key lost | Yes (admin cannot move reserve funds — §2) | Yes, for ordinary deposits/releases (neither requires admin); No, for pause/limit changes | Yes (unaffected) | Requires a program upgrade to install a new admin-recovery path (needs a functioning upgrade authority) — see §2 |
| Deployer key lost | Yes | Yes (deployer's only role is the one-time `initialize`/`initialize_reserve_vault` call, already used) | Yes | None needed post-launch, unless a second reserve-vault init is ever required |
| Upgrade authority lost/revoked | Yes (§2: no effect on reserve funds) | Yes, entirely | Yes | None — but the program becomes permanently un-upgradeable; any future bug/recovery-instruction need becomes unfixable |
| One Goldcoin vault signer lost | Yes | Yes — 2 of 3 still meets `vault_threshold` (§1) | Yes | Restore or rotate that one signer at leisure |
| Two Goldcoin vault signers lost | Yes (funds remain, just unspendable) | No — Goldcoin payouts cannot be built (§1) | Only if the lost key material itself is recoverable from backup; otherwise no | Recover signer key backups, or (last resort) a purpose-built recovery path — none exists in code today (§1, §5) |
| All Goldcoin signer servers lost | Yes (same as above) | No | Same as above — depends entirely on out-of-band key backups | Same as above |
| Solana RPC unavailable | Yes | No — nothing can be submitted/read | Yes, once RPC access is restored | Point `config.toml`'s `[solana].rpc_url` (or `GLC_BRIDGE_SOLANA_RPC_URL`) at a working endpoint |
| Goldcoin RPC unavailable | Yes | No | Yes, once restored | Point `[goldcoin].rpc_url`/env override at a working node |
| Database (ledger) lost | Yes (on-chain/on-Goldcoin state is unaffected) | No, until restored | Yes | Restore from `scripts/backup-ledger.sh` output via `scripts/restore-ledger.sh`, per `docs/09-runbook.md` |
| Program paused (global or release) | Yes | No new releases (deposits still work unless deposit-scope also paused) — §2 | Yes, once unpaused | Admin calls `glc-admin onchain-unpause` with a note (§2) |
| Program upgrade authority revoked | Yes | Yes, unaffected | Yes | Same as "upgrade authority lost" above |
| Fee payer (submitter) has zero SOL | Yes | No new releases (deposits are user-paid, unaffected — §3) | Yes | Top up the submitter wallet's SOL balance |

---

## 7. Security-critical key inventory

Roles and public identifiers only — no private key material, seed phrases, tokens, or secrets appear anywhere in this document or in this codebase's committed files (§9 secret-scan note).

| Role | What it controls | Where configured | Notes |
|---|---|---|---|
| **Deployer** | One-time `initialize`/`initialize_reserve_vault` calls | `glc-mainnet-bootstrap --deployer-keypair` | Its pubkey becomes `admin` at `initialize` time (`programs/glc-reserve-bridge/src/instructions/initialize.rs`) |
| **Upgrade authority** | Whether the program can ever be upgraded | External to this repo until `accept_upgrade_authority` is called; thereafter the `upgrade_authority` PDA (`SEED_UPGRADE_AUTHORITY = b"upgrade_authority"`, `constants.rs:42`), itself gated by admin-proposed + 172,800s-timelocked + permissionless-execute (`upgrade_timelock.rs`) | Has **no** authority over reserve funds, pause state, or attestation keys — upgrade-only |
| **Admin** | `set_paused`, `set_limit`, `transfer_admin`, `propose_upgrade`/`cancel_upgrade` (propose/cancel only), one-time reserve-vault init | `BridgeConfig.admin` (on-chain), set at `initialize` | Cannot move reserve funds, cannot rotate attestation keys, cannot execute an upgrade unilaterally (timelock + permissionless-execute) |
| **Submitter / fee payer** | Pays SOL fees + rent for `release_from_reserve` calls | `operators.submitter_key_path` in `config.toml` | Explicitly "confers no authority" (`release_from_reserve.rs:47-49`) |
| **Goldcoin attestation signers** (2-of-3, pilot) | Authorize *Solana-side* `release_from_reserve` by attesting to a verified Goldcoin deposit (ed25519 precompile signature over `release_claim_message`) | `operators.attestation_pubkeys`/`attestation_threshold`, `operators.attestation_key_paths` (dev) or `attestation_remote_signers` (production) | Also the same threshold gates `propose_attestation_key_rotation` (`governance.rs:1-13`: "cannot be admin-gated") — **UNCONFIRMED as real production keys**, see `docs/09-runbook.md`'s "Attestation signer provenance" section: no repository evidence confirms any specific 3 pubkeys as the real production set |
| **Goldcoin vault signers** (2-of-3, pilot) | Authorize outgoing *Goldcoin-side* multisig spends | `operators.vault_pubkeys`/`vault_threshold`, `operators.vault_key_paths` (dev) or `vault_remote_signers` (production) | Distinct key set from attestation signers in the config schema — nothing in this repo states or requires they be the same physical operators. **Also unconfirmed as real production keys** — no vault pubkeys appear anywhere in this repo's documentation at all, placeholder or otherwise |
| **`reserve_authority` PDA** | Sole SPL authority over the Solana reserve token account | Derived, `seeds = [b"reserve_authority"]` | No private key exists — structurally impossible, not merely undisclosed |
| **`upgrade_authority` PDA** | Target of the timelocked upgrade mechanism, once accepted | Derived, `seeds = [b"upgrade_authority"]` | Same "no private key" property; inert until `accept_upgrade_authority` is called by the real external authority |

---

## 8. Recommendations

Ranked by what most directly threatens fund safety or recoverability before real reserves are deposited:

1. **No HSM/KMS backend exists for either signer group.** `service/src/signing/goldcoin_vault.rs`'s own module doc states this plainly, and the equivalent for attestation signing (`service/src/signing/remote.rs`) implements the *client* protocol for `production` mode but the actual signing endpoint — the thing holding the key and deciding whether to sign — is external and unbuilt by this repository. **Before funding, confirm real HSM/KMS-backed (or equivalent hardware-isolated) signer endpoints exist and are reachable in `production` mode** — `operators.mode = "dev"` must never be used for a real deployment (the code comment already says this; it is a configuration discipline risk, not a code gap).
2. **Losing `vault_threshold`-many Goldcoin signer keys (2 of 3) permanently freezes that side's funds, with no in-repo recovery path.** This is an inherent property of a plain multisig with no on-chain program layer, not a defect — but it means **key backup/escrow procedure for the Goldcoin vault signers is not optional**. Confirm a real backup procedure exists for each of the 3 vault keys before funding, since this document found none referenced in the codebase.
3. **Losing `attestation_threshold`-many attestation keys (2 of 3) freezes the Solana release path**, recoverable only via a program upgrade adding new logic — which itself requires a functioning, *accepted* upgrade authority. **The upgrade-authority interim posture is still an open, undocumented decision** (per this session's own earlier finding, `docs/22-production-readiness-review.md`'s Pilot Launch Policy B-list item) — if the upgrade authority is never accepted/established, the "recover via upgrade" escape hatch for a lost attestation threshold does not actually exist in practice. **Resolve the upgrade-authority posture before funding**, specifically so this escape hatch is real, not theoretical.
4. **Attestation and vault pubkeys are both unconfirmed as real production values** (§7) — funding the reserve before real, verified production keys are in place would mean the ~$400 pilot reserve is secured by whatever placeholder/example keys happen to be in a config file, if any. **Do not fund until the exact production attestation and vault pubkeys are confirmed and independently verified** (e.g., each signer/custody-domain operator confirms their own pubkey out of band).
5. **Could the project owner become unable to recover reserve funds?** Yes, in the specific scenario of losing threshold-many keys in either group with no working backup and no accepted/functional upgrade authority — this is the single largest tail risk this inspection found. It is a designed tradeoff (no single key can steal funds) but its mirror cost (a threshold-loss can freeze funds indefinitely) should be explicitly acknowledged and accepted by whoever approves funding, not discovered after the fact.
6. **Could a single lost key strand funds?** No — both custody mechanisms require losing at least 2 of 3 keys, by design (`MIN_THRESHOLD: u8 = 2` in both `vault.rs:23` and `validation.rs:18`). A single lost key in either group is a non-event operationally.
7. **Is emergency withdrawal adequately protected?** There is no emergency withdrawal mechanism at all (§5) — so the question of whether it's "adequately protected" doesn't yet apply; what exists instead is the normal bridge-release path (attestation-gated) for Solana and a plain multisig spend (vault-signer-gated) for Goldcoin, both already threshold-protected. If a dedicated emergency-withdrawal instruction is ever built (§5's suggested code changes), it should be held to at least the same threshold + pause + timelock bar as normal operations — not a weaker "admin can drain in an emergency" shortcut, which would reintroduce the single-point-of-failure this architecture otherwise avoids.
8. **Are upgrade/admin powers too centralized?** Admin is a single key today (no threshold), controlling pause and limit changes — a real operational risk to availability (an admin-key loss cannot steal funds, but can leave the bridge unable to pause in a real emergency until a governance/upgrade recovery is performed). Given the pilot's proportional-risk policy already treats "full threshold-custody activation" for admin/upgrade as a scale gate rather than a pilot blocker (`docs/22`'s Pilot Launch Policy), this is a known, accepted tradeoff for the current bounded pilot size — but it should be revisited before scaling reserves meaningfully past the pilot amount.
9. **Should operational SOL be kept in a separate fee-payer wallet?** Yes — and it already is, structurally: `submitter` is architecturally distinct from `admin`/`deployer`/reserve custody (§3), and is explicitly documented as "not a custody authority." This is correctly designed already; the only gap is operational (no automated low-balance alert was found for the submitter wallet specifically — worth adding to monitoring before launch, though `ops::alerting` exists generically for other conditions per earlier project documentation).
10. **Do documented backups/recovery procedures exist?** For the ledger database: yes, real and tested (`scripts/backup-ledger.sh`/`restore-ledger.sh`, `docs/09-runbook.md`). For signer key material (both attestation and vault groups): **no backup/recovery procedure was found documented anywhere in this codebase** — this is the most concrete, actionable gap this inspection surfaced.

---

## 9. Evidence

Citations are given inline throughout §§1-8, each as file path + function/struct/instruction name (and line numbers where the reference is narrow enough to pin precisely). No claim in this document rests on an uncited assumption about how the system "probably" works. Where a question could not be answered from the code (e.g., real key provenance, backup procedures for signer keys), this document says so explicitly rather than inferring an answer.

Key files referenced throughout, for convenience:
- `programs/glc-reserve-bridge/src/instructions/release_from_reserve.rs` — the only reserve-outflow instruction (Solana)
- `programs/glc-reserve-bridge/src/instructions/deposit_to_reserve.rs` — reserve inflow (Solana)
- `programs/glc-reserve-bridge/src/instructions/reserve_vault.rs` — one-time reserve setup, PDA authority
- `programs/glc-reserve-bridge/src/instructions/admin.rs` — pause/limit changes
- `programs/glc-reserve-bridge/src/instructions/governance.rs` — threshold-gated attestation-key rotation
- `programs/glc-reserve-bridge/src/instructions/upgrade_timelock.rs` — upgrade authority mechanism
- `programs/glc-reserve-bridge/src/constants.rs` — every PDA seed
- `programs/glc-reserve-bridge/src/validation.rs` — `MIN_THRESHOLD`, key-set validation
- `programs/glc-reserve-bridge/src/events.rs` — confirms `rebalance_deposit`/`rebalance_withdraw` are not yet built
- `service/src/goldcoin/vault.rs` — P2SH multisig construction
- `service/src/signing/goldcoin_vault.rs` — independent-re-derivation signing flow, HSM/KMS status
- `service/src/orchestrator.rs` — threshold-signature collection for both settlement directions
- `service/src/config.rs` — `operators.*` schema, `SignerMode`, submitter loading
- `service/src/bin/glc-bridge-daemon.rs` — vault construction at runtime
- `service/src/bin/glc-mainnet-bootstrap.rs` — deployer/attestation-key CLI, program-id checks
- `docs/09-runbook.md` — cold-start funding sequence, executable-command inventory, attestation signer provenance, confirmation-depth/reserve-bounds values
- `docs/22-production-readiness-review.md` — approved pilot policy table, Pilot Launch Policy

---

## GO / NO-GO BEFORE RESERVE FUNDING

**NO-GO.**

The core settlement architecture is sound and well-evidenced: no single key can move reserve funds on either chain, the Solana side is protocol-enforced (`transfer_checked`, no minting capability), and the Goldcoin side is a genuine on-chain multisig. That is not what's blocking a GO. What's blocking it is that the custody this architecture depends on is not yet real:

1. **No real HSM/KMS signer backend exists for either the attestation or vault signer groups** — only a `dev`-mode plaintext-key stand-in and an unimplemented `production`-mode remote-signer *client* protocol (§1, §8-1).
2. **Neither the 3 attestation pubkeys nor the 3 vault pubkeys are confirmed as real production values** — the only pubkeys appearing anywhere in this repo are an explicitly-marked-unconfirmed example (§7, §8-4).
3. **The upgrade-authority posture is still undecided/undocumented**, which matters specifically because it is the *only* escape hatch if the attestation threshold is ever lost (§8-3).
4. **No documented backup/recovery procedure exists for either signer key group** — the single largest concrete gap found (§8-10).
5. **Manual/emergency reserve withdrawal is not implemented** — not itself a blocker for a pilot this size (the normal release path is already threshold-protected), but means there is currently no operator-controlled way to move reserve funds for any purpose other than normal bridge settlement (§5).

None of these are architectural defects — they are exactly the "human decisions and real infrastructure, not yet supplied" gaps this session's own readiness checks have already surfaced in other documents. But items 1-4 specifically mean that, as of this commit, the ~$400 pilot reserve would be secured by custody infrastructure that does not yet concretely exist. Resolve 1-4 (item 5 can reasonably wait past initial pilot launch) before depositing production reserves.
