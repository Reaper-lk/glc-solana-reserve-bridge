//! Types shared between the `glc-reserve-bridge` Anchor program and the
//! off-chain bridge service. See `docs/08-migration-strategy.md`.

pub mod claim;
pub mod governance;

/// The deployed `glc-reserve-bridge` program's address on Solana mainnet
/// (`7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn`), as raw bytes — the
/// single authoritative source of truth this whole workspace's various
/// independent copies of "the program id" are required to agree with
/// (docs/22-production-readiness-review.md P0-6).
///
/// # Why this lives here, and why it's bytes rather than a `Pubkey`
///
/// This crate is compiled into BOTH the on-chain program (the SBF/BPF
/// target) and the off-chain service (a normal host binary) — see this
/// module's own top-level docs and docs/08-migration-strategy.md. It is
/// deliberately kept dependency-free (no `solana-program`/`solana-sdk`),
/// so this constant is a plain `[u8; 32]` rather than a `Pubkey` type, to
/// avoid pulling either SDK into a crate whose whole purpose is staying
/// buildable on both sides without caring which one's SDK it's talking
/// to. Each side converts it into its own `Pubkey` type trivially:
/// - on-chain: `Pubkey::new_from_array(PROGRAM_ID_BYTES)` (or compare
///   against `crate::ID` — see below).
/// - off-chain: `solana_sdk::pubkey::Pubkey::new_from_array(PROGRAM_ID_BYTES)`
///   (`service::solana::accounts::PROGRAM_ID` does exactly this).
///
/// # Why `declare_id!` still has its own separate literal
///
/// Anchor's `declare_id!`/`pubkey!` macros parse a base58 string literal
/// at proc-macro-expansion time — before any type-checking or const
/// evaluation happens — so they cannot take a `const` from another crate
/// as their argument; `programs/glc-reserve-bridge/src/lib.rs` must keep
/// its own literal `declare_id!("7h2zSJuq...")` call. What this constant
/// *does* buy: a same-crate-compile-time-independent regression test
/// (`programs/glc-reserve-bridge/src/lib.rs`'s own `#[cfg(test)]`
/// module) asserts `crate::ID.to_bytes() == PROGRAM_ID_BYTES` on every
/// `cargo test` run, so the two literals (`declare_id!`'s and this one)
/// can never silently drift apart the way the pre-2026-08-19 codebase's
/// on-chain `declare_id!` and off-chain `accounts::PROGRAM_ID` did (both
/// were independently hardcoded to the program's original scaffold/dev
/// id, `BnCFcMaZtpXUzZhXZdQSeQWH4A2BMv5ZaebGe6Ysv2oY`, and neither was
/// ever updated when the program was actually deployed to mainnet at a
/// *different* address — see docs/22-production-readiness-review.md
/// P0-6 for the full incident writeup).
///
/// Computed once via `solana_sdk::pubkey::Pubkey::from_str(
/// "7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn").to_bytes()` and
/// cross-checked against an independent base58 decode — see the PR that
/// introduced this constant for both derivations.
#[rustfmt::skip]
pub const PROGRAM_ID_BYTES: [u8; 32] = [
    99, 103, 154, 182, 10, 30, 57, 99, 0, 225, 186, 155, 178, 139, 98, 243,
    233, 199, 70, 154, 57, 69, 111, 142, 19, 60, 148, 64, 54, 32, 236, 219,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the hand-transcribed byte array above against a transcription
    /// error — decodes the same base58 address independently (a small,
    /// self-contained decoder, deliberately not reusing any code this
    /// constant itself is meant to be validated against) and compares.
    #[test]
    fn program_id_bytes_matches_independent_base58_decode() {
        const ADDRESS: &str = "7h2zSJuqpmbSq4seeXDdaJChVoxhEWwA9b8qG6Ct1GNn";
        const ALPHABET: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

        let mut digits: Vec<u8> = vec![0];
        for ch in ADDRESS.bytes() {
            let value = ALPHABET
                .iter()
                .position(|&c| c == ch)
                .expect("valid base58 character") as u32;
            let mut carry = value;
            for digit in digits.iter_mut() {
                carry += (*digit as u32) * 58;
                *digit = (carry & 0xff) as u8;
                carry >>= 8;
            }
            while carry > 0 {
                digits.push((carry & 0xff) as u8);
                carry >>= 8;
            }
        }
        // No leading '1' characters in this particular address (each would
        // contribute one leading zero byte) — this decoder is intentionally
        // minimal, not a general-purpose base58 implementation.
        assert!(!ADDRESS.starts_with('1'));
        digits.reverse();
        let mut decoded = [0u8; 32];
        let start = 32 - digits.len();
        decoded[start..].copy_from_slice(&digits);
        assert_eq!(decoded, PROGRAM_ID_BYTES);
    }
}
