//! The custody domain's half of the v2 protocol.
//!
//! The property every test here circles is the one the module exists for:
//! **the signer signs the digest it computed, never the one it was
//! sent.** So the tests do not check "does evaluate return ok" — they
//! check that the returned digest is derived from the fields, that a
//! request whose fields and digest disagree is refused, and that a
//! request whose fields are wrong is refused even when its digest is
//! perfectly consistent with them.

use super::*;
use crate::amount_conversion::robinhood::RobinhoodAtomic;
use crate::evm::{EvmAddress, EvmChainId};
use crate::robinhood::auth::{PayoutAuth, RefundAuth, SettlementAuth};

const CHAIN_ID: u64 = 4663;
const NOW: u64 = 1_800_000_000;
const TTL: u64 = 900;

fn addr(byte: u8) -> EvmAddress {
    EvmAddress::from_bytes([byte; 20])
}

fn bridge() -> EvmAddress {
    addr(0xb1)
}

fn token() -> EvmAddress {
    addr(0x70)
}

fn domain() -> BridgeDomain {
    BridgeDomain::new(EvmChainId::new(CHAIN_ID).unwrap(), bridge())
}

fn glc_to_rhn_chains() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 1001,
        dest: 2001,
    }
}

fn rhn_to_glc_chains() -> ProtocolChainPair {
    ProtocolChainPair {
        source: 2001,
        dest: 1001,
    }
}

/// A domain provisioned to serve this deployment fully.
fn policy() -> EvmSignerPolicy {
    EvmSignerPolicy {
        chain_id: EvmChainId::new(CHAIN_ID).unwrap(),
        verifying_contract: bridge(),
        token: token(),
        allowed_actions: vec![ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE],
        allowed_routes: vec![Route::GlcToRhn, Route::RhnToGlc],
        route_chains: vec![
            (Route::GlcToRhn, glc_to_rhn_chains()),
            (Route::RhnToGlc, rhn_to_glc_chains()),
        ],
        max_amount_robinhood_atomic: 10_000 * 1_000_000_000_000_000_000,
        max_authorization_ttl_secs: 3_600,
        expected_signer_epoch: None,
    }
}

fn payout_request() -> EvmAuthRequest {
    EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn_chains(),
            token: token(),
            request_id: [0x11; 32],
            recipient: addr(0xc0),
            amount: RobinhoodAtomic::new(5 * 1_000_000_000_000_000_000),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    )
}

fn refund_request() -> EvmAuthRequest {
    EvmAuthRequest::refund(
        domain(),
        RefundAuth {
            route: Route::RhnToGlc,
            chains: rhn_to_glc_chains(),
            token: token(),
            request_id: [0x22; 32],
            obligation_index: 42,
            recipient: addr(0xd0),
            amount: RobinhoodAtomic::new(3 * 1_000_000_000_000_000_000),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    )
}

fn settlement_request() -> EvmAuthRequest {
    EvmAuthRequest::settlement(
        domain(),
        SettlementAuth {
            route: Route::RhnToGlc,
            chains: rhn_to_glc_chains(),
            request_id: [0x33; 32],
            obligation_index: 42,
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    )
}

fn document(request: &EvmAuthRequest) -> EvmAuthSignRequest {
    EvmAuthSignRequest::from_request(request).expect("a well-formed authorization encodes")
}

// ------------------------------------------------------------ round trip --

/// The core contract: the digest the domain returns is the one it derived
/// from the fields, and it agrees with what the bridge derived from the
/// same authorization.
#[test]
fn each_payload_family_round_trips_and_the_digest_is_recomputed_not_copied() {
    for request in [payout_request(), refund_request(), settlement_request()] {
        let decision = policy()
            .evaluate(&document(&request), NOW)
            .unwrap_or_else(|e| panic!("{} must be signable: {e}", request.kind_str()));

        // Reconstructed identically — every field survived the wire.
        assert_eq!(decision.request, request, "{}", request.kind_str());
        // And the digest is the encoder's, computed on this side.
        assert_eq!(decision.digest, request.digest().unwrap());
        assert!(!decision.summary.is_empty());
    }
}

/// A settlement binds no token, no recipient and no amount, and a payout
/// binds no obligation. The document must reflect that rather than
/// carrying zeroed placeholders — a placeholder would be a field the
/// signer "checked" that the digest does not cover.
#[test]
fn the_document_omits_exactly_the_fields_its_payload_family_does_not_bind() {
    let settlement = document(&settlement_request());
    assert_eq!(settlement.token, None);
    assert_eq!(settlement.recipient, None);
    assert_eq!(settlement.amount_robinhood_atomic, None);
    assert_eq!(settlement.obligation_index, Some(42));

    let payout = document(&payout_request());
    assert_eq!(payout.obligation_index, None);
    assert!(payout.token.is_some());
    assert!(payout.recipient.is_some());
    assert!(payout.amount_robinhood_atomic.is_some());
}

/// Amounts cross the wire as decimal strings because 10^18 atomic units
/// do not survive an IEEE-754 double. A JSON parser that rounded one
/// would produce a digest for a different transfer than the operator
/// approved.
#[test]
fn amounts_cross_the_wire_as_decimal_strings_not_json_numbers() {
    let doc = document(&payout_request());
    let json = serde_json::to_string(&doc).expect("serialises");
    assert!(
        json.contains("\"amount_robinhood_atomic\":\"5000000000000000000\""),
        "{json}"
    );
    let back: EvmAuthSignRequest = serde_json::from_str(&json).expect("round trips");
    assert_eq!(back, doc);
}

// ------------------------------------------ the digest is never trusted --

/// The single most important test in this module. A caller that supplies
/// a digest of its choosing gets a refusal, not a signature over it.
#[test]
fn a_supplied_digest_that_disagrees_with_the_fields_is_refused() {
    let mut doc = document(&payout_request());
    doc.expected_digest = hex32(&[0xaa; 32]);
    let err = policy()
        .evaluate(&doc, NOW)
        .expect_err("a digest that does not follow from the fields must be refused");
    assert!(
        matches!(err, EvmPolicyError::DigestMismatch { .. }),
        "{err:?}"
    );
}

/// The other direction: fields tampered with, digest left alone. The
/// recomputation catches it, because the digest no longer follows from
/// what is being asked for.
#[test]
fn tampering_with_a_field_after_the_digest_was_computed_is_refused() {
    let honest = payout_request();
    for mutate in [
        (|d: &mut EvmAuthSignRequest| d.recipient = Some(addr(0xee).to_checksum_string()))
            as fn(&mut EvmAuthSignRequest),
        |d| d.amount_robinhood_atomic = Some("1".to_string()),
        |d| d.request_id = hex32(&[0x99; 32]),
        |d| d.signer_epoch = 8,
        |d| d.expiry = NOW + 800,
    ] {
        let mut doc = document(&honest);
        mutate(&mut doc);
        let err = policy()
            .evaluate(&doc, NOW)
            .expect_err("a tampered field must be refused");
        assert!(
            matches!(err, EvmPolicyError::DigestMismatch { .. }),
            "{err:?}"
        );
    }
}

/// And the case an attacker would actually attempt: change a field AND
/// recompute the digest so the two agree. Consistency is not authority —
/// the policy checks still stand.
#[test]
fn a_consistently_re_signed_request_is_still_judged_on_its_content() {
    // A payout to a different recipient, for a different amount, with a
    // digest that is perfectly consistent with those values.
    let tampered = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            chains: glc_to_rhn_chains(),
            token: addr(0xbe), // NOT the token this domain serves
            request_id: [0x11; 32],
            recipient: addr(0xee),
            amount: RobinhoodAtomic::new(1),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    let err = policy()
        .evaluate(&document(&tampered), NOW)
        .expect_err("a self-consistent request naming the wrong token must be refused");
    assert!(matches!(err, EvmPolicyError::WrongToken { .. }), "{err:?}");
}

// ------------------------------------------------------ shape and version --

#[test]
fn an_unknown_protocol_version_is_refused_rather_than_guessed() {
    for version in [0, 1, 3, 99] {
        let mut doc = document(&payout_request());
        doc.protocol_version = version;
        let err = policy().evaluate(&doc, NOW).expect_err("version {version}");
        assert!(
            matches!(err, EvmPolicyError::UnknownProtocolVersion { .. }),
            "{err:?}"
        );
    }
}

/// Abandonment closes an obligation while RETAINING the depositor's
/// principal. Nothing in this service can build one, and a custody domain
/// asked for one refuses it as an unknown action.
#[test]
fn an_unknown_action_including_abandonment_is_refused() {
    for action in [0x00, 0x04, 0x05, 0xff] {
        let mut doc = document(&payout_request());
        doc.action = action;
        let err = policy().evaluate(&doc, NOW).expect_err("action {action}");
        assert!(
            matches!(err, EvmPolicyError::UnknownAction { .. }),
            "action {action:#04x}: {err:?}"
        );
    }
}

#[test]
fn a_kind_that_contradicts_its_action_byte_is_refused() {
    let mut doc = document(&payout_request());
    doc.kind = "settlement".to_string();
    let err = policy().evaluate(&doc, NOW).expect_err("kind mismatch");
    assert!(
        matches!(err, EvmPolicyError::KindActionMismatch { .. }),
        "{err:?}"
    );
}

#[test]
fn an_unknown_route_is_refused() {
    let mut doc = document(&payout_request());
    doc.route = "GlcToMars".to_string();
    let err = policy().evaluate(&doc, NOW).expect_err("unknown route");
    assert!(
        matches!(err, EvmPolicyError::UnknownRoute { .. }),
        "{err:?}"
    );
}

// -------------------------------------------------- deployment identity --

#[test]
fn a_request_for_a_different_verifying_contract_is_refused() {
    let other = BridgeDomain::new(EvmChainId::new(CHAIN_ID).unwrap(), addr(0xbb));
    let request = EvmAuthRequest::payout(
        other,
        match payout_request().payload {
            EvmAuthPayload::Payout(p) => p,
            _ => unreachable!(),
        },
    );
    let err = policy()
        .evaluate(&document(&request), NOW)
        .expect_err("wrong verifying contract");
    assert!(
        matches!(err, EvmPolicyError::WrongVerifyingContract { .. }),
        "{err:?}"
    );
}

#[test]
fn a_request_for_a_different_evm_chain_is_refused() {
    let other = BridgeDomain::new(EvmChainId::new(1).unwrap(), bridge());
    let request = EvmAuthRequest::payout(
        other,
        match payout_request().payload {
            EvmAuthPayload::Payout(p) => p,
            _ => unreachable!(),
        },
    );
    let err = policy()
        .evaluate(&document(&request), NOW)
        .expect_err("wrong chain id");
    assert!(
        matches!(err, EvmPolicyError::WrongChainId { .. }),
        "{err:?}"
    );
}

/// The protocol chain pair is held by the domain, not taken from the
/// request. A pair that is valid for a DIFFERENT route is the dangerous
/// case, and it is refused.
#[test]
fn a_protocol_chain_pair_the_domain_does_not_hold_for_that_route_is_refused() {
    let request = EvmAuthRequest::payout(
        domain(),
        PayoutAuth {
            route: Route::GlcToRhn,
            // The INBOUND route's pair, on an outbound route.
            chains: rhn_to_glc_chains(),
            token: token(),
            request_id: [0x11; 32],
            recipient: addr(0xc0),
            amount: RobinhoodAtomic::new(1_000_000_000_000_000_000),
            signer_epoch: 7,
            expiry: NOW + TTL,
        },
    );
    let err = policy()
        .evaluate(&document(&request), NOW)
        .expect_err("swapped chain pair");
    assert!(
        matches!(err, EvmPolicyError::WrongProtocolChains { .. }),
        "{err:?}"
    );
}

// --------------------------------------------------- credential scoping --

#[test]
fn a_credential_scoped_to_settlement_cannot_authorize_a_payout() {
    let settle_only = EvmSignerPolicy {
        allowed_actions: vec![ACTION_SETTLE],
        ..policy()
    };
    let err = settle_only
        .evaluate(&document(&payout_request()), NOW)
        .expect_err("a settle-only credential must not authorize a payout");
    assert!(
        matches!(err, EvmPolicyError::ActionNotPermitted { .. }),
        "{err:?}"
    );
    // ...and still authorizes what it is scoped to.
    settle_only
        .evaluate(&document(&settlement_request()), NOW)
        .expect("a settle-only credential authorizes a settlement");
}

#[test]
fn a_domain_that_has_not_agreed_to_a_route_refuses_it() {
    let inbound_only = EvmSignerPolicy {
        allowed_routes: vec![Route::RhnToGlc],
        ..policy()
    };
    let err = inbound_only
        .evaluate(&document(&payout_request()), NOW)
        .expect_err("an unagreed route must be refused");
    assert!(
        matches!(err, EvmPolicyError::RouteNotPermitted { .. }),
        "{err:?}"
    );
}

// ------------------------------------------------------------- content --

#[test]
fn an_amount_above_this_domains_own_ceiling_is_refused() {
    let strict = EvmSignerPolicy {
        max_amount_robinhood_atomic: 1_000_000_000_000_000_000, // 1 GLC
        ..policy()
    };
    let err = strict
        .evaluate(&document(&payout_request()), NOW) // 5 GLC
        .expect_err("above the ceiling");
    assert!(
        matches!(err, EvmPolicyError::AmountAboveCeiling { .. }),
        "{err:?}"
    );
    // Exactly at the ceiling is allowed — the bound is inclusive.
    let at_ceiling = EvmSignerPolicy {
        max_amount_robinhood_atomic: 5 * 1_000_000_000_000_000_000,
        ..policy()
    };
    at_ceiling
        .evaluate(&document(&payout_request()), NOW)
        .expect("exactly at the ceiling is permitted");
}

/// A settlement moves nothing, so the amount ceiling does not apply to
/// it — a domain with a ceiling of zero still settles.
#[test]
fn the_amount_ceiling_does_not_bound_a_settlement() {
    let zero_ceiling = EvmSignerPolicy {
        max_amount_robinhood_atomic: 0,
        ..policy()
    };
    zero_ceiling
        .evaluate(&document(&settlement_request()), NOW)
        .expect("a settlement moves no tokens and is not bounded by the amount ceiling");
}

#[test]
fn an_already_expired_authorization_is_refused() {
    let doc = document(&payout_request()); // expiry = NOW + TTL
    let err = policy()
        .evaluate(&doc, NOW + TTL)
        .expect_err("expiry is exclusive: at the expiry second it is dead");
    assert!(
        matches!(err, EvmPolicyError::AlreadyExpired { .. }),
        "{err:?}"
    );
    let err = policy().evaluate(&doc, NOW + TTL + 1).expect_err("past");
    assert!(
        matches!(err, EvmPolicyError::AlreadyExpired { .. }),
        "{err:?}"
    );
    // One second before, it is still live.
    policy()
        .evaluate(&doc, NOW + TTL - 1)
        .expect("still within its life");
}

/// An authorization with a long life is a standing permission slip, and
/// the domain bounds it independently of what the bridge configured.
#[test]
fn a_ttl_above_this_domains_own_ceiling_is_refused() {
    let strict = EvmSignerPolicy {
        max_authorization_ttl_secs: 60,
        ..policy()
    };
    let err = strict
        .evaluate(&document(&payout_request()), NOW) // 900s of life
        .expect_err("above the TTL ceiling");
    assert!(
        matches!(err, EvmPolicyError::TtlAboveCeiling { .. }),
        "{err:?}"
    );
}

#[test]
fn a_domain_tracking_the_signer_epoch_refuses_a_stale_one() {
    let tracking = EvmSignerPolicy {
        expected_signer_epoch: Some(9),
        ..policy()
    };
    let err = tracking
        .evaluate(&document(&payout_request()), NOW) // epoch 7
        .expect_err("stale epoch");
    assert!(
        matches!(err, EvmPolicyError::WrongSignerEpoch { .. }),
        "{err:?}"
    );
    // A domain with no independent way to read the epoch does not check
    // it — checking it against a value taken from the request would be no
    // check at all.
    policy()
        .evaluate(&document(&payout_request()), NOW)
        .expect("an untracking domain does not check the epoch");
}

// ------------------------------------------------- absent/extra fields --

/// A signer must never DEFAULT a field the digest binds: a zeroed
/// recipient would be a field it "checked" that means something else.
#[test]
fn a_missing_required_field_is_refused_rather_than_defaulted() {
    for (name, mutate) in [
        (
            "recipient",
            (|d: &mut EvmAuthSignRequest| d.recipient = None) as fn(&mut EvmAuthSignRequest),
        ),
        ("token", |d| d.token = None),
        ("amount", |d| d.amount_robinhood_atomic = None),
    ] {
        let mut doc = document(&payout_request());
        mutate(&mut doc);
        let err = policy().evaluate(&doc, NOW).expect_err("{name} missing");
        assert!(
            matches!(err, EvmPolicyError::MissingField { .. }),
            "{name}: {err:?}"
        );
    }

    let mut doc = document(&refund_request());
    doc.obligation_index = None;
    let err = policy()
        .evaluate(&doc, NOW)
        .expect_err("obligation missing");
    assert!(
        matches!(err, EvmPolicyError::MissingField { .. }),
        "{err:?}"
    );
}

/// The mirror image: a field the payload family does not bind must not be
/// silently ignored, because a caller supplying one believes it bound
/// something.
#[test]
fn a_field_the_payload_family_does_not_bind_is_refused_not_ignored() {
    for mutate in [
        (|d: &mut EvmAuthSignRequest| d.token = Some(token().to_checksum_string()))
            as fn(&mut EvmAuthSignRequest),
        |d| d.recipient = Some(addr(0xc0).to_checksum_string()),
        |d| d.amount_robinhood_atomic = Some("1".to_string()),
    ] {
        let mut doc = document(&settlement_request());
        mutate(&mut doc);
        let err = policy().evaluate(&doc, NOW).expect_err("extra field");
        assert!(
            matches!(err, EvmPolicyError::UnexpectedField { .. }),
            "{err:?}"
        );
    }

    let mut doc = document(&payout_request());
    doc.obligation_index = Some(1);
    let err = policy().evaluate(&doc, NOW).expect_err("payout obligation");
    assert!(
        matches!(err, EvmPolicyError::UnexpectedField { .. }),
        "{err:?}"
    );
}

#[test]
fn a_malformed_field_is_refused_with_the_field_named() {
    let mut doc = document(&payout_request());
    doc.recipient = Some("not-an-address".to_string());
    let err = policy().evaluate(&doc, NOW).expect_err("bad address");
    assert!(
        matches!(
            err,
            EvmPolicyError::MalformedField {
                field: "recipient",
                ..
            }
        ),
        "{err:?}"
    );

    let mut doc = document(&payout_request());
    doc.amount_robinhood_atomic = Some("5.0".to_string());
    let err = policy()
        .evaluate(&doc, NOW)
        .expect_err("non-integer amount");
    assert!(
        matches!(
            err,
            EvmPolicyError::MalformedField {
                field: "amount_robinhood_atomic",
                ..
            }
        ),
        "{err:?}"
    );
}

/// A payout on a DEPOSIT route has no digest at all — the encoder refuses
/// it — so it can never reach a signature, whatever the policy says.
#[test]
fn a_payload_on_the_wrong_leg_has_no_digest_and_is_refused() {
    let mut doc = document(&payout_request());
    doc.route = Route::RhnToGlc.as_str().to_string();
    doc.protocol_source_chain_id = rhn_to_glc_chains().source;
    doc.protocol_dest_chain_id = rhn_to_glc_chains().dest;
    let err = policy()
        .evaluate(&doc, NOW)
        .expect_err("a payout on a deposit route is not encodable");
    assert!(matches!(err, EvmPolicyError::NotEncodable(_)), "{err:?}");
}

/// The two Solana<->Robinhood routes have a contract discriminator but no
/// settlement machinery. A domain must never sign for one.
#[test]
fn the_solana_robinhood_routes_cannot_be_authorized() {
    let permissive = EvmSignerPolicy {
        allowed_routes: vec![Route::SolToRhn, Route::RhnToSol],
        route_chains: vec![
            (Route::SolToRhn, glc_to_rhn_chains()),
            (Route::RhnToSol, rhn_to_glc_chains()),
        ],
        ..policy()
    };
    for route in [Route::SolToRhn, Route::RhnToSol] {
        let mut doc = document(&payout_request());
        doc.route = route.as_str().to_string();
        // Even a domain deliberately misconfigured to allow them cannot
        // produce a signature: the digest never agrees, because
        // `EvmAuthSignRequest::from_request` built `expected_digest` for
        // a different route and no consistent one can be constructed for
        // a route with no `Direction` behind it.
        assert!(
            permissive.evaluate(&doc, NOW).is_err(),
            "{} must never be authorizable",
            route.as_str()
        );
    }
}
