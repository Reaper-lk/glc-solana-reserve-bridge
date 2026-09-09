# Goldcoin ↔ Robinhood Chain Mainnet Deployment

## Network

- Network: Robinhood Chain
- EIP-155 Chain ID: 4663
- Gas token: ETH
- Status: DISABLED

## Production deployment

- GLC ERC-20: NOT DEPLOYED / NOT APPROVED
- GlcRobinhoodBridge: NOT DEPLOYED
- Deployment block: NOT SET
- Deployment transaction: NOT SET

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
