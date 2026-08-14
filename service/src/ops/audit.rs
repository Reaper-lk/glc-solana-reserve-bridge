//! The offline integrity auditor (docs/07-implementation-plan.md Phase 5).
//! Adapted from the old bridge's `ops/audit.rs` shape
//! (docs/01-reuse-inventory.md: pattern reusable unchanged, the recompute
//! functions rewritten against this bridge's two attestation-message
//! families instead of its mint-claim format).
//!
//! Re-verifies every frozen [`crate::ledger::AttestationRecord`] using the
//! same recompute-and-compare discipline the signing path implements, plus
//! SQLite's own `PRAGMA integrity_check`, and reports every finding
//! without stopping at the first one — the operator wants to know how
//! much is affected, not merely that something is.
//!
//! # What "recompute" checks here, precisely
//!
//! Two independent things, and only two:
//!
//! 1. **Self-consistency**: does `sha256(canonical_message)` still equal
//!    the stored `message_hash`? If not, the pair was altered
//!    independently and there is no meaningful "expected" value to
//!    compare against, so this is checked first and short-circuits the
//!    rest.
//! 2. **Field-level agreement with the ledger's own current record**: the
//!    bytes of `canonical_message` that encode this request's txid/vout/
//!    amount/recipient (release) or obligation index/payout txid/height/
//!    amount (completion) are extracted at their known offsets
//!    ([`shared::claim`]'s documented layout) and compared against
//!    `bridge_requests`/`goldcoin_payouts`'s current values for the same
//!    request.
//!
//! What this deliberately does **not** re-verify: the attestation epoch
//! and reserve-mint bytes embedded in a release message, or the
//! destination-commitment hash in a completion message. Both were fetched
//! live from Solana at attestation time and can legitimately differ from
//! "current" state later (an attestation-key rotation bumps the epoch on
//! purpose) — re-deriving them from *current* chain state and comparing
//! would produce false positives on every legitimate rotation, which is
//! worse than not checking them at all. An offline audit with no live
//! chain access has no honest way to re-verify those fields; this is a
//! real, documented scope limit, not a silent gap.

use sha2::{Digest, Sha256};

use glc_reserve_bridge_shared::claim::{COMPLETION_MESSAGE_LEN, RELEASE_CLAIM_MESSAGE_LEN};

use crate::ledger::{AttestationRecord, Ledger, LedgerError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Finding {
    #[error("SQLite integrity_check failed: {0}")]
    DatabaseCorrupt(String),
    #[error("attestation record {record_id} (request {request_id}): the frozen canonical message does not hash to its stored hash — the two were altered independently")]
    SelfInconsistent { record_id: i64, request_id: i64 },
    #[error("attestation record {record_id} (request {request_id}, action {action_type}): message is {actual} bytes, expected {expected}")]
    UnexpectedLength {
        record_id: i64,
        request_id: i64,
        action_type: String,
        expected: usize,
        actual: usize,
    },
    #[error("attestation record {record_id} (request {request_id}): frozen message's {field} does not match this request's current recorded value")]
    FieldMismatch {
        record_id: i64,
        request_id: i64,
        field: &'static str,
    },
    #[error("attestation record {record_id} references bridge request {request_id}, which no longer exists")]
    RequestMissing { record_id: i64, request_id: i64 },
    #[error("attestation record {record_id} (request {request_id}): completion message references a Goldcoin payout that does not exist")]
    PayoutMissing { record_id: i64, request_id: i64 },
}

#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    pub integrity_check: String,
    pub records_checked: usize,
    pub findings: Vec<Finding>,
}

impl AuditReport {
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "integrity_check: {}\nattestation records checked: {}\nfindings: {}",
            self.integrity_check,
            self.records_checked,
            self.findings.len()
        )
    }
}

pub fn audit(ledger: &Ledger) -> Result<AuditReport, LedgerError> {
    let integrity_check = ledger.integrity_check()?;
    let mut report = AuditReport {
        integrity_check: integrity_check.clone(),
        records_checked: 0,
        findings: Vec::new(),
    };
    if integrity_check != "ok" {
        // Reported, and the walk still runs: the operator wants to know how
        // much is affected, not merely that something is.
        report
            .findings
            .push(Finding::DatabaseCorrupt(integrity_check));
    }

    for record in ledger.all_attestation_records()? {
        report.records_checked += 1;
        if let Some(f) = check_record(ledger, &record) {
            report.findings.push(f);
        }
    }
    Ok(report)
}

fn check_record(ledger: &Ledger, record: &AttestationRecord) -> Option<Finding> {
    let computed_hash = Sha256::digest(&record.canonical_message);
    if computed_hash.as_slice() != record.message_hash.as_slice() {
        return Some(Finding::SelfInconsistent {
            record_id: record.id,
            request_id: record.request_id,
        });
    }

    let request = match ledger.get_request(record.request_id) {
        Ok(Some(r)) => r,
        _ => {
            return Some(Finding::RequestMissing {
                record_id: record.id,
                request_id: record.request_id,
            })
        }
    };

    match record.action_type.as_str() {
        "release" => check_release(record, &request),
        "completion" => check_completion(ledger, record, &request),
        _ => None, // unreachable given the schema's CHECK constraint
    }
}

fn check_release(
    record: &AttestationRecord,
    request: &crate::ledger::BridgeRequest,
) -> Option<Finding> {
    let m = &record.canonical_message;
    if m.len() != RELEASE_CLAIM_MESSAGE_LEN {
        return Some(Finding::UnexpectedLength {
            record_id: record.id,
            request_id: record.request_id,
            action_type: "release".to_string(),
            expected: RELEASE_CLAIM_MESSAGE_LEN,
            actual: m.len(),
        });
    }
    let mismatch = |field| {
        Some(Finding::FieldMismatch {
            record_id: record.id,
            request_id: record.request_id,
            field,
        })
    };
    let txid: [u8; 32] = m[58..90].try_into().unwrap();
    if Some(txid) != request.source_txid {
        return mismatch("txid");
    }
    let vout = u32::from_le_bytes(m[90..94].try_into().unwrap());
    if Some(vout) != request.source_vout {
        return mismatch("vout");
    }
    let amount = u64::from_le_bytes(m[94..102].try_into().unwrap());
    if amount != request.amount_atomic {
        return mismatch("amount");
    }
    if m[102..134] != request.recipient[..] {
        return mismatch("recipient");
    }
    None
}

fn check_completion(
    ledger: &Ledger,
    record: &AttestationRecord,
    request: &crate::ledger::BridgeRequest,
) -> Option<Finding> {
    let m = &record.canonical_message;
    if m.len() != COMPLETION_MESSAGE_LEN {
        return Some(Finding::UnexpectedLength {
            record_id: record.id,
            request_id: record.request_id,
            action_type: "completion".to_string(),
            expected: COMPLETION_MESSAGE_LEN,
            actual: m.len(),
        });
    }
    let mismatch = |field| {
        Some(Finding::FieldMismatch {
            record_id: record.id,
            request_id: record.request_id,
            field,
        })
    };
    let obligation_index = u64::from_le_bytes(m[58..66].try_into().unwrap());
    if Some(obligation_index) != request.source_obligation_index {
        return mismatch("obligation_index");
    }

    let payout = match ledger.get_goldcoin_payout(record.request_id) {
        Ok(Some(p)) => p,
        _ => {
            return Some(Finding::PayoutMissing {
                record_id: record.id,
                request_id: record.request_id,
            })
        }
    };
    let payout_txid: [u8; 32] = m[66..98].try_into().unwrap();
    if Some(payout_txid) != payout.txid {
        return mismatch("payout_txid");
    }
    let payout_height = u64::from_le_bytes(m[98..106].try_into().unwrap());
    if Some(payout_height as i64) != payout.mined_height {
        return mismatch("payout_height");
    }
    let amount = u64::from_le_bytes(m[106..114].try_into().unwrap());
    if amount != payout.payout_atomic {
        return mismatch("amount");
    }
    None
}

#[cfg(test)]
mod tests;
