use super::*;

use std::collections::{HashMap, HashSet};

fn tx(byte: u8) -> EvmTxHash {
    EvmTxHash::from_bytes([byte; 32])
}

fn block(byte: u8) -> EvmBlockHash {
    EvmBlockHash::from_bytes([byte; 32])
}

// --- The two required distinctness properties ------------------------

#[test]
fn two_logs_from_the_same_transaction_are_distinct_by_log_index() {
    // One transaction emitting the same event twice is two deposits, not
    // one. Keying on the transaction hash alone would fold the second away.
    let first = EvmLogId::new(tx(0xaa), 0);
    let second = EvmLogId::new(tx(0xaa), 1);

    assert_ne!(first, second);
    assert_eq!(first.tx_hash, second.tx_hash);

    let mut set = HashSet::new();
    assert!(set.insert(first));
    assert!(set.insert(second), "the second log must not dedup away");
    assert_eq!(set.len(), 2);
}

#[test]
fn the_same_log_index_in_different_transactions_is_distinct() {
    // Log index 0 exists in essentially every transaction. Keying on the
    // index alone would collapse them all into one.
    let first = EvmLogId::new(tx(0xaa), 0);
    let second = EvmLogId::new(tx(0xbb), 0);

    assert_ne!(first, second);
    assert_eq!(first.log_index, second.log_index);

    let mut set = HashSet::new();
    set.insert(first);
    set.insert(second);
    assert_eq!(set.len(), 2);
}

#[test]
fn the_identical_pair_is_the_same_log() {
    let first = EvmLogId::new(tx(0xaa), 7);
    let second = EvmLogId::new(tx(0xaa), 7);
    assert_eq!(first, second);

    let mut set = HashSet::new();
    set.insert(first);
    set.insert(second);
    assert_eq!(set.len(), 1, "an already-seen log must dedup");

    // And it works as a map key, which is how a replay guard uses it.
    let mut seen: HashMap<EvmLogId, &str> = HashMap::new();
    seen.insert(first, "folded");
    assert_eq!(seen.get(&second), Some(&"folded"));
}

#[test]
fn a_transposed_pair_is_not_the_same_log() {
    // Guards against a constructor whose arguments could be swapped without
    // notice — here the values differ, so a transposition changes identity.
    let correct = EvmLogId::new(tx(0x01), 2);
    let transposed = EvmLogId::new(tx(0x02), 1);
    assert_ne!(correct, transposed);
}

// --- Ordering --------------------------------------------------------

#[test]
fn log_ids_order_by_transaction_then_index() {
    let mut ids = vec![
        EvmLogId::new(tx(0xbb), 0),
        EvmLogId::new(tx(0xaa), 5),
        EvmLogId::new(tx(0xaa), 0),
        EvmLogId::new(tx(0xaa), 1),
    ];
    ids.sort();
    assert_eq!(
        ids,
        vec![
            EvmLogId::new(tx(0xaa), 0),
            EvmLogId::new(tx(0xaa), 1),
            EvmLogId::new(tx(0xaa), 5),
            EvmLogId::new(tx(0xbb), 0),
        ]
    );
}

// --- Location versus identity ----------------------------------------

#[test]
fn a_reorg_changes_the_location_but_not_the_identity() {
    let id = EvmLogId::new(tx(0xaa), 3);
    let before = EvmLogLocation::new(id, 1_000, block(0x11));
    // Same log, re-observed at a different height in a different block.
    let after = EvmLogLocation::new(id, 1_001, block(0x22));

    assert_eq!(before.id(), after.id(), "identity survives a reorg");
    assert_ne!(before, after, "the location does not");
    assert_ne!(before.block_hash, after.block_hash);
}

#[test]
fn the_block_hash_is_part_of_the_location_not_just_the_height() {
    // Two different blocks at the same height — the shape of a reorg. A
    // location keyed on the height alone could not tell them apart.
    let id = EvmLogId::new(tx(0xaa), 0);
    let canonical = EvmLogLocation::new(id, 500, block(0x11));
    let orphaned = EvmLogLocation::new(id, 500, block(0x22));

    assert_ne!(canonical, orphaned);
    assert_eq!(canonical.block_number, orphaned.block_number);

    let mut set = HashSet::new();
    set.insert(canonical);
    set.insert(orphaned);
    assert_eq!(set.len(), 2);
}

#[test]
fn the_chain_order_key_sorts_into_canonical_order() {
    let mut locations = [
        EvmLogLocation::new(EvmLogId::new(tx(0xcc), 0), 1_001, block(0x22)),
        EvmLogLocation::new(EvmLogId::new(tx(0xbb), 4), 1_000, block(0x11)),
        EvmLogLocation::new(EvmLogId::new(tx(0xaa), 1), 1_000, block(0x11)),
    ];
    locations.sort_by_key(|location| location.chain_order_key());
    assert_eq!(
        locations
            .iter()
            .map(|location| location.chain_order_key())
            .collect::<Vec<_>>(),
        vec![(1_000, 1), (1_000, 4), (1_001, 0)]
    );
}

#[test]
fn the_chain_order_key_can_collide_across_blocks_which_is_why_ord_is_absent() {
    // Documents the reason `EvmLogLocation` has no `Ord`: these two are not
    // equal, yet their order keys are identical, so an `Ord` derived from
    // the key would contradict `Eq`.
    let first = EvmLogLocation::new(EvmLogId::new(tx(0xaa), 2), 700, block(0x11));
    let second = EvmLogLocation::new(EvmLogId::new(tx(0xbb), 2), 700, block(0x22));
    assert_eq!(first.chain_order_key(), second.chain_order_key());
    assert_ne!(first, second);
}

// --- Boundary values -------------------------------------------------

#[test]
fn the_zero_log_of_the_zero_transaction_is_representable() {
    // Log index 0 is the common case, not a sentinel, and the zero hash is a
    // real value; neither may be treated as "absent".
    let id = EvmLogId::new(EvmTxHash::ZERO, 0);
    assert_eq!(id.log_index, 0);
    assert!(id.tx_hash.is_zero());
    assert_ne!(id, EvmLogId::new(EvmTxHash::ZERO, 1));
}

#[test]
fn the_extremes_of_the_index_and_height_ranges_are_representable() {
    let id = EvmLogId::new(tx(0xff), u64::MAX);
    let location = EvmLogLocation::new(id, u64::MAX, block(0xff));
    assert_eq!(location.chain_order_key(), (u64::MAX, u64::MAX));
}

// --- Display ---------------------------------------------------------

#[test]
fn display_is_greppable_and_names_every_component() {
    let id = EvmLogId::new(tx(0xaa), 12);
    let expected_tx = format!("0x{}", "aa".repeat(32));
    assert_eq!(id.to_string(), format!("{expected_tx}#12"));

    let location = EvmLogLocation::new(id, 21_000_000, block(0xbb));
    let text = location.to_string();
    assert!(text.contains(&expected_tx), "{text}");
    assert!(text.contains("#12"), "{text}");
    assert!(text.contains("21000000"), "{text}");
    assert!(text.contains(&format!("0x{}", "bb".repeat(32))), "{text}");
}
