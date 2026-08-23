//! Deterministic, per-request 2-of-3 P2SH deposit-address derivation.
//!
//! # What this is, and what it is deliberately NOT (yet)
//!
//! This module is Step 1 of the unique-deposit-address redesign: it
//! derives a fresh, request-specific [`MultisigVault`] from the existing
//! three root signer pubkeys, and proves (via this module's own tests)
//! that the corresponding tweaked SECRET key material is equally
//! derivable. It does **not** touch the indexer, the API, payout
//! construction, or any [`crate::signing`] trait/implementation — those
//! are later steps, done only after this derivation itself is reviewed.
//!
//! # The derivation, precisely
//!
//! For a given `request_id` (the `bridge_requests.id` — already unique,
//! permanent, and never reused, since it is a SQLite rowid; reused
//! directly as the derivation index, no separate counter invented):
//!
//! ```text
//! tweak = SHA256("glc-bridge-deposit" || request_id.to_le_bytes()) mod n
//! for each root pubkey P_j:      P_j' = P_j + tweak * G   (EC point addition)
//! for the matching root seckey:  d_j' = d_j + tweak (mod n)
//! ```
//!
//! `tweak` is computed from public information only (`request_id` is
//! already public today, embedded in the existing OP_RETURN scheme) —
//! deriving the new address never needs, and never touches, any secret
//! key material. [`derive_request_vault`] does exactly the pubkey half;
//! [`derive_request_seckey`] does the seckey half, and this module's own
//! tests prove the two halves agree (`P_j' == pubkey_of(d_j')`) — i.e. a
//! signer holding only its own root secret key CAN deterministically
//! reconstruct the exact secret key needed to sign for any request's
//! derived address, without needing any of the other two signers'
//! material and without this module ever seeing a real secret key
//! itself.
//!
//! This is the same "unhardened public-key derivation" primitive BIP32
//! uses (minus BIP32's chaincode/hardened-path machinery, which this
//! doesn't need — a fixed set of 3 signer keys, not a hierarchical
//! wallet, per the explicit "smallest correct implementation" brief).
//! [`MultisigVault::new`] (`vault.rs`) is unmodified and reused as-is —
//! it already builds a correct 2-of-3 P2SH script from any 3 valid
//! pubkeys, tweaked or not, so this module contains no script-building
//! or address-encoding logic of its own.
//!
//! # Independent verification
//!
//! Every constant and every derived value in this module's tests was
//! cross-checked against a from-scratch Python re-implementation of
//! secp256k1 point arithmetic and Base58Check encoding (no shared code,
//! no third-party crypto library — none were available in the
//! verification environment) — never against this module's own logic,
//! and never against a hand-recalled "well-known" constant (a first
//! draft of that verification script had a memorized generator-point
//! coordinate silently missing its last hex digit; every constant now
//! traces back to a live `libsecp256k1`/`vault.rs` computation instead
//! of memory). See the session record for the full cross-check script
//! and output; the specific pinned golden vectors below are its result.

use libsecp256k1::{PublicKey, SecretKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::address::Network;
use super::vault::{MultisigVault, VaultError};

const TWEAK_DOMAIN: &[u8] = b"glc-bridge-deposit";

#[derive(Debug, Error)]
pub enum DerivationError {
    #[error("derived pubkey/redeem script is invalid: {0}")]
    Vault(#[from] VaultError),
    /// `tweak_add_assign` can fail only if the tweak happens to be exactly
    /// the scalar that cancels the point out to infinity (or is `>= n`,
    /// which [`request_tweak`] already cannot produce since a SHA-256
    /// digest reduced mod n is always in range) — a cryptographically
    /// negligible, not-realistically-reachable event. Surfaced as a hard
    /// error rather than silently retried or ignored: this deliberately
    /// fails closed rather than guessing a fallback address.
    #[error("EC tweak operation failed for request_id {request_id} (cryptographically negligible; treat as a bug if ever actually seen)")]
    TweakFailed { request_id: i64 },
}

/// Deterministic per-request scalar tweak, computable from `request_id`
/// alone (no secret material). Must be identical to whatever a future
/// signer implementation uses to derive its own tweaked secret key
/// (`derive_request_seckey` below is that reference implementation).
pub fn request_tweak(request_id: i64) -> SecretKey {
    let mut msg = Vec::with_capacity(TWEAK_DOMAIN.len() + 8);
    msg.extend_from_slice(TWEAK_DOMAIN);
    msg.extend_from_slice(&request_id.to_le_bytes());
    let digest = Sha256::digest(&msg);
    // A SHA-256 digest interpreted as a 256-bit big-endian integer is
    // `>= n` (secp256k1's group order) with probability ~2^-128 -- for
    // the extraordinarily unlikely case it ever happens,
    // `parse_slice` fails and this would panic; accepted here exactly
    // once, at the single point this scalar is minted, rather than
    // threading a fallible constructor through every caller for a case
    // that will not occur in the lifetime of this pilot.
    SecretKey::parse_slice(&digest)
        .expect("SHA-256 digest as scalar is astronomically unlikely to be >= group order")
}

/// Derives the request-specific 2-of-3 [`MultisigVault`] from `root`'s
/// own signer pubkeys — pure public-key math, no secret material
/// involved or required. `root.threshold` and signer count are preserved
/// exactly (still 2-of-3, same custody model, just different keys).
pub fn derive_request_vault(
    root: &MultisigVault,
    request_id: i64,
    network: Network,
) -> Result<MultisigVault, DerivationError> {
    let tweak = request_tweak(request_id);
    let mut tweaked_pubkeys = Vec::with_capacity(root.signer_pubkeys.len());
    for pk_bytes in &root.signer_pubkeys {
        // `root.signer_pubkeys` were already validated as well-formed
        // compressed secp256k1 points by `MultisigVault::new`/
        // `from_redeem_script_hex` at construction time -- re-parsing
        // here cannot fail on malformed input, only (negligibly) on the
        // tweak itself, per `request_tweak`'s own docs.
        let mut pk = PublicKey::parse_compressed(pk_bytes)
            .map_err(|_| DerivationError::TweakFailed { request_id })?;
        pk.tweak_add_assign(&tweak)
            .map_err(|_| DerivationError::TweakFailed { request_id })?;
        tweaked_pubkeys.push(pk.serialize_compressed());
    }
    Ok(MultisigVault::new(
        tweaked_pubkeys,
        root.threshold,
        network,
    )?)
}

/// Derives the request-specific tweaked SECRET key for one signer, given
/// that signer's own ROOT secret key. This is the exact operation a real
/// signer implementation will need at signing time (not wired into
/// [`crate::signing::signers::VaultSigner`] yet — that is a later,
/// separate step) — included and tested here to prove the capability is
/// sound in isolation first. Never called with, and this module never
/// otherwise touches, real production key material.
pub fn derive_request_seckey(
    root_seckey: &SecretKey,
    request_id: i64,
) -> Result<SecretKey, DerivationError> {
    let tweak = request_tweak(request_id);
    let mut sk = *root_seckey;
    sk.tweak_add_assign(&tweak)
        .map_err(|_| DerivationError::TweakFailed { request_id })?;
    Ok(sk)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// vault.rs's own real, node-verified golden vector (see that
    /// module's `REAL_REDEEM_SCRIPT`/`REAL_ADDRESS`) -- reused here
    /// verbatim as the root vault so these tests derive from a real,
    /// independently-verified starting point, not a fabricated one.
    const REAL_REDEEM_SCRIPT: &str = "5221028e7147e643d67093dc8ca6a8fb888f1a452dddc62de991c7ed72080d65a421e42102f1c88ca7176c3ffee952ee6fae697991b257b6d53c3bc88e81cfe99adbcdbee5210256220bb7865197a40c4590ac80f12ef18e9063eac2eff92c4476ec27034042f953ae";
    const REAL_ADDRESS: &str = "QY9YcpypWD91BEZ37TjNHYoqrquhcnVBYV";

    fn real_root_vault() -> MultisigVault {
        let v =
            MultisigVault::from_redeem_script_hex(REAL_REDEEM_SCRIPT, Network::Testnet).unwrap();
        assert_eq!(
            v.address(),
            REAL_ADDRESS,
            "sanity: root vector must still be the real one"
        );
        v
    }

    /// Golden vectors, independently cross-checked (see module docs) —
    /// `(request_id, expected_testnet_address, expected_mainnet_address)`.
    /// Any change to the derivation algorithm that alters these values is
    /// a breaking change and must be re-verified independently before
    /// this list is ever updated.
    const GOLDEN_VECTORS: &[(i64, &str, &str)] = &[
        (
            1,
            "QetrJtMHg7FepEmc6WpimFzb8aSZ3CBqth",
            "MSC2S1xyzfYeGmeuuAAAtFpJ6YP1NmBjhB",
        ),
        (
            2,
            "QRNkJM5rEWk7y1pkYu3FGYdaZDwnkeJGY1",
            "MCfvRUhYZ537RYi4MYNhPYTHXBtF3759HA",
        ),
        (
            42,
            "QhgmVLi4Y9eYGuZCT7ATAsMt63TamAgwsh",
            "MUywcUKkrhwXjSSWFkVuHsBb41Q35398QT",
        ),
        (
            1_000_000,
            "QTGSE7N2Qy4SbaVjLhZwKWkHhYNqLNxAkp",
            "MEZcMEyijXMS47P39LuPSWZzfWKHbXLePy",
        ),
    ];

    #[test]
    fn golden_vectors_match_independent_cross_check_testnet() {
        let root = real_root_vault();
        for &(request_id, expected_testnet, _) in GOLDEN_VECTORS {
            let derived = derive_request_vault(&root, request_id, Network::Testnet).unwrap();
            assert_eq!(
                derived.address(),
                expected_testnet,
                "request_id={request_id}: derived testnet address must match the independently cross-checked vector"
            );
        }
    }

    #[test]
    fn golden_vectors_match_independent_cross_check_mainnet() {
        let root = real_root_vault();
        for &(request_id, _, expected_mainnet) in GOLDEN_VECTORS {
            let derived = derive_request_vault(&root, request_id, Network::Mainnet).unwrap();
            assert_eq!(
                derived.address(),
                expected_mainnet,
                "request_id={request_id}: derived mainnet address must match the independently cross-checked vector"
            );
        }
    }

    #[test]
    fn derived_vault_preserves_threshold_and_signer_count() {
        let root = real_root_vault();
        let derived = derive_request_vault(&root, 7, Network::Testnet).unwrap();
        assert_eq!(
            derived.threshold, root.threshold,
            "still 2-of-3, not some other threshold"
        );
        assert_eq!(
            derived.signer_count(),
            root.signer_count(),
            "still 3 signers"
        );
        assert_eq!(root.threshold, 2);
        assert_eq!(root.signer_count(), 3);
    }

    #[test]
    fn derivation_is_deterministic() {
        let root = real_root_vault();
        let a = derive_request_vault(&root, 12345, Network::Testnet).unwrap();
        let b = derive_request_vault(&root, 12345, Network::Testnet).unwrap();
        assert_eq!(a.address(), b.address());
        assert_eq!(a.redeem_script_hex(), b.redeem_script_hex());
    }

    #[test]
    fn different_request_ids_derive_different_addresses() {
        let root = real_root_vault();
        let mut addresses = std::collections::HashSet::new();
        for request_id in 1..200i64 {
            let derived = derive_request_vault(&root, request_id, Network::Testnet).unwrap();
            assert!(
                addresses.insert(derived.address().to_string()),
                "request_id={request_id} collided with an earlier request_id's derived address"
            );
        }
    }

    #[test]
    fn derived_address_never_equals_the_root_vault_address() {
        let root = real_root_vault();
        for request_id in [1, 2, 42, 1_000_000] {
            let derived = derive_request_vault(&root, request_id, Network::Testnet).unwrap();
            assert_ne!(
                derived.address(),
                root.address(),
                "a derived address must never coincide with the shared legacy vault address"
            );
        }
    }

    #[test]
    fn different_networks_give_different_addresses_for_the_same_request() {
        let root = real_root_vault();
        let testnet = derive_request_vault(&root, 1, Network::Testnet).unwrap();
        let mainnet = derive_request_vault(&root, 1, Network::Mainnet).unwrap();
        assert_ne!(testnet.address(), mainnet.address());
        // Same underlying tweaked pubkeys/redeem script, only the version
        // byte (and thus the address encoding) differs.
        assert_eq!(testnet.redeem_script_hex(), mainnet.redeem_script_hex());
    }

    /// The core safety property this whole module exists to prove: given
    /// only ONE signer's own root secret key (never the other two), that
    /// signer can deterministically derive the exact secret key needed to
    /// sign for a specific request's derived address -- and the public
    /// key of that derived secret key matches what `derive_request_vault`
    /// (the public-only path) independently computed for the same
    /// signer position. A synthetic, clearly non-production test key
    /// stands in for the real root secret; this module never sees or
    /// needs real key material to prove the property.
    #[test]
    fn each_signer_can_derive_its_own_request_specific_secret_key() {
        // Synthetic test-only secret key, reproducibly derived from a
        // fixed label (never a real signer's key) -- documented so
        // anyone can regenerate/verify it independently rather than
        // trusting a hand-typed constant.
        let seed = Sha256::digest(b"glc-bridge-derivation-test-only-synthetic-root-key");
        let root_seckey = SecretKey::parse_slice(&seed).unwrap();
        let root_pubkey = PublicKey::from_secret_key(&root_seckey);

        for request_id in [1i64, 2, 42, 1_000_000] {
            let tweak = request_tweak(request_id);

            // Public-only path: tweak the pubkey directly.
            let mut pubkey_via_public_path = root_pubkey;
            pubkey_via_public_path.tweak_add_assign(&tweak).unwrap();

            // Private path: a signer, holding only its own root secret,
            // derives the request-specific secret key and computes ITS
            // public key.
            let derived_seckey = derive_request_seckey(&root_seckey, request_id).unwrap();
            let pubkey_via_private_path = PublicKey::from_secret_key(&derived_seckey);

            assert_eq!(
                pubkey_via_public_path, pubkey_via_private_path,
                "request_id={request_id}: a signer's own private-path derivation must reach \
                 exactly the pubkey the public-only path (used to build the deposit address) \
                 committed to -- otherwise funds sent to the derived address could not later be \
                 signed for"
            );
        }
    }

    #[test]
    fn tweak_is_deterministic_and_request_id_dependent() {
        assert_eq!(request_tweak(1), request_tweak(1));
        assert_ne!(request_tweak(1).serialize(), request_tweak(2).serialize());
    }
}
