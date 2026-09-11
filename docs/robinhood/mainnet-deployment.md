# Goldcoin ↔ Robinhood Chain Mainnet Deployment

## Network

- Network: Robinhood Chain
- EIP-155 Chain ID: 4663
- Gas token: ETH
- Status: DISABLED

## Production deployment

- GLC ERC-20: NOT DEPLOYED / NOT APPROVED
- GlcRobinhoodBridge: `0x1753dDA0256A2cB10B44497ACeA9650A1422f440`
- Deployment block: NOT SET
- Deployment transaction: NOT SET

The bridge contract address above is the deployed production contract.
Recording it here is not an approval: every launch gate below still
applies, and `glc-admin robinhood-preflight --config PATH` is what
establishes that this address is the contract this code was written
against, on the network this deployment expects. Nothing in this
repository has verified it.

## Treasury (V2 constructor argument, decided 2026-09-11)

- TREASURY: `0x1b77C2Aa7cAB2466FB34D814BEEB32007179bE7D`
- Immutable: set at V2 construction, unchangeable afterwards by any key.
- The ONE address `executeTreasuryWithdraw` may pay. Withdrawals to it
  have NO per-transaction, daily, rolling or percentage cap — only the
  accounting constraints (`protectedMinReserve`, unsettled depositor
  principal on chain; `protected_minimum`, `reserved_liquidity`,
  `pending_obligations` in the ledger) and the both-directions-paused
  requirement. See docs/34-robinhood-reserve-withdrawal.md §9.
- NOT yet deployed. The V1 contract above has no treasury and no
  withdrawal entry point.

## V2 deployment (prepared 2026-09-11, NOT executed)

Constructor arguments are V1's, read back from V1's creation transaction
(`0x102b9c2a…86a8`, block 58484762) and its live state, plus the treasury.
Nothing else changes: same token, same three signers, same three
guardians, same protocol ids, same `Limits` as V1 currently holds.

| argument | value |
| --- | --- |
| `token_` | `0xaf0172DDEa4ce60dB3EBab05748A00B14fC8e433` (existing GLC; NEVER redeployed) |
| `signers_` | `0x136B0324Fa342E1BD77d71473a3d1773F9e69E9B`, `0xe0c468380Ccab928D1B83f56D0bDEb37Ac1FeF57`, `0x4f724B3173F7F193f74c28c4CB9584C65271f8E2` |
| `guardians_` | `0x1d934c384E2f1220D3c73d30c15c6194704bE769`, `0x139EbEFFD1E7f4b16d1A0B6B75374ceeBD5411D3`, `0x5CD78C0F06168b7bb74cB9C0ecbc51E043CC4424` |
| protocol ids | 1001, 2001, 3001 |
| `limits_` | V1's live `limits()` at the time of deployment — re-read it, do not copy this table (as of 2026-09-11: inboundMin 100 GLC, inboundMax 20,000 GLC, inboundRollingLimit 5,000,000 GLC, outboundMin 97 GLC, outboundMax 20,000 GLC, outboundRollingLimit 5,000,000 GLC, protectedMinReserve 0) |
| `treasury_` | `0x1b77C2Aa7cAB2466FB34D814BEEB32007179bE7D` |

V2 launches FAIL-CLOSED by construction: `depositsPaused = true`,
`payoutsPaused = true`, every `routeEnabled` false, `governanceNonce = 0`,
`signerEpoch = 0`. Opening any route on it takes a pause-clearing quorum
AND a route-enabling quorum, after the migration has finalized and the
service has been cut over.

```
# From contracts/, with the pinned toolchain (solc 0.8.30, cancun, 200 runs):
forge build
forge create src/GlcRobinhoodBridge.sol:GlcRobinhoodBridge \
  --rpc-url "$ROBINHOOD_RPC_URL" --private-key "$DEPLOYER_KEY" --broadcast \
  --constructor-args \
    0xaf0172DDEa4ce60dB3EBab05748A00B14fC8e433 \
    "[0x136B0324Fa342E1BD77d71473a3d1773F9e69E9B,0xe0c468380Ccab928D1B83f56D0bDEb37Ac1FeF57,0x4f724B3173F7F193f74c28c4CB9584C65271f8E2]" \
    "[0x1d934c384E2f1220D3c73d30c15c6194704bE769,0x139EbEFFD1E7f4b16d1A0B6B75374ceeBD5411D3,0x5CD78C0F06168b7bb74cB9C0ecbc51E043CC4424]" \
    1001 2001 3001 \
    "(100000000000000000000,20000000000000000000000,5000000000000000000000000,97000000000000000000,20000000000000000000000,5000000000000000000000000,0)" \
    0x1b77C2Aa7cAB2466FB34D814BEEB32007179bE7D
```

After deployment, BEFORE anything else: rebuild from the merged commit,
compare `deployedBytecode` against `eth_getCode` (immutables masked,
metadata tail stripped) and `treasury()`, `signers()`, `guardians()`,
`limits()`, `depositsPaused()`, `payoutsPaused()`, `routeEnabled(1..4)`
against this table; record the address, block and tx hash here.

## Migration delay (V2 source, decided 2026-09-11)

- V2 has NO mandatory delay between `commitMigration` and
  `finalizeMigration`; `MIGRATION_DELAY` was removed from the source
  (docs/34-robinhood-reserve-withdrawal.md §10). A migration OUT OF V2 can
  finalize immediately after commit, under its own second 2-of-3 quorum.
- The deployed V1 contract still enforces its own 48-hour delay from its
  bytecode (`MIGRATION_DELAY()` = 172800 on chain). The V1 -> V2 migration
  is therefore a 48-hour migration regardless of this change.
- Because V2 no longer waits for you, a V2 migration MUST be run with an
  agreed hold between the commit and the finalize so the guardian veto
  has a window to land in. See docs/34 §10.2.

## Protocol chain IDs

These are Goldcoin bridge protocol namespace IDs, not EIP-155 IDs.

- Goldcoin: 1001
- Robinhood: 2001
- Solana: 3001

## Authorization

- Signer A: NOT SET
- Signer B: NOT SET
- Signer C: NOT SET
- Threshold: 2-of-3

## Guardians

- Guardian A: NOT SET
- Guardian B: NOT SET
- Guardian C: NOT SET

## Submitter

- Address: NOT SET
- Purpose: gas payment and transaction broadcast only
- Must not be an authorization signer.

## Initial on-chain state

Required immediately after deployment:

- depositsPaused = true
- payoutsPaused = true
- GlcToRhn = false
- RhnToGlc = false
- SolToRhn = false
- RhnToSol = false

## Production daemon state

Until launch approval, `/etc/glc-bridge/config.toml` MUST NOT contain:

- `[robinhood.indexer]`
- `[robinhood.settlement]`
- `[reserve.robinhood]`

This prevents the production daemon from observing, signing, broadcasting,
or accounting Robinhood operations.

## Launch policy

Approved commercial terms for the Goldcoin <-> Robinhood routes. Backend
values are canonical 8-decimal units; contract values are the token's
native 18 decimals.

| Policy | Backend (`[robinhood.policy]`) | Contract `limits()` |
| --- | --- | --- |
| Fee | `fee_bps = 600` (6.00%) | not a contract value |
| Per transfer | `per_transfer_limit = 2000000000000` (20,000 GLC) | `inboundMax` = `outboundMax` = `20000000000000000000000` |
| Rolling 24h (strict) | `rolling_daily_limit = 1000000000000000` (10,000,000 GLC) | `inboundRollingLimit` = `outboundRollingLimit` = `5000000000000000000000000` (5,000,000 GLC) |

The rolling row is the one that is easy to get wrong, so it is stated
twice: **the on-chain rolling limit is HALF the strict policy.**
`GlcRobinhoodBridge`'s window is a fixed bucket that resets wholesale, so
2x the configured limit can move within one 86,400-second span — its
`_consumeWindow` documentation proves the worst case is exactly 2x and is
reachable. Configuring 10,000,000 GLC on chain would make the real
ceiling 20,000,000 GLC / 24h.

`glc-admin robinhood-preflight` FAILS on any disagreement between the two
columns, in either direction, and `glc-bridge-daemon` refuses to start
when the backend column claims the larger limit.

### Changing these limits after deployment

`inboundMax`, `outboundMax`, `inboundRollingLimit` and
`outboundRollingLimit` live in the contract's `Limits` STORAGE struct
(`GlcRobinhoodBridge.sol`, `Limits private _limits`) — not in
`immutable`s and not in `constant`s. They are changed by
`setLimits(Limits calldata newLimits, uint256 nonce, uint64 expiry,
bytes[] calldata signatures)`, which authorizes under
`ACTION_SET_LIMITS = 0x07` through `_governance` -> `_authorize`:
exactly `SIGNER_THRESHOLD` (2) distinct signatures from the 3-address
signer set, over the current `signerEpoch` and the current
`governanceNonce`, before the expiry.

Consequences:

- No redeployment is needed to change any limit, in either direction, by
  any factor.
- The whole `Limits` struct is replaced at once, so signers approve a
  complete policy rather than a delta.
- `_validateLimits` is the only ceiling: mins non-zero, `min <= max`,
  `rollingLimit >= max`, and every field an exact multiple of
  `CANONICAL_SCALE` (1e10). There is NO upper bound on any limit.
- `setLimits` reverts once `migrated` is true.

## Launch gates

Before enabling anything:

1. Production GLC ERC-20 approved.
2. Mainnet bridge contract verified.
3. Contract preflight PASS.
4. Independent 3-domain signer custody established.
5. Separate submitter established and funded.
6. Mainnet gas policy measured and approved.
7. Reserve sizing/limits approved and installed on chain via `setLimits` (see "Launch policy" above).
8. Contract reserve funded.
9. Mainnet observation-only test completed.
10. Mainnet payout test completed with intentionally small amount.
11. Emergency pause/recovery procedure tested.
12. Operator approval to open a specific route.

Routes are enabled individually. Enabling one route does not authorize
opening any other route.
