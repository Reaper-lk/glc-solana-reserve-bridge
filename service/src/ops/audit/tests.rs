use super::*;
use crate::ledger::{CreateRequestOutcome, Direction, ReserveDirection};
use glc_reserve_bridge_shared::claim::release_claim_message;

fn temp_ledger() -> (tempfile::TempDir, std::path::PathBuf, Ledger) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let ledger = Ledger::open(&path).unwrap();
    (dir, path, ledger)
}

fn finalized_glc_to_sol_request(ledger: &mut Ledger) -> (i64, [u8; 32], u32, u64, [u8; 32]) {
    ledger
        .configure_reserve(
            ReserveDirection::SolanaReserve,
            10_000_000,
            0,
            5_000_000,
            2_000_000,
            1_000_000,
            0,
        )
        .unwrap();
    let recipient = [9u8; 32];
    let CreateRequestOutcome::Reserved { request_id } = ledger
        .create_request(Direction::GlcToSol, 500_000, &recipient, None, 3600, 0)
        .unwrap()
    else {
        panic!()
    };
    let txid = [0xAAu8; 32];
    let vout = 2u32;
    ledger
        .record_glc_deposit_observed(request_id, txid, vout, 500_000, 10, [0u8; 32], 0)
        .unwrap();
    ledger.mark_glc_source_finalized(request_id, 0).unwrap();
    (request_id, txid, vout, 500_000, recipient)
}

#[test]
fn a_freshly_recorded_attestation_audits_clean() {
    let (_dir, _path, mut ledger) = temp_ledger();
    let (request_id, txid, vout, amount, recipient) = finalized_glc_to_sol_request(&mut ledger);
    let message = release_claim_message(
        1,
        &[0x11; 32],
        5,
        &txid,
        vout,
        amount,
        &recipient,
        &[0x22; 32],
    );
    ledger
        .record_attestation(request_id, "release", &message, 0)
        .unwrap();

    let report = audit(&ledger).unwrap();
    assert!(report.is_clean(), "{:?}", report.findings);
    assert_eq!(report.records_checked, 1);
    assert_eq!(report.integrity_check, "ok");
}

#[test]
fn a_tampered_hash_is_caught_as_self_inconsistent() {
    let (_dir, path, mut ledger) = temp_ledger();
    let (request_id, txid, vout, amount, recipient) = finalized_glc_to_sol_request(&mut ledger);
    let message = release_claim_message(
        1,
        &[0x11; 32],
        5,
        &txid,
        vout,
        amount,
        &recipient,
        &[0x22; 32],
    );
    ledger
        .record_attestation(request_id, "release", &message, 0)
        .unwrap();
    drop(ledger);

    // Simulate an altered stored hash — a raw connection standing in for
    // "the file was tampered with between writes", the exact scenario an
    // offline audit exists to catch.
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute(
        "UPDATE attestation_records SET message_hash = X'00' WHERE request_id = ?1",
        [request_id],
    )
    .unwrap();
    drop(raw);

    let ledger = Ledger::open(&path).unwrap();
    let report = audit(&ledger).unwrap();
    assert!(!report.is_clean());
    assert!(matches!(
        report.findings[0],
        Finding::SelfInconsistent { request_id: r, .. } if r == request_id
    ));
}

#[test]
fn a_tampered_amount_is_caught_as_a_field_mismatch() {
    let (_dir, path, mut ledger) = temp_ledger();
    let (request_id, txid, vout, amount, recipient) = finalized_glc_to_sol_request(&mut ledger);
    let message = release_claim_message(
        1,
        &[0x11; 32],
        5,
        &txid,
        vout,
        amount,
        &recipient,
        &[0x22; 32],
    );
    ledger
        .record_attestation(request_id, "release", &message, 0)
        .unwrap();
    drop(ledger);

    // Tamper the amount field in place and recompute a hash that matches
    // the tampered bytes, so self-consistency alone would miss it — this
    // is exactly why recompute-from-scalar-fields exists as a second,
    // independent check.
    let raw = rusqlite::Connection::open(&path).unwrap();
    let mut tampered = message.to_vec();
    tampered[94..102].copy_from_slice(&999_999u64.to_le_bytes());
    let new_hash = Sha256::digest(&tampered);
    raw.execute(
        "UPDATE attestation_records SET canonical_message = ?1, message_hash = ?2 WHERE request_id = ?3",
        rusqlite::params![tampered, new_hash.as_slice(), request_id],
    )
    .unwrap();
    drop(raw);

    let ledger = Ledger::open(&path).unwrap();
    let report = audit(&ledger).unwrap();
    assert!(!report.is_clean());
    assert!(matches!(
        &report.findings[0],
        Finding::FieldMismatch { field, request_id: r, .. } if *field == "amount" && *r == request_id
    ));
}

#[test]
fn an_empty_ledger_audits_clean_with_zero_records_checked() {
    let (_dir, _path, ledger) = temp_ledger();
    let report = audit(&ledger).unwrap();
    assert!(report.is_clean());
    assert_eq!(report.records_checked, 0);
}
