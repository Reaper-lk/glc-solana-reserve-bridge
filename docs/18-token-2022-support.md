# Token-2022 support: the canonical Solana GLC asset and its accounting policy

Approved 2026-08-14 as a security-critical compatibility upgrade, without
changing the approved reserve-backed 1:1 architecture (docs/00-executive-
summary.md, docs/02-trust-model.md, docs/03-architecture.md). This document
is the reference for what changed, why, and the policies it establishes.
It supersedes docs/17-p1-checkpoint.md item 13.1's open decision: Token-2022
support is now built, not merely flagged as a blocker.

## What this bridge is — and is not

This bridge transfers **existing** assets between **pre-funded reserves**.
It is **not** a wrapped-token bridge and **not** a mint/burn bridge. Solana
GLC is not created by this program; it already exists as the canonical
mint below, and this program only ever moves already-existing GLC between
a Solana-side reserve token account and users' own token accounts, 1:1.
The Solana program:

- **does not mint** GLC — there is no mint instruction, no CPI to
  `MintTo`/`MintToChecked` anywhere in this program, and the reserve mint's
  own mint authority was independently verified renounced (`null`) on the
  canonical mint, so no one — including this program — could mint even if
  it tried.
- **does not burn** GLC — no CPI to `Burn`/`BurnChecked`.
- **does not wrap** GLC — there is no second, bridge-issued token; the
  asset a user holds on Solana after a release is the same mint, same
  supply, same fungibility class as before.
- **does not create another token** — this program has never had, and
  still does not have, any instruction that creates a mint.
- **does not require or acquire mint authority** — `initialize_reserve_vault`
  accepts an existing mint's address and program as supplied by the admin;
  it never touches `SetAuthority`, never requires the mint authority to be
  assigned to any PDA this program controls, and functions correctly with
  mint authority permanently `null`, exactly as the canonical mint has it.

GLC -> Solana: native GLC enters the Goldcoin reserve; existing Token-2022
GLC is transferred out of the pre-funded Solana reserve. Solana -> GLC:
existing Token-2022 GLC enters the Solana reserve; existing native GLC is
released from the pre-funded Goldcoin reserve. Both directions are plain
`transfer_checked` calls against a reserve token account this program's
PDA (`reserve_authority`, seed `b"reserve_authority"`, no keypair — see
docs/02-trust-model.md) is the authority over. Supply is conserved by
construction: there is no code path in this program that can increase or
decrease total GLC supply, on either chain.

## The canonical Solana GLC mint

Verified read-only against Solana mainnet (`getAccountInfo`, no
transaction, no wallet — docs/17-p1-checkpoint.md §1-6):

| Field | Value |
|---|---|
| Mint address | `Hn6Kdxs6cJrXDLvArAief8ueTgdZLkRacLPPUZo2pump` |
| **Token program** | **Token-2022** — `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb` |
| Decimals | **6** |
| Supply at verification time | 978,182,574.793857 GLC (978,182,574,793,857 raw units) |
| Mint authority | `null` (renounced) |
| Freeze authority | `null` (renounced) |
| Extensions present | `MetadataPointer` + `TokenMetadata` only (on-chain name "GOLDCOIN", symbol "GLC"; both extensions' `authority`/`updateAuthority` are `null` — frozen, cannot be changed by anyone) |

This program never hardcodes this address, its decimals, or its program
id as compile-time constants that production logic depends on:
`reserve_token_mint` is admin-configured once (`initialize_reserve_vault`)
and read from `BridgeConfig` thereafter; decimals are read live from the
mint account on every transfer (`reserve_mint.decimals`, never a constant);
the owning token program is captured once at the same configuration point
and pinned via an on-chain constraint (below), never assumed. If the
canonical mint's supply, metadata, or (implausibly, since both authorities
are renounced) any other mutable field changes in the future, this
program's behavior does not need a rebuild — only its live reads of that
account.

## Verified program: legacy SPL Token or Token-2022, structurally pinned

The on-chain program uses `anchor_spl::token_interface`
(`InterfaceAccount<Mint>`/`InterfaceAccount<TokenAccount>`/
`Interface<TokenInterface>`) throughout — the same account/CPI types
Anchor provides specifically to support either legacy SPL Token or
Token-2022 without duplicating instruction logic. `token_interface::
transfer_checked` builds a standard `transfer_checked` instruction and
invokes it against whichever program was actually supplied in the
transaction, never a hardcoded program id.

`initialize_reserve_vault` accepts whatever mint the admin supplies and
whatever SPL token program actually owns it at that moment (`token_program`
is not constrained to a fixed id there — that pin does not exist yet).
The handler captures that program into a new `BridgeConfig.reserve_token_program`
field:

```rust
config.reserve_token_program = ctx.accounts.token_program.key();
```

Every later reserve-touching instruction (`deposit_to_reserve`,
`release_from_reserve`) constrains its own `token_program` account against
that stored value:

```rust
#[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]
pub token_program: Interface<'info, TokenInterface>,
```

This is what makes "wrong token program" — including substituting the
*other* legitimate SPL token program for an already-configured reserve —
a **structural, on-chain-enforced rejection**, not an off-chain assumption
or a convention operators must remember to uphold. Associated-token-account
derivation is threaded the same way (`associated_token::token_program =
token_program` on every ATA-constrained account), since the ATA PDA's
seeds include the owning token program — the same `(owner, mint)` pair
resolves to a *different* address under each program, so getting this
wrong is not a cosmetic bug but a real misrouting risk.

Off-chain, `service/src/solana/accounts.rs::verify_reserve_mint_token_program`
performs the equivalent check before this service ever attempts a
transfer: it accepts a mint owned by either program, rejects anything
else, and reports which program it found (`MintBasics::token_program`) so
callers never need a second read to learn it. This is a diagnostic
fail-fast, not the enforcement boundary — the on-chain constraints above
are what actually protect funds; this just gives an operator a clear,
early, specific error instead of a generic on-chain constraint failure.

Adversarial coverage: `programs/glc-reserve-bridge/tests/token2022_adversarial.rs`
exercises real Token-2022 and real legacy SPL vault initialization,
non-admin and double-init rejection, a full real Token-2022 deposit +
release settling 1:1, and three substitution attacks against a real
Token-2022 reserve — the other legitimate program supplied in place of
the configured one (both directions), a real but never-configured mint
supplied as the reserve mint, and a real token account for a different
mint supplied as a deposit source. All fail closed.

## Extension policy: explicit allowlist, checked on every reserve-touching call

Token-2022's extension mechanism is a real, general-purpose way for a
mint or token account to carry behavior this program was never written to
account for — a transfer fee, a transfer hook invoked via CPI, a
permanent delegate that can move funds without the owner's signature,
confidential balances, non-transferability, and more. Any of these would
silently break the 1:1 reserve invariant if simply ignored. The previous
mainnet verification found only benign extensions on the canonical mint,
but this program does not treat "benign today" as "safe to assume
forever" — it inspects and classifies every extension actually present,
every time.

**Rule: allowlist, not denylist.** `programs/glc-reserve-bridge/src/
token_extensions.rs` defines exactly which extensions are accepted:

| Scope | Allowed | Why |
|---|---|---|
| Mint | `MetadataPointer` | Points at the account holding the mint's on-chain metadata (here, the mint itself). Purely informational; never consulted by transfer logic. |
| Mint | `TokenMetadata` | The on-chain name/symbol/URI. Purely informational. |
| Token account | `ImmutableOwner` | The extension every real Token-2022 associated token account carries by default. Only prevents the account's *ownership* from ever being reassigned; no effect on balances, transfer amounts, or who can initiate a transfer with a valid signature/PDA authority — strictly safer than its absence. |

Everything else is rejected. `token_extensions::classify` additionally
gives an exhaustive, reviewable classification of every extension type
Token-2022 currently defines (not relied on by production logic — the
allowlist above is what's actually enforced — but kept as a single,
testable, named list so the review itself is visible and auditable):

- **Unsafe** (rejected, and specifically what Task 2 asked this review to
  pay attention to): `TransferFeeConfig`/`TransferFeeAmount` (would
  silently reduce the amount actually received), `TransferHook`/
  `TransferHookAccount` (arbitrary CPI on every transfer), `PermanentDelegate`
  (a third party could move reserve funds without this program's
  authority), `ConfidentialTransferMint`/`ConfidentialTransferAccount`/
  `ConfidentialTransferFeeConfig`/`ConfidentialTransferFeeAmount`/
  `ConfidentialMintBurn` (balances/amounts not verifiable in cleartext —
  incompatible with `transfer_checked`'s and this program's amount
  accounting entirely), `NonTransferable`/`NonTransferableAccount` (would
  make releases impossible), `InterestBearingConfig` (balance grows
  independent of any transfer, breaking 1:1 accounting), `DefaultAccountState`
  (could default new accounts to frozen).
- **Irrelevant** (reviewed, confirmed no bearing on transfer/accounting
  semantics, but not currently added to the active allowlist since the
  canonical mint doesn't carry them and there is no present reason to):
  `MintCloseAuthority`, `MemoTransfer`, `CpiGuard`, `GroupPointer`,
  `TokenGroup`, `GroupMemberPointer`, `TokenGroupMember`.

Any extension type not named above — including one a future Token-2022
release adds after this review — fails closed as unsupported by
construction (`token_extensions.rs`'s trailing match arm), not silently
allowed.

**Checked on every call, not just once at setup.** A mint's extension
*type set* is fixed at creation for most extensions, but some extension
*authorities* can still change behavior within an already-present
extension after the fact (e.g. a live `TransferFeeConfig` authority
updating the fee rate). `validate_mint_extensions`/
`validate_token_account_extensions` run inside `initialize_reserve_vault`
(the natural onboarding review point) **and** inside every
`deposit_to_reserve`/`release_from_reserve` call, on the mint and on every
token account the instruction touches. A mint that somehow acquired an
unsupported extension after this bridge started using it is caught before
the next transfer, not just at onboarding.

Off-chain, `service/src/solana/accounts.rs` mirrors the same
`SUPPORTED_MINT_EXTENSIONS` allowlist for its own diagnostic check
(`verify_reserve_mint_token_program`) — a deliberate, documented
duplication across the workspace boundary, same discipline already used
for this workspace's account-layout decoders.

## Amount/accounting conversion policy

Goldcoin's native chain uses 8 decimals (a Bitcoin-fork convention,
docs/goldcoin-rpc-notes.md, verified against a real Goldcoin node). The
canonical Solana GLC mint uses 6 decimals. These are genuinely different
units for the same underlying asset, and — found during this work as a
real, severe bug, not a hypothetical — code that passed a raw atomic
amount from one chain straight through to the other as if the units were
identical was off by a factor of 100 in both directions (Goldcoin -> Solana
releases would have moved 100x too much; Solana -> Goldcoin payouts would
have paid out 1/100th of what was deposited).

`service/src/amount_conversion.rs` is now the single, canonical
conversion policy, used at every point a settlement amount actually
crosses from one chain's units to the other's:

- **Widening** (destination has more decimals) is always exact: multiply
  by the scale factor.
- **Narrowing** (destination has fewer decimals — today's real case for
  Goldcoin -> Solana, 8 -> 6) is exact **only** when the source amount's
  low-order digits beyond the destination's precision are zero.
  Otherwise, conversion is **rejected**, never rounded or truncated:
  rounding down would permanently strand the depositor's entitlement to
  the remainder inside the reserve; rounding up would create GLC that was
  never actually deposited anywhere. Both are exactly the "silently round
  value in a way that creates or destroys user entitlement" failure this
  policy exists to prevent.
- Solana decimals are **never hardcoded** in the conversion call sites —
  every caller reads the reserve mint's live `decimals` (from an
  independent RPC read in the attestation-signing path, preserving that
  module's "never trust a handed-in value" discipline; as an ordinary
  parameter elsewhere, matching how `fee_rate_per_kb`/`network`/etc.
  already work).

Applied at exactly these points, and nowhere else (the ledger's own
`bridge_requests.amount_atomic` bookkeeping is left in its existing
per-direction source-chain-native unit — see "Known remaining gap" below):

| Direction | Conversion | Where |
|---|---|---|
| GLC -> Solana | Goldcoin-native (8dp, declared/verified against the real deposit) -> reserve mint's live decimals | `signing::attestation::independently_attest_release` (the signed claim) and `orchestrator::submit_release` (the submitted instruction) — both independently re-derive the same converted amount from the same live mint read, so they always agree |
| Solana -> GLC | Reserve mint's live decimals (folded directly from the real on-chain `WithdrawalObligation`) -> Goldcoin-native (8dp) | `signing::goldcoin_vault`'s payout-plan re-derivation (`IndependentPayoutSource::rederive_plan`), and the matching cross-check in `signing::attestation::independently_attest_completion` against the on-chain obligation's ground-truth amount |

Boundary and round-trip property tests live in `amount_conversion.rs`
itself (exact widening/narrowing, remainder rejection for both a
single-atomic-unit and a partial-remainder case, and a round-trip check
across 10,001 representable amounts). The real-node acceptance suite
(`service/tests/regtest_acceptance.rs`) uses a genuine 6-decimal
Token-2022 throwaway mint specifically so its assertions exercise the
real 8-vs-6 mismatch end to end against a real validator and a real
regtest node, not a degenerate case where both chains happen to agree.

**Known remaining gap, flagged not fixed:** `Ledger::create_request`/
`Ledger::fold_sol_deposit` check a request's declared/folded capacity
directly against the *opposite* chain's reserve balance with no unit
conversion at all. This does not move real funds incorrectly — the fix
above already closes that — but it does mean GLC->Solana capacity is
checked overly conservatively (reserves more headroom than a request
actually needs) and Solana->GLC capacity is checked overly permissively
(under-reserves) whenever the two chains' decimals differ, as they do
today. Fixing this changes what unit reserve balances and the public API's
declared amount are denominated in, system-wide — a reserve-sizing/API-
contract decision, not a bug fix, and is not made unilaterally here (see
docs/12-management-decisions.md's precedent for this kind of carve-out).

## Reserve-account validation (what's checked, and where)

Every value in Task 1's required validation list is enforced on-chain,
fail-closed, on every reserve-touching instruction:

| Check | Enforced by |
|---|---|
| Mint address | `#[account(address = bridge_config.reserve_token_mint @ BridgeError::WrongReserveMint)]` on `reserve_mint` |
| Token-2022 (or legacy SPL) program ID | `#[account(address = bridge_config.reserve_token_program @ BridgeError::WrongTokenProgram)]` on `token_program`, pinned once at `initialize_reserve_vault` |
| Mint ownership (which program actually owns the mint account) | `InterfaceAccount<Mint>` deserialization itself (Anchor's `token_interface::Mint` accepts only accounts owned by one of the two known SPL token programs) combined with the `token_program` pin above |
| Mint decimals from chain state | Read live (`reserve_mint.decimals`), passed to `transfer_checked`, which itself validates that argument against the mint account and errors on any mismatch — never a hardcoded constant |
| Source/destination/reserve token account mint | `associated_token::mint = reserve_mint` on every token account field |
| Token-account ownership/authority | `associated_token::authority = <expected owner>` (the reserve authority PDA, the user, or the recipient, per field) |
| PDA authority (`reserve_authority`) | Seeds-derived, bump-verified (`seeds = [SEED_RESERVE_AUTHORITY], bump = bridge_config.reserve_authority_bump`); `invoke_signed` with those exact seeds is the only way to move reserve funds — no keypair exists for this account (constraint 8: signing keys never stored in the repository — there is nothing to store) |
| Expected reserve account | Derived, not merely checked: `associated_token::mint`/`associated_token::authority`/`associated_token::token_program` together fix the reserve token account's address to exactly one value per configured mint/program |
| Transfer amount precision | `transfer_checked`'s own decimals argument/validation, plus the amount-conversion policy above at the one point a cross-chain amount is computed |
| Extension safety | `token_extensions::validate_mint_extensions`/`validate_token_account_extensions`, on every reserve-touching instruction (see above) |

## Why no mint authority is required

This program never needs to mint, so it never needs mint authority. The
canonical mint's mint authority is independently verified `null`
(permanently renounced) — this program's correctness does not depend on
that fact (it would work identically against a mint whose authority is
still live, since it never calls any authority-gated instruction), but it
is additional, independent confirmation that GLC issuance is not, and
cannot be, controlled by this bridge, its operators, or anyone else
through this program.

## Why bridge operations cannot create supply

Every instruction that moves value (`deposit_to_reserve`,
`release_from_reserve`) is a `transfer_checked` CPI — a transfer moves an
existing balance from one already-existing account to another; it cannot
create a new balance from nothing. There is no `MintTo`/`MintToChecked`
CPI anywhere in this program's source, and there never has been (this
program was designed from the outset as a reserve-transfer bridge, not
adapted from a mint/burn design — docs/00-executive-summary.md,
docs/01-reuse-inventory.md). Total GLC supply — on Solana and on
Goldcoin — is therefore invariant across every operation this program can
perform; the only quantities that change are which *account* holds a
given balance.

## See also

- docs/02-trust-model.md — internal 2-of-3 threshold custody (not
  federation), the trust model this Token-2022 work operates inside
  unchanged.
- docs/16-p0-checkpoint.md, docs/17-p1-checkpoint.md — the read-only
  mainnet mint verification and the decimals-hardcoding fix that preceded
  this work.
- docs/10-threat-model.md — the broader threat model this extends.
