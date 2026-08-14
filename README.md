# Goldcoin Solana Reserve Bridge

A **1:1 reserve-backed bridge** connecting native **Goldcoin (GLC)** on the Goldcoin L1 blockchain with the existing **GLC token on Solana**.

> **Status:** Architecture and development phase.
> **Do not use with production funds.**

## Overview

The Goldcoin Solana Reserve Bridge is designed to provide direct **1:1 interoperability** between:

* Native GLC on the Goldcoin L1 blockchain
* GLC on Solana

Unlike a mint-and-burn wrapped-token bridge, this architecture uses **pre-funded reserves on both chains**.

The bridge does **not mint or burn tokens as part of normal bridging operations**.

Instead:

```text
1 native GLC  <---->  1 Solana GLC
```

A transfer into the reserve on one blockchain authorizes an equivalent transfer from the available reserve on the destination blockchain after the source transaction has been sufficiently verified.

## Architecture

### Goldcoin L1 → Solana

```text
User
 │
 │ Native GLC
 ▼
Goldcoin L1 Reserve
 │
 │ Source transaction verification
 │
 ▼
Bridge Service
 │
 │ 1:1 settlement authorization
 ▼
Solana GLC Reserve
 │
 │ Existing Solana GLC
 ▼
User's Solana Wallet
```

Example:

```text
User deposits:       1,000 native GLC
User receives:       1,000 Solana GLC
Conversion ratio:    1:1
```

The native GLC remains in the Goldcoin reserve.

Existing Solana GLC is released from the Solana reserve.

No Solana GLC is minted.

---

### Solana → Goldcoin L1

```text
User
 │
 │ Solana GLC
 ▼
Solana GLC Reserve
 │
 │ Source transaction verification
 │
 ▼
Bridge Service
 │
 │ 1:1 settlement authorization
 ▼
Goldcoin L1 Reserve
 │
 │ Native GLC
 ▼
User's Goldcoin Wallet
```

Example:

```text
User deposits:       1,000 Solana GLC
User receives:       1,000 native GLC
Conversion ratio:    1:1
```

The Solana GLC remains in the Solana reserve and becomes available for future L1 → Solana transfers.

No token is burned.

## Reserve Model

Both sides of the bridge maintain dedicated pre-funded reserves.

```text
┌──────────────────────┐
│ Goldcoin L1 Reserve  │
│     Native GLC       │
└──────────┬───────────┘
           │
           │ 1:1 Bridge
           │
┌──────────▼───────────┐
│   Solana Reserve     │
│     Solana GLC       │
└──────────────────────┘
```

Bridge liquidity is therefore finite.

The available capacity in each direction depends on the destination reserve.

For example:

```text
Solana reserve balance:       500,000 GLC
Protected minimum reserve:    100,000 GLC
Pending/reserved liquidity:    50,000 GLC

Available L1 → SOL capacity:   350,000 GLC
```

The bridge must not accept transfers that cannot be fulfilled safely.

## Reserve Management

Reserve levels are intended to cover the largest expected net outflow between operational rebalances.

Reserve policy may consider:

* expected peak bridge volume
* directional net flow
* pending transfers
* operational rebalancing time
* safety margin
* abnormal market conditions

Conceptually:

```text
Minimum Reserve =
    Expected Maximum Net Outflow
  + Pending Settlement Requirements
  + Operational Safety Buffer
```

Reserve thresholds and rebalancing procedures will be defined as part of the production architecture.

## Rebalancing

Bridge activity naturally changes the distribution of reserves.

For example:

```text
Initial state

L1 Reserve        500,000 GLC
SOL Reserve       500,000 GLC
```

After a net 200,000 GLC moves from L1 to Solana:

```text
L1 Reserve        700,000 GLC
SOL Reserve       300,000 GLC
```

Operational rebalancing may therefore be required to restore target liquidity.

The production implementation will include:

* minimum reserve thresholds
* reserve monitoring
* capacity reporting
* alerts
* directional circuit breakers
* documented rebalancing procedures

## Core Safety Invariant

For every successful bridge settlement:

```text
1 confirmed GLC deposited
        =
at most 1 GLC released
```

A source transaction must never authorize more than one successful destination settlement.

The implementation must protect against:

* replay attacks
* duplicate settlement
* double release
* chain reorganizations
* insufficient reserves
* concurrent liquidity exhaustion
* database inconsistencies
* service crashes
* interrupted settlements
* stale requests
* unauthorized reserve access

## Liquidity Reservations

Available reserve balance and spendable bridge capacity are not necessarily the same.

The bridge will track states such as:

```text
total reserve
reserved liquidity
pending settlements
completed settlements
protected minimum reserve
available capacity
```

This prevents multiple simultaneous users from being promised the same reserve liquidity.

## Confirmation and Finality

Destination funds must not be released merely because a source transaction has been observed.

The bridge must wait for the configured source-chain confirmation/finality policy before settlement.

Final confirmation policies for both Goldcoin and Solana will be established during implementation and production testing.

## Trust Model

The previous Goldcoin Solana bridge architecture used a federated threshold model.

This reserve-backed architecture is being designed separately.

The final mechanism responsible for:

1. verifying source-chain deposits, and
2. authorizing destination reserve releases

must be explicitly defined and security-reviewed before production deployment.

The project will not assume that pre-funded reserves alone solve cross-chain verification or authorization.

## Circuit Breakers

The production bridge is expected to support independent directional controls.

For example:

```text
L1 → Solana     ACTIVE
Solana → L1     ACTIVE
```

or:

```text
L1 → Solana     PAUSED — insufficient reserve
Solana → L1     ACTIVE
```

Additional controls may include:

* maximum transfer size
* rolling volume limits
* reserve thresholds
* emergency pause
* reconciliation failure protection
* abnormal withdrawal detection

## Reconciliation

Internal bridge accounting must continuously reconcile against actual blockchain state.

The blockchains—not only the bridge database—remain authoritative for reserve balances and confirmed transactions.

Unexpected discrepancies must result in safe failure behavior and operator alerts rather than silently continuing settlement.

## Monitoring

Production monitoring is expected to cover:

* Goldcoin reserve balance
* Solana reserve balance
* available capacity by direction
* pending settlements
* stale transfers
* confirmation delays
* RPC health
* settlement failures
* reconciliation differences
* reserve thresholds
* service health

## Security

Reserve bridges directly custody assets and must be treated as high-value financial infrastructure.

Production deployment requires appropriate controls around:

* reserve authority
* signing credentials
* key storage
* access control
* infrastructure security
* withdrawal authorization
* rate limiting
* monitoring
* incident response
* recovery procedures
* audit logging

Secrets must never be committed to this repository.

This includes:

```text
private keys
seed phrases
wallet files
production credentials
RPC credentials
API secrets
.env files containing secrets
```

## Relationship to the Previous Bridge

This project is derived from engineering work performed on the original Goldcoin Solana bridge.

Useful infrastructure may be retained where appropriate, including:

* Goldcoin RPC integration
* Solana RPC integration
* transaction confirmation
* persistent state management
* reconciliation
* health monitoring
* administrative tooling
* logging and metrics
* recovery mechanisms
* integration testing
* real-node testing

Components built specifically around federated mint/burn behavior will be independently reviewed before being reused.

The reserve bridge must earn production readiness through its own acceptance testing.

## Development Principles

Changes should follow several principles:

1. **Safety before availability**
2. **Fail closed when settlement cannot be proven**
3. **Never promise unavailable liquidity**
4. **Every payout must map to a unique verified source transaction**
5. **Settlement operations must be idempotent**
6. **Blockchain state must reconcile with internal accounting**
7. **Reserve authority must follow least-privilege principles**
8. **Recovery must not create duplicate payouts**
9. **Critical assumptions must be documented**
10. **No production deployment without full acceptance testing**

## Production Acceptance

Before production deployment, the bridge must demonstrate successful real-node testing of:

```text
L1 → Solana
Solana → L1
```

including:

* normal transfers
* concurrent transfers
* minimum-reserve boundaries
* insufficient liquidity
* duplicate requests
* replay attempts
* service restart during settlement
* RPC interruption
* delayed confirmations
* chain reorganization scenarios where applicable
* reconciliation failures
* directional pause/resume
* reserve replenishment
* stale reservations
* crash recovery
* repeated bidirectional transfers
* exact 1:1 accounting

No manual database editing or manual state repair should be required for a successful normal transfer.

## Current Status

🚧 **Architecture / Development**

This repository is not yet production-ready.

The immediate development stages are:

```text
Architecture
    ↓
Threat Model
    ↓
Reserve & Authorization Design
    ↓
Implementation
    ↓
Unit / Integration Testing
    ↓
Real-Node Testing
    ↓
Failure Injection
    ↓
Recovery Testing
    ↓
Security Review
    ↓
Limited Production Launch
```

## Important Notice

This software is under active development.

Do not send production funds to development bridge addresses or reserve accounts.

Bridge capacity, reserve balances, market liquidity, and the existence of 1:1 conversion mechanisms do not guarantee any particular market price or valuation for GLC on either blockchain.

---

**Goldcoin — One GLC. Two Networks.**
