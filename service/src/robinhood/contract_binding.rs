//! The one check that keeps a contract-local obligation index from being
//! read off the wrong contract.
//!
//! # Why this exists
//!
//! `GlcRobinhoodBridge` numbers its obligations with a contract-local
//! counter. The ledger therefore records every Robinhood-sourced request
//! under the pair `(source_contract, source_obligation_index)` (schema
//! v21, `docs/06-schema.md`), and that pair is what makes the identity
//! durable across a contract replacement. But the SETTLEMENT side of this
//! service is bound to exactly one deployment — the one
//! `[robinhood.settlement]`/`[robinhood.indexer]` name, verified at
//! startup by [`super::preflight`] — and every value-moving path reads
//! `obligation(index)` off THAT deployment: the refund's recipient and
//! principal, the settlement's route, the replay guard.
//!
//! When the two disagree, `index` describes some other user's deposit.
//!
//! This is not hypothetical. On 2026-09-12 production was cut over from
//! the V1 custody contract (`0x1753dDA0…`) to V2 (`0xbaEdFFdA…`). Two
//! `RhnToSol` deposits (V1 obligations #29 and #30, requests 4037 and
//! 4038) landed on V1 after the cutover and were recovered into the
//! ledger by `robinhood-recover-deposit` with their TRUE contract
//! recorded. V2 has its own obligations #29 and #30 — different
//! depositors, different amounts, different route, both already
//! `Settled`. A refund of 4037 against the configured (V2) deployment
//! would have read V2's #29, found it not `Pending`, and refused — by
//! luck. Had V2's #29 still been `Pending`, the authorization would have
//! named V2-#29's depositor and principal, and request 4037 would have
//! been marked `Refunded` for a refund that went to someone else.
//!
//! # What it does
//!
//! [`require_same_contract`] compares the request's recorded
//! `source_contract` against the deployment's `bridge_contract` and
//! refuses on any difference — before any chain read, before any
//! authorization is minted, before any row is written. A request whose
//! contract is not the configured one can only be acted on by a process
//! configured against ITS contract (a config whose
//! `[robinhood.indexer]`/`[robinhood.settlement]` name it), which is
//! exactly how `robinhood-recover-deposit` recovered it in the first
//! place.
//!
//! It is applied at every point where a Robinhood-sourced request's
//! obligation index is about to be used against the deployment:
//! [`super::refund::begin_refund`], the `RhnToGlc`/`RhnToSol`
//! settlement authorization in [`super::settlement`], and the
//! `RhnToSol` Solana release in the orchestrator (a release whose
//! close-out could never land on the configured contract would leave the
//! obligation `Pending` — and refundable — on its real one).

use crate::evm::EvmAddress;
use crate::ledger::BridgeRequest;

use super::preflight::VerifiedDeployment;

/// A request whose recorded custody contract is not the one this process
/// is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignContract {
    pub request_id: i64,
    pub obligation_index: Option<u64>,
    pub request_contract: String,
    pub deployment_contract: String,
}

impl std::fmt::Display for ForeignContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let index = match self.obligation_index {
            Some(i) => format!("obligation {i}"),
            None => "its obligation".to_string(),
        };
        write!(
            f,
            "request {} was deposited on custody contract {}, but this process is bound to {}: \
             {} read off the wrong contract is someone else's deposit. Act on it only from a \
             config whose [robinhood.indexer] and [robinhood.settlement] name {}.",
            self.request_id,
            self.request_contract,
            self.deployment_contract,
            index,
            self.request_contract
        )
    }
}

impl std::error::Error for ForeignContract {}

/// Refuses unless `request.source_contract` is exactly the deployment's
/// `bridge_contract`.
///
/// A request with NO recorded contract is refused too: every
/// Robinhood-sourced row written since schema v21 carries one, and the
/// check exists to make the binding explicit, not to guess.
pub fn require_same_contract(
    request: &BridgeRequest,
    deployment: &VerifiedDeployment,
) -> Result<(), ForeignContract> {
    require_contract(request, deployment.bridge_contract)
}

/// [`require_same_contract`] against a bare address — for callers that
/// hold the configured contract without a full [`VerifiedDeployment`]
/// (the orchestrator's `RhnToSol` release gate).
pub fn require_contract(
    request: &BridgeRequest,
    bound_to: EvmAddress,
) -> Result<(), ForeignContract> {
    let recorded = request.source_contract.as_deref().unwrap_or(&[]);
    if recorded == bound_to.to_bytes() {
        return Ok(());
    }
    Err(ForeignContract {
        request_id: request.id,
        obligation_index: request.source_obligation_index,
        request_contract: match EvmAddress::try_from_slice(recorded) {
            Ok(a) => a.to_checksum_string(),
            Err(_) if recorded.is_empty() => "(none recorded)".to_string(),
            Err(_) => format!("0x{}", crate::evm::hex::encode_lower(recorded)),
        },
        deployment_contract: bound_to.to_checksum_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::Ledger;
    use crate::robinhood::testkit::{MockNode, BRIDGE};

    fn request_on(contract: Option<&[u8]>) -> BridgeRequest {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .conn_for_tests()
            .execute(
                "INSERT INTO bridge_requests
                    (direction, state, gross_amount_atomic, fee_bps, fee_amount_atomic,
                     net_amount_atomic, net_destination_atomic, recipient, created_at,
                     source_chain, source_contract, source_obligation_index,
                     source_confirmations, source_finalized_at, manual_review_note)
                 VALUES ('RhnToGlc', 'ManualReview', 100, 300, 3, 97, 97, X'6162', 100,
                         'robinhood', ?1, 29, 1, 100, 'parked')",
                rusqlite::params![contract],
            )
            .unwrap();
        let id = ledger.conn_for_tests().last_insert_rowid();
        ledger.get_request(id).unwrap().unwrap()
    }

    #[test]
    fn the_configured_contract_passes() {
        let node = MockNode::new(BRIDGE);
        let request = request_on(Some(&BRIDGE.to_bytes()));
        assert!(require_same_contract(&request, &node.verified_deployment()).is_ok());
    }

    #[test]
    fn a_predecessor_contract_is_refused_before_any_read() {
        let node = MockNode::new(BRIDGE);
        let v1 = EvmAddress::try_from_slice(&[0x17; 20]).unwrap();
        let request = request_on(Some(&v1.to_bytes()));
        let err = require_same_contract(&request, &node.verified_deployment()).unwrap_err();
        assert_eq!(err.request_contract, v1.to_checksum_string());
        assert_eq!(err.deployment_contract, BRIDGE.to_checksum_string());
        assert_eq!(err.obligation_index, Some(29));
        assert!(err.to_string().contains("someone else's deposit"), "{err}");
    }

    #[test]
    fn a_missing_contract_is_refused_not_guessed() {
        // The schema forbids a Robinhood row without a contract, so this
        // can only come from a hand-edited or legacy row — refused, never
        // assumed to be the configured one.
        let node = MockNode::new(BRIDGE);
        let mut request = request_on(Some(&BRIDGE.to_bytes()));
        request.source_contract = None;
        let err = require_same_contract(&request, &node.verified_deployment()).unwrap_err();
        assert_eq!(err.request_contract, "(none recorded)");
    }
}
