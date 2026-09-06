//! Preflight tests.
//!
//! Every check is a REFUSAL, not a warning, and each of these sets
//! exactly one thing wrong and changes nothing else — so a passing test
//! proves that check is load-bearing rather than that the whole fixture
//! happens to be consistent.

use super::*;
use crate::robinhood::testkit::{signer_addresses, MockNode, BRIDGE, TOKEN};

fn node() -> MockNode {
    MockNode::new(BRIDGE)
}

async fn run(node: &MockNode) -> Result<VerifiedDeployment, PreflightError> {
    verify(node, &node.indexer_config(), &node.settlement_config()).await
}

#[tokio::test]
async fn a_healthy_deployment_verifies_and_reports_what_it_established() {
    let node = node();
    let verified = run(&node).await.expect("a healthy deployment verifies");

    assert_eq!(verified.chain_id.get(), 4663);
    assert_eq!(verified.bridge_contract, BRIDGE);
    assert_eq!(verified.token, TOKEN);
    assert_eq!(verified.token_decimals, 18);
    assert_eq!(verified.signers, signer_addresses());
    assert_eq!(verified.tx_envelope, TxEnvelope::Eip1559);
    assert!(verified.chain_has_base_fee);

    // The chain pairs are read from the CONTRACT, not configured.
    assert_eq!(
        verified.chains_for(Route::GlcToRhn).unwrap().source,
        crate::robinhood::testkit::PROTOCOL_GOLDCOIN
    );
    assert_eq!(
        verified.chains_for(Route::RhnToGlc).unwrap().source,
        crate::robinhood::testkit::PROTOCOL_ROBINHOOD
    );
    // And no pair is offered for a route that is not executable.
    assert!(verified.chains_for(Route::SolToRhn).is_none());
    assert!(verified.chains_for(Route::GlcToSol).is_none());
}

#[tokio::test]
async fn a_wrong_chain_is_refused() {
    let node = node();
    let mut cfg = node.settlement_config();
    // A settlement config for the testnet, pointed at a mainnet endpoint.
    // Built directly rather than through `new`, which would refuse the
    // mismatch against the indexer first — this isolates the CHAIN check.
    cfg.chain_id = crate::evm::EvmChainId::new(46630).unwrap();
    let err = verify(&node, &node.indexer_config(), &cfg)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PreflightError::WrongChain {
                expected: 46630,
                actual: 4663
            }
        ),
        "{err}"
    );
}

#[tokio::test]
async fn an_address_with_no_contract_code_is_refused_with_its_own_message() {
    // An `eth_call` to a codeless address returns EMPTY DATA rather than
    // failing, so without this check the operator would see a confusing
    // ABI decode error instead of "there is nothing deployed there".
    let node = node();
    node.with(|s| s.contract.bridge_code = Vec::new());
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(err, PreflightError::NoContractCode { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_token_address_with_no_code_is_refused_separately() {
    let node = node();
    node.with(|s| s.contract.token_code = Vec::new());
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::NoTokenCode { .. }), "{err}");
}

#[tokio::test]
async fn a_contract_of_a_different_protocol_family_is_refused() {
    let node = node();
    node.with(|s| s.contract.protocol_id = [0x99; 32]);
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::WrongProtocol { .. }), "{err}");
}

#[tokio::test]
async fn the_wrong_token_is_refused() {
    // Paying out against this contract would move a DIFFERENT asset.
    let node = node();
    node.with(|s| s.contract.token = crate::evm::EvmAddress::from_bytes([0x99; 20]));
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::WrongToken { .. }), "{err}");
}

#[tokio::test]
async fn a_token_with_the_wrong_decimals_is_refused() {
    // Not "scale differently" — this is not the asset the amount model is
    // built for, and every amount would be off by orders of magnitude.
    for decimals in [6u8, 8, 17, 19] {
        let node = node();
        node.with(|s| s.contract.token_decimals = decimals);
        let err = run(&node).await.unwrap_err();
        assert!(
            matches!(err, PreflightError::WrongDecimals { actual, .. } if actual == decimals),
            "{decimals} decimals must be refused, got {err}"
        );
    }
}

#[tokio::test]
async fn a_signer_set_that_disagrees_with_the_contract_is_refused() {
    // Every quorum this service assembled would be rejected on-chain
    // after gas was spent.
    let node = node();
    node.with(|s| s.contract.signers[1] = crate::evm::EvmAddress::from_bytes([0x99; 20]));
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(err, PreflightError::WrongSignerSet { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn the_signer_set_is_compared_as_a_set_not_as_an_ordered_list() {
    // The contract stores an array but membership is a mapping, and a
    // rotation may legitimately reorder. Requiring an order would make a
    // correct configuration fail.
    let node = node();
    node.with(|s| {
        let signers = s.contract.signers;
        s.contract.signers = [signers[2], signers[0], signers[1]];
    });
    run(&node)
        .await
        .expect("a reordered signer set is the same set");
}

#[tokio::test]
async fn a_domain_separator_that_disagrees_with_the_deployment_is_refused() {
    // The golden fixture proves the FORMULA matches the contract's. This
    // proves the formula, applied to THIS deployment, produces the
    // separator the deployed contract actually uses.
    let node = node();
    node.with(|s| s.contract.domain_separator = [0x5a; 32]);
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(err, PreflightError::DomainSeparatorMismatch { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn a_migrated_contract_is_refused_as_terminal() {
    let node = node();
    node.with(|s| s.contract.migrated = true);
    let err = run(&node).await.unwrap_err();
    assert!(matches!(err, PreflightError::AlreadyMigrated), "{err}");
}

#[tokio::test]
async fn the_envelope_is_verified_against_the_chains_own_fee_market_in_both_directions() {
    // THE check the whole configurable-envelope decision rests on. A
    // configured value nothing verifies is still a guess — just one an
    // operator made instead of one this code made.
    let node = node();

    // eip1559 configured, chain has NO base fee.
    node.with(|s| s.contract.base_fee = None);
    let err = run(&node).await.unwrap_err();
    assert!(
        matches!(
            err,
            PreflightError::EnvelopeMismatch {
                envelope: "eip1559",
                ..
            }
        ),
        "{err}"
    );

    // legacy configured, chain HAS a base fee.
    node.with(|s| s.contract.base_fee = Some(1_000_000_000));
    let mut cfg = node.settlement_config();
    cfg.tx_envelope = TxEnvelope::Legacy;
    let err = verify(&node, &node.indexer_config(), &cfg)
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            PreflightError::EnvelopeMismatch {
                envelope: "legacy",
                ..
            }
        ),
        "{err}"
    );

    // legacy configured, no base fee: agrees.
    node.with(|s| s.contract.base_fee = None);
    let verified = verify(&node, &node.indexer_config(), &cfg).await.unwrap();
    assert_eq!(verified.tx_envelope, TxEnvelope::Legacy);
    assert!(!verified.chain_has_base_fee);
}

#[tokio::test]
async fn a_disabled_route_does_not_fail_preflight() {
    // Route enablement is a GOVERNANCE state, checked live before every
    // broadcast — not a startup condition. A deployment must be able to
    // start against a contract whose routes are all still closed, which
    // is exactly the state a fresh deployment is in.
    let node = node();
    node.with(|s| {
        s.contract.route_enabled.insert(0x01, false);
        s.contract.route_enabled.insert(0x02, false);
    });
    run(&node)
        .await
        .expect("a closed route is a governance state, not a preflight failure");
}

#[tokio::test]
async fn a_paused_contract_does_not_fail_preflight() {
    // Same reasoning: a pause is what a guardian just did, and a service
    // that refused to START during an incident could not be brought up to
    // observe or refund.
    let node = node();
    node.with(|s| {
        s.contract.deposits_paused = true;
        s.contract.payouts_paused = true;
    });
    run(&node)
        .await
        .expect("a pause is not a preflight failure");
}

#[tokio::test]
async fn preflight_reads_the_contract_and_does_not_broadcast_anything() {
    let node = node();
    run(&node).await.unwrap();
    let calls = node.with(|s| s.calls.clone());
    assert!(calls.iter().any(|c| c == "eth_call"));
    assert!(calls.iter().any(|c| c == "eth_getCode"));
    assert!(
        !calls.iter().any(|c| c == "eth_sendRawTransaction"),
        "preflight must never broadcast: {calls:?}"
    );
}
