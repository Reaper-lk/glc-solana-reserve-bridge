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

## Launch gates

Before enabling anything:

1. Production GLC ERC-20 approved.
2. Mainnet bridge contract verified.
3. Contract preflight PASS.
4. Independent 3-domain signer custody established.
5. Separate submitter established and funded.
6. Mainnet gas policy measured and approved.
7. Reserve sizing/limits approved.
8. Contract reserve funded.
9. Mainnet observation-only test completed.
10. Mainnet payout test completed with intentionally small amount.
11. Emergency pause/recovery procedure tested.
12. Operator approval to open a specific route.

Routes are enabled individually. Enabling one route does not authorize
opening any other route.
