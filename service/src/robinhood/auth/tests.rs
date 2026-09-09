//! Authorization-payload tests.
//!
//! The `golden` module is the mandatory cross-language check: it parses
//! `contracts/test/fixtures/eip712-golden.json` — the SAME file
//! `contracts/test/GoldenDigests.t.sol` asserts the deployed contract
//! produces — and requires this crate's independent implementation to
//! produce every value in it. Neither side writes the file.
//!
//! The `tamper` module then proves the digests are actually load-bearing:
//! changing any single field of any payload must change the digest, so a
//! signature is a statement about exactly one operation.

use super::*;
use crate::evm::EvmSignature;

fn addr(hex: &str) -> EvmAddress {
    hex.parse().unwrap()
}

fn chain() -> EvmChainId {
    EvmChainId::new(4663).unwrap()
}

fn domain() -> BridgeDomain {
    BridgeDomain::new(chain(), addr(golden::VERIFYING_CONTRACT))
}

fn chains_glc_to_rhn() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 1001,
        dest: 2001,
    }
}

fn chains_rhn_to_glc() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 2001,
        dest: 1001,
    }
}

fn request_id() -> [u8; 32] {
    [0x11u8; 32]
}

fn payout() -> PayoutAuth {
    PayoutAuth {
        route: Route::GlcToRhn,
        chains: chains_glc_to_rhn(),
        token: addr(golden::TOKEN),
        request_id: request_id(),
        recipient: addr(golden::RECIPIENT),
        amount: RobinhoodAtomic::new(1_234_000_000_000_000_000_000),
        signer_epoch: 7,
        expiry: 1_800_000_000,
    }
}

fn refund() -> RefundAuth {
    RefundAuth {
        route: Route::RhnToGlc,
        chains: chains_rhn_to_glc(),
        token: addr(golden::TOKEN),
        request_id: request_id(),
        obligation_index: 42,
        recipient: addr(golden::RECIPIENT),
        amount: RobinhoodAtomic::new(500_000_000_000_000_000_000),
        signer_epoch: 7,
        expiry: 1_800_000_000,
    }
}

fn settlement() -> SettlementAuth {
    SettlementAuth {
        route: Route::RhnToGlc,
        chains: chains_rhn_to_glc(),
        request_id: request_id(),
        obligation_index: 42,
        signer_epoch: 7,
        expiry: 1_800_000_000,
    }
}

use crate::robinhood::golden::hex32;

// ------------------------------------------------------------- golden --

use crate::robinhood::golden;

#[test]
fn golden_domain_separator() {
    assert_eq!(
        hex32(&domain().separator()),
        golden::get("domainSeparator"),
        "the Rust EIP-712 domain drifted from the deployed contract's"
    );
}

#[test]
fn golden_typehashes() {
    assert_eq!(hex32(&payout_typehash()), golden::get("payoutTypehash"));
    assert_eq!(hex32(&refund_typehash()), golden::get("refundTypehash"));
    assert_eq!(
        hex32(&settlement_typehash()),
        golden::get("settlementTypehash")
    );
}

#[test]
fn golden_payout_struct_hash_and_digest() {
    let auth = payout();
    assert_eq!(
        hex32(&auth.struct_hash().unwrap()),
        golden::get("payout.structHash")
    );
    assert_eq!(
        hex32(&auth.digest(domain()).unwrap()),
        golden::get("payout.digest")
    );
}

#[test]
fn golden_refund_struct_hash_and_digest() {
    let auth = refund();
    assert_eq!(
        hex32(&auth.struct_hash().unwrap()),
        golden::get("refund.structHash")
    );
    assert_eq!(
        hex32(&auth.digest(domain()).unwrap()),
        golden::get("refund.digest")
    );
}

#[test]
fn golden_settlement_struct_hash_and_digest() {
    let auth = settlement();
    assert_eq!(
        hex32(&auth.struct_hash().unwrap()),
        golden::get("settlement.structHash")
    );
    assert_eq!(
        hex32(&auth.digest(domain()).unwrap()),
        golden::get("settlement.digest")
    );
}

#[test]
fn golden_fixture_names_the_inputs_this_test_uses() {
    // The fixture's `inputs` block is documentation, and documentation
    // that can drift from the code it documents is worse than none. Every
    // input this test actually feeds the encoders is asserted against it.
    assert_eq!(golden::get("eip712Name"), EIP712_NAME);
    assert_eq!(golden::get("eip712Version"), EIP712_VERSION);
    assert_eq!(golden::get("evmChainId"), chain().get().to_string());
    assert_eq!(
        golden::get("verifyingContract"),
        domain().verifying_contract.to_checksum_string()
    );
    assert_eq!(
        golden::get("signerEpoch"),
        payout().signer_epoch.to_string()
    );
    assert_eq!(golden::get("expiry"), payout().expiry.to_string());
    assert_eq!(
        golden::get("obligationIndex"),
        settlement().obligation_index.to_string()
    );
    assert_eq!(
        golden::get("payoutAmountRobinhoodAtomic"),
        payout().amount.get().to_string()
    );
    assert_eq!(
        golden::get("refundAmountRobinhoodAtomic"),
        refund().amount.get().to_string()
    );
    assert_eq!(
        golden::get("routeGlcToRhn"),
        Route::GlcToRhn.contract_route_id().unwrap().to_string()
    );
    assert_eq!(
        golden::get("routeRhnToGlc"),
        Route::RhnToGlc.contract_route_id().unwrap().to_string()
    );
    assert_eq!(
        golden::get("protocolChainGoldcoin"),
        chains_glc_to_rhn().source.to_string()
    );
    assert_eq!(
        golden::get("protocolChainRobinhood"),
        chains_glc_to_rhn().dest.to_string()
    );
}

// ------------------------------------------------------------- tamper --

/// Every field of every payload must be bound into its digest. A field
/// that is NOT bound is a field an attacker can change after the
/// signatures are collected.
mod tamper {
    use super::*;

    fn payout_digest(auth: &PayoutAuth) -> [u8; 32] {
        auth.digest(domain()).unwrap()
    }

    #[test]
    fn payout_route_is_bound() {
        // The only other outbound route. Same amount, same recipient,
        // same request id, different source network — and therefore a
        // different signed statement.
        let base = payout();
        let mut other = base.clone();
        other.route = Route::SolToRhn;
        other.chains = ProtocolChainPair {
            source: 3001,
            dest: 2001,
        };
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_chain_pair_is_bound_independently_of_the_route_byte() {
        // The pair is redundant given the byte, deliberately: a signer
        // reviewing the payload recognises the ids, not the byte. Prove
        // the redundancy is real by changing only the pair.
        let base = payout();
        let mut other = base.clone();
        other.chains = ProtocolChainPair {
            source: 1002,
            dest: 2001,
        };
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_amount_is_bound() {
        let base = payout();
        let mut other = base.clone();
        other.amount = RobinhoodAtomic::new(base.amount.get() + 1);
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_recipient_is_bound() {
        let base = payout();
        let mut other = base.clone();
        other.recipient = EvmAddress::from_bytes([0x99; 20]);
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_request_id_is_bound() {
        let base = payout();
        let mut other = base.clone();
        other.request_id[31] ^= 1;
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_token_is_bound() {
        let base = payout();
        let mut other = base.clone();
        other.token = EvmAddress::from_bytes([0x77; 20]);
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_signer_epoch_is_bound() {
        let base = payout();
        let mut other = base.clone();
        other.signer_epoch += 1;
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_expiry_is_bound() {
        let base = payout();
        let mut other = base.clone();
        other.expiry += 1;
        assert_ne!(payout_digest(&base), payout_digest(&other));
    }

    #[test]
    fn payout_chain_id_is_bound_through_the_domain() {
        let base = payout();
        let testnet =
            BridgeDomain::new(EvmChainId::new(46630).unwrap(), domain().verifying_contract);
        assert_ne!(payout_digest(&base), base.digest(testnet).unwrap());
    }

    #[test]
    fn payout_verifying_contract_is_bound_through_the_domain() {
        let base = payout();
        let successor = BridgeDomain::new(chain(), EvmAddress::from_bytes([0x5a; 20]));
        assert_ne!(payout_digest(&base), base.digest(successor).unwrap());
    }

    #[test]
    fn refund_obligation_index_and_amount_and_recipient_are_bound() {
        let base = refund();
        let d = |a: &RefundAuth| a.digest(domain()).unwrap();

        let mut other = base.clone();
        other.obligation_index += 1;
        assert_ne!(d(&base), d(&other));

        let mut other = base.clone();
        other.amount = RobinhoodAtomic::new(base.amount.get() - 1);
        assert_ne!(d(&base), d(&other));

        let mut other = base.clone();
        other.recipient = EvmAddress::from_bytes([0x01; 20]);
        assert_ne!(d(&base), d(&other));
    }

    #[test]
    fn settlement_obligation_index_and_epoch_and_expiry_are_bound() {
        let base = settlement();
        let d = |a: &SettlementAuth| a.digest(domain()).unwrap();

        let mut other = base.clone();
        other.obligation_index += 1;
        assert_ne!(d(&base), d(&other));

        let mut other = base.clone();
        other.signer_epoch += 1;
        assert_ne!(d(&base), d(&other));

        let mut other = base.clone();
        other.expiry += 1;
        assert_ne!(d(&base), d(&other));
    }

    #[test]
    fn the_three_actions_never_share_a_digest_even_with_identical_fields() {
        // A refund and a settlement over the same obligation, the same
        // request id, the same epoch and the same expiry. The action byte
        // AND the type name both differ, so the digests must too — and
        // the contract's own replay guard is keyed on the action, so a
        // collision here would be a genuine authorization confusion.
        let settle = settlement();
        let refund = RefundAuth {
            route: settle.route,
            chains: settle.chains,
            token: addr(golden::TOKEN),
            request_id: settle.request_id,
            obligation_index: settle.obligation_index,
            recipient: addr(golden::RECIPIENT),
            amount: RobinhoodAtomic::new(1),
            signer_epoch: settle.signer_epoch,
            expiry: settle.expiry,
        };
        assert_ne!(
            settle.digest(domain()).unwrap(),
            refund.digest(domain()).unwrap()
        );
    }
}

// ------------------------------------------------------- route safety --

#[test]
fn a_payout_on_a_deposit_route_is_refused_before_it_can_be_signed() {
    let mut auth = payout();
    auth.route = Route::RhnToGlc;
    assert_eq!(
        auth.struct_hash(),
        Err(AuthError::NotAPayoutRoute { route: "RhnToGlc" })
    );
}

#[test]
fn a_settlement_or_refund_on_a_payout_route_is_refused() {
    let mut auth = settlement();
    auth.route = Route::GlcToRhn;
    assert_eq!(
        auth.struct_hash(),
        Err(AuthError::NotADepositRoute { route: "GlcToRhn" })
    );

    let mut auth = refund();
    auth.route = Route::SolToRhn;
    assert_eq!(
        auth.struct_hash(),
        Err(AuthError::NotADepositRoute { route: "SolToRhn" })
    );
}

#[test]
fn the_solana_only_routes_have_no_contract_authorization_at_all() {
    for route in [Route::GlcToSol, Route::SolToGlc] {
        let mut auth = payout();
        auth.route = route;
        assert_eq!(
            auth.struct_hash(),
            Err(AuthError::NotAContractRoute {
                route: route.as_str()
            }),
            "{route:?} must not be expressible as a custody-contract authorization"
        );
        assert!(is_deposit_route(route).is_err());
    }
}

#[test]
fn deposit_and_payout_routes_are_classified_exactly_as_the_contract_classifies_them() {
    assert!(is_deposit_route(Route::RhnToGlc).unwrap());
    assert!(is_deposit_route(Route::RhnToSol).unwrap());
    assert!(!is_deposit_route(Route::GlcToRhn).unwrap());
    assert!(!is_deposit_route(Route::SolToRhn).unwrap());
}

// --------------------------------------------------------- request id --

#[test]
fn a_request_id_is_stable_across_re_derivation() {
    // The property the whole restart story rests on: the same operation,
    // derived again after a crash, is the SAME on-chain request.
    let identity = obligation_identity(42);
    let a = derive_request_id(ACTION_SETTLE, Route::RhnToGlc, domain(), &identity).unwrap();
    let b = derive_request_id(ACTION_SETTLE, Route::RhnToGlc, domain(), &identity).unwrap();
    assert_eq!(a, b);
}

#[test]
fn request_ids_differ_across_action_route_contract_chain_and_identity() {
    let base = derive_request_id(
        ACTION_SETTLE,
        Route::RhnToGlc,
        domain(),
        &obligation_identity(42),
    )
    .unwrap();

    let by_action = derive_request_id(
        ACTION_REFUND,
        Route::RhnToGlc,
        domain(),
        &obligation_identity(42),
    )
    .unwrap();
    assert_ne!(base, by_action);

    let by_route = derive_request_id(
        ACTION_SETTLE,
        Route::RhnToSol,
        domain(),
        &obligation_identity(42),
    )
    .unwrap();
    assert_ne!(base, by_route);

    let by_contract = derive_request_id(
        ACTION_SETTLE,
        Route::RhnToGlc,
        BridgeDomain::new(chain(), EvmAddress::from_bytes([0x5a; 20])),
        &obligation_identity(42),
    )
    .unwrap();
    assert_ne!(base, by_contract);

    let by_chain = derive_request_id(
        ACTION_SETTLE,
        Route::RhnToGlc,
        BridgeDomain::new(EvmChainId::new(46630).unwrap(), domain().verifying_contract),
        &obligation_identity(42),
    )
    .unwrap();
    assert_ne!(base, by_chain);

    let by_identity = derive_request_id(
        ACTION_SETTLE,
        Route::RhnToGlc,
        domain(),
        &obligation_identity(43),
    )
    .unwrap();
    assert_ne!(base, by_identity);
}

#[test]
fn goldcoin_source_identity_distinguishes_outpoint_and_row() {
    let a = goldcoin_source_identity([1u8; 32], 0, 5);
    assert_ne!(a, goldcoin_source_identity([1u8; 32], 1, 5));
    assert_ne!(a, goldcoin_source_identity([2u8; 32], 0, 5));
    assert_ne!(a, goldcoin_source_identity([1u8; 32], 0, 6));
    assert_eq!(a.len(), 44, "32-byte txid + 4-byte vout + 8-byte row id");
}

#[test]
fn a_request_id_for_a_non_contract_route_cannot_be_derived() {
    assert_eq!(
        derive_request_id(ACTION_PAYOUT, Route::GlcToSol, domain(), b"x"),
        Err(AuthError::NotAContractRoute { route: "GlcToSol" })
    );
}

// -------------------------------------------------- signing round trip --

#[test]
fn a_quorum_signature_over_a_payout_digest_recovers_to_the_signing_addresses() {
    // Proves the whole chain this module sits in: build the exact digest
    // the contract will build, sign it, and recover the addresses a
    // Solidity `ECDSA.recover` would recover.
    let digest = payout().digest(domain()).unwrap();
    let mut seen = Vec::new();
    for k in [3u8, 5] {
        let mut bytes = [0u8; 32];
        bytes[31] = k;
        let key = crate::evm::EvmSecretKey::from_bytes(&bytes).unwrap();
        let signature: EvmSignature = crate::evm::secp::sign_digest(&key, &digest);
        assert_eq!(
            crate::evm::secp::recover_address(&digest, &signature).unwrap(),
            key.address()
        );
        seen.push(key.address());
    }
    assert_ne!(seen[0], seen[1], "two distinct signers");
}
