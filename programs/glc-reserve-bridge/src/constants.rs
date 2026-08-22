//! PDA seed constants and protocol-wide limits.

/// Singleton bridge configuration account.
pub const SEED_BRIDGE_CONFIG: &[u8] = b"bridge_config";

/// Singleton internal attestation-key set (docs/02-trust-model.md, approved
/// Option 6: 2-of-3 threshold, three genuinely separate custody domains —
/// NOT a federation validator set). The epoch is a field inside the
/// account, not a seed: the address never changes across key rotations.
pub const SEED_ATTESTATION_KEY_SET: &[u8] = b"attestation_key_set";

/// Data-less PDA that owns the reserve GLC token account. The program signs
/// reserve releases via `invoke_signed`; no keypair exists (constraint 8:
/// signing keys never stored in the repository — there is nothing to
/// store for this authority).
pub const SEED_RESERVE_AUTHORITY: &[u8] = b"reserve_authority";

/// Per-deposit claim PDA (Goldcoin -> Solana direction), additionally
/// seeded with the Goldcoin deposit identity `(txid: [u8; 32],
/// vout: u32 little-endian)`. Existence of this account IS the on-chain
/// replay guard for this direction (docs/10-threat-model.md — the direction
/// that gets a cryptographically-enforced guard, unlike the reverse
/// direction).
pub const SEED_DEPOSIT_CLAIM: &[u8] = b"deposit_claim";

/// Per-obligation record PDA (Solana -> Goldcoin direction), additionally
/// seeded with a monotonically increasing index from `BridgeConfig`.
pub const SEED_WITHDRAWAL_OBLIGATION: &[u8] = b"withdrawal_obligation";

/// Per-withdrawal record PDA for an intentional, operator-initiated reserve
/// rebalance withdrawal (`instructions::rebalance_withdraw`), additionally
/// seeded with an operator-supplied `nonce: u64` (little-endian). Existence
/// of this account IS the on-chain replay guard for this action, same
/// mechanism as [`SEED_DEPOSIT_CLAIM`] — a given nonce can authorize at
/// most one withdrawal, ever.
pub const SEED_REBALANCE_WITHDRAWAL: &[u8] = b"rebalance_withdrawal";

/// Singleton PDA holding a governance action currently inside its timelock
/// window. At most one may be pending at a time (same discipline as the old
/// bridge's `PendingGovernanceAction`, docs/01-reuse-inventory.md).
pub const SEED_GOVERNANCE_ACTION: &[u8] = b"governance_action";

/// Data-less PDA that CAN hold the program's real BPF-loader-v3 upgrade
/// authority (docs/12-management-decisions.md item 3, option (c)). Exactly
/// the `SEED_RESERVE_AUTHORITY` pattern: no keypair ever exists for this
/// address, and there is nothing to store for it in the repository.
/// Whether it ever actually holds real authority in a given deployment is
/// a separate, explicit, one-time decision — see
/// `instructions::upgrade_timelock` module docs.
pub const SEED_UPGRADE_AUTHORITY: &[u8] = b"upgrade_authority";

/// Singleton PDA holding a proposed program upgrade currently inside its
/// timelock window. At most one may be pending at a time — same discipline
/// as [`SEED_GOVERNANCE_ACTION`].
pub const SEED_PENDING_UPGRADE: &[u8] = b"pending_upgrade";

/// Per-direction rolling volume window PDA, seeded additionally with a
/// single direction byte (see `state::Direction`).
pub const SEED_ROLLING_VOLUME_WINDOW: &[u8] = b"rolling_volume_window";

/// Version of the on-chain state layout / protocol semantics. Bumped on any
/// breaking change to account layouts or instruction semantics.
pub const PROTOCOL_VERSION: u8 = 1;

/// Hard cap on the number of internal attestation keys (N). The approved
/// trust model is 2-of-3; this bounds the account size and the ed25519
/// precompile dedup bitmask (u16 -> max 16), with headroom for the
/// custody-domain composition decision in docs/12-management-decisions.md
/// item 2 without implying anything federation-scale is intended.
pub const MAX_ATTESTATION_KEYS: usize = 8;

/// Goldcoin's own native-chain decimals (docs/goldcoin-rpc-notes.md,
/// verified against a real Goldcoin node by the old bridge's engineering
/// work) — NOT the Solana GLC token's decimals, which this program never
/// hardcodes anywhere: `release_from_reserve`/`deposit_to_reserve` both
/// read `reserve_mint.decimals` live from chain state on every call
/// (docs/18-token-2022-support.md). The canonical Solana GLC Token-2022
/// mint is verified to use 6 decimals, genuinely different from this
/// constant. Used only by this crate's own test fixtures (a legacy SPL
/// Token throwaway mint's default decimals, `tests/common/mod.rs`), never
/// by production instruction logic.
pub const GOLDCOIN_DECIMALS: u8 = 8;

/// Maximum byte length of the opaque ASCII Goldcoin destination address
/// stored in a `WithdrawalObligation`.
pub const MAX_GLC_ADDRESS_LEN: usize = 64;
