use super::*;

use crate::robinhood::governance::CANONICAL_SCALE;

const NOW: u64 = 1_700_000_000;
const EXPIRY: u64 = NOW + 600;
const CHAIN_ID: u64 = 4663;

fn contract() -> EvmAddress {
    EvmAddress::from_bytes([0x0b; 20])
}

fn domain() -> BridgeDomain {
    BridgeDomain::new(EvmChainId::new(CHAIN_ID).unwrap(), contract())
}

/// A domain that HAS opted in to all three actions.
fn policy() -> EvmGovernancePolicy {
    EvmGovernancePolicy {
        chain_id: EvmChainId::new(CHAIN_ID).unwrap(),
        verifying_contract: contract(),
        allowed_actions: vec![
            ACTION_SET_LIMITS,
            ACTION_SET_PAUSE,
            ACTION_SET_ROUTE_ENABLED,
        ],
        max_authorization_ttl_secs: 3600,
        expected_signer_epoch: Some(7),
    }
}

fn limits() -> BridgeLimits {
    BridgeLimits {
        inbound_min: EvmU256::from_u128(CANONICAL_SCALE),
        inbound_max: EvmU256::from_u128(20 * CANONICAL_SCALE),
        inbound_rolling_limit: EvmU256::from_u128(40 * CANONICAL_SCALE),
        outbound_min: EvmU256::from_u128(CANONICAL_SCALE),
        outbound_max: EvmU256::from_u128(20 * CANONICAL_SCALE),
        outbound_rolling_limit: EvmU256::from_u128(40 * CANONICAL_SCALE),
        protected_min_reserve: EvmU256::from_u128(5 * CANONICAL_SCALE),
    }
}

fn auth(payload: GovernancePayload) -> GovernanceAuth {
    GovernanceAuth {
        payload,
        signer_epoch: 7,
        nonce: EvmU256::from_u64(3),
        expiry: EXPIRY,
    }
}

fn document(payload: GovernancePayload) -> EvmGovernanceSignRequest {
    EvmGovernanceSignRequest::from_auth(&auth(payload), domain()).expect("a buildable document")
}

// =====================================================================
// The round trip, and the inversion that makes it safe
// =====================================================================

/// Every action a domain may allow round-trips: the proposer builds a
/// document, the signer rebuilds the authorization from its structured
/// fields alone, and derives the same digest.
#[test]
fn every_governance_action_round_trips_through_the_wire_document() {
    for payload in [
        GovernancePayload::SetLimits(limits()),
        GovernancePayload::SetPaused {
            deposits_paused: false,
            payouts_paused: true,
        },
        GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        },
    ] {
        let expected = auth(payload.clone());
        let doc = document(payload);
        let decision = policy()
            .evaluate(&doc, NOW)
            .unwrap_or_else(|e| panic!("{doc:?} must be accepted: {e}"));

        assert_eq!(
            decision.auth, expected,
            "the signer rebuilt a different auth"
        );
        assert_eq!(decision.digest, expected.digest(domain()).unwrap());
        assert!(
            decision.summary.starts_with("governance "),
            "{}",
            decision.summary
        );
    }
}

/// The digest is derived, never taken. A request whose `expected_digest`
/// disagrees with the fields is refused — neither value is signed.
#[test]
fn a_digest_that_disagrees_with_the_fields_is_refused() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.expected_digest = crate::evm::hex::encode_lower(&[0xAA; 32]);

    let err = policy().evaluate(&doc, NOW).expect_err("must refuse");
    assert!(
        matches!(err, EvmGovernanceError::DigestMismatch { .. }),
        "{err}"
    );
}

/// And the inversion proves itself: tampering with a bound field changes
/// the digest the signer derives, so the untouched `expected_digest` no
/// longer matches. A caller cannot smuggle different content past the
/// policy check.
#[test]
fn tampering_with_a_bound_field_is_caught_by_the_derivation() {
    let good = document(GovernancePayload::SetLimits(limits()));

    let mut tampered = good.clone();
    tampered.limits.as_mut().unwrap().outbound_max = (30 * CANONICAL_SCALE).to_string();
    let err = policy().evaluate(&tampered, NOW).expect_err("must refuse");
    assert!(
        matches!(err, EvmGovernanceError::DigestMismatch { .. }),
        "a changed limit must not verify: {err}"
    );

    let mut tampered = good.clone();
    tampered.nonce = "4".to_string();
    assert!(
        matches!(
            policy().evaluate(&tampered, NOW),
            Err(EvmGovernanceError::DigestMismatch { .. })
        ),
        "a changed nonce must not verify"
    );

    let mut tampered = good;
    tampered.signer_epoch = 8;
    // The epoch check fires first, which is the more specific refusal.
    assert!(
        matches!(
            policy().evaluate(&tampered, NOW),
            Err(EvmGovernanceError::WrongSignerEpoch { .. })
        ),
        "a changed epoch must not verify"
    );
}

// =====================================================================
// Governance is off until a domain turns it on
// =====================================================================

#[test]
fn a_domain_that_has_not_opted_in_refuses_every_governance_action() {
    let disabled = EvmGovernancePolicy {
        allowed_actions: Vec::new(),
        ..policy()
    };
    assert!(!disabled.is_enabled());

    for payload in [
        GovernancePayload::SetLimits(limits()),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        },
        GovernancePayload::SetRouteEnabled {
            route: Route::GlcToRhn,
            enabled: false,
        },
    ] {
        let err = disabled
            .evaluate(&document(payload), NOW)
            .expect_err("governance is off");
        assert!(
            matches!(err, EvmGovernanceError::GovernanceDisabled),
            "{err}"
        );
    }
}

/// A domain may allow one action without allowing the others.
#[test]
fn an_action_outside_the_credential_scope_is_refused() {
    let pause_only = EvmGovernancePolicy {
        allowed_actions: vec![ACTION_SET_PAUSE],
        ..policy()
    };
    assert!(pause_only
        .evaluate(
            &document(GovernancePayload::SetPaused {
                deposits_paused: true,
                payouts_paused: true
            }),
            NOW
        )
        .is_ok());

    let err = pause_only
        .evaluate(&document(GovernancePayload::SetLimits(limits())), NOW)
        .expect_err("setLimits is not permitted for this credential");
    assert!(
        matches!(
            err,
            EvmGovernanceError::ActionNotPermitted {
                requested: ACTION_SET_LIMITS,
                ..
            }
        ),
        "{err}"
    );
}

// =====================================================================
// Deployment identity is held independently
// =====================================================================

#[test]
fn a_request_for_another_chain_is_refused() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.chain_id = 1;
    let err = policy().evaluate(&doc, NOW).expect_err("wrong chain");
    assert!(
        matches!(
            err,
            EvmGovernanceError::WrongChainId {
                expected: CHAIN_ID,
                actual: 1
            }
        ),
        "{err}"
    );
}

#[test]
fn a_request_for_another_contract_is_refused() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.verifying_contract = EvmAddress::from_bytes([0x0c; 20]).to_checksum_string();
    let err = policy().evaluate(&doc, NOW).expect_err("wrong contract");
    assert!(
        matches!(err, EvmGovernanceError::WrongVerifyingContract { .. }),
        "{err}"
    );
}

#[test]
fn a_stale_signer_epoch_is_refused_when_the_domain_tracks_it() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.signer_epoch = 6;
    let err = policy().evaluate(&doc, NOW).expect_err("stale epoch");
    assert!(
        matches!(
            err,
            EvmGovernanceError::WrongSignerEpoch {
                expected: 7,
                actual: 6
            }
        ),
        "{err}"
    );

    // A domain with no independent epoch source does not pretend to
    // check — but the digest still binds it, so a stale epoch fails
    // on-chain rather than silently passing here.
    let untracking = EvmGovernancePolicy {
        expected_signer_epoch: None,
        ..policy()
    };
    let stale = EvmGovernanceSignRequest::from_auth(
        &GovernanceAuth {
            signer_epoch: 6,
            ..auth(GovernancePayload::SetPaused {
                deposits_paused: true,
                payouts_paused: true,
            })
        },
        domain(),
    )
    .unwrap();
    assert!(untracking.evaluate(&stale, NOW).is_ok());
}

// =====================================================================
// Lifetime
// =====================================================================

#[test]
fn an_already_expired_authorization_is_refused() {
    let doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    let err = policy()
        .evaluate(&doc, EXPIRY + 1)
        .expect_err("already dead");
    assert!(
        matches!(err, EvmGovernanceError::AlreadyExpired { .. }),
        "{err}"
    );
    // Exactly at the expiry is also refused: the contract compares
    // `block.timestamp > expiry`, and signing something with zero life
    // left is pointless either way.
    assert!(matches!(
        policy().evaluate(&doc, EXPIRY),
        Err(EvmGovernanceError::AlreadyExpired { .. })
    ));
}

#[test]
fn an_authorization_that_outlives_the_domains_ceiling_is_refused() {
    let strict = EvmGovernancePolicy {
        max_authorization_ttl_secs: 60,
        ..policy()
    };
    let doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    let err = strict.evaluate(&doc, NOW).expect_err("600s > 60s ceiling");
    assert!(
        matches!(
            err,
            EvmGovernanceError::TtlAboveCeiling {
                ttl_secs: 600,
                max_ttl_secs: 60
            }
        ),
        "{err}"
    );
}

// =====================================================================
// Shape, and the fields that must not appear
// =====================================================================

#[test]
fn an_unknown_protocol_version_is_refused_before_anything_else() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.protocol_version = 2;
    let err = policy().evaluate(&doc, NOW).expect_err("wrong version");
    assert!(
        matches!(
            err,
            EvmGovernanceError::UnknownProtocolVersion {
                expected: 3,
                actual: 2
            }
        ),
        "{err}"
    );
}

/// Rotation, migration and abandonment have no representation here.
#[test]
fn the_governance_actions_this_tool_cannot_produce_are_refused_as_unknown() {
    for action in [0x01u8, 0x02, 0x03, 0x05, 0x06, 0x08, 0x09, 0x0A, 0x00, 0xFF] {
        let mut doc = document(GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: true,
        });
        doc.action = action;
        let err = policy()
            .evaluate(&doc, NOW)
            .expect_err(&format!("{action:#04x} must not produce a decision"));
        assert!(
            matches!(
                err,
                EvmGovernanceError::UnknownAction { .. }
                    | EvmGovernanceError::KindActionMismatch { .. }
            ),
            "{action:#04x}: {err}"
        );
    }
}

#[test]
fn a_kind_that_disagrees_with_the_action_is_refused() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.kind = "set_limits".to_string();
    let err = policy().evaluate(&doc, NOW).expect_err("kind mismatch");
    assert!(
        matches!(err, EvmGovernanceError::KindActionMismatch { .. }),
        "{err}"
    );
}

/// A field that the named payload does not bind is a refusal, never
/// something quietly ignored — the caller would otherwise believe it had
/// bound something the digest does not cover.
#[test]
fn a_field_foreign_to_the_payload_is_refused() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.route = Some("RhnToGlc".to_string());
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::UnexpectedField { field: "route", .. })
    ));

    let mut doc = document(GovernancePayload::SetLimits(limits()));
    doc.deposits_paused = Some(true);
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::UnexpectedField {
            field: "deposits_paused",
            ..
        })
    ));

    let mut doc = document(GovernancePayload::SetRouteEnabled {
        route: Route::RhnToGlc,
        enabled: true,
    });
    doc.limits = Some(GovernanceLimitsFields {
        inbound_min: "1".into(),
        inbound_max: "1".into(),
        inbound_rolling_limit: "1".into(),
        outbound_min: "1".into(),
        outbound_max: "1".into(),
        outbound_rolling_limit: "1".into(),
        protected_min_reserve: "1".into(),
    });
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::UnexpectedField {
            field: "limits",
            ..
        })
    ));
}

#[test]
fn a_missing_required_field_is_refused_rather_than_defaulted() {
    let mut doc = document(GovernancePayload::SetPaused {
        deposits_paused: true,
        payouts_paused: true,
    });
    doc.payouts_paused = None;
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::MissingField {
            field: "payouts_paused",
            ..
        })
    ));

    let mut doc = document(GovernancePayload::SetLimits(limits()));
    doc.limits = None;
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::MissingField {
            field: "limits",
            ..
        })
    ));
}

// =====================================================================
// The two routes no signer may enable
// =====================================================================

/// A hand-built request naming a Solana-facing route is refused by the
/// ENCODER, so a signer refuses it whether or not its own configuration
/// thought to exclude it. There is no way to construct the document
/// through `from_auth` either — the payload has no digest.
#[test]
fn a_solana_goldcoin_route_is_refused_by_every_signer() {
    for route in ["GlcToSol", "SolToGlc"] {
        for enabled in [true, false] {
            let mut doc = document(GovernancePayload::SetRouteEnabled {
                route: Route::RhnToGlc,
                enabled,
            });
            doc.route = Some(route.to_string());
            let err = policy()
                .evaluate(&doc, NOW)
                .expect_err(&format!("{route} must never produce a decision"));
            // The refusal comes from the ENCODER, reached through
            // `Encoding`, so it holds for every signer configuration.
            assert!(
                matches!(
                    &err,
                    EvmGovernanceError::Encoding(GovernanceError::RouteNotGovernable { .. })
                ),
                "{route}/{enabled}: {err}"
            );
        }
    }
}

/// `set_route_enabled` round-trips for EVERY route the contract models —
/// the Goldcoin pair and, since Phase H, the Solana<->Robinhood pair —
/// and the byte each one commits to is the contract's own discriminator.
#[test]
fn set_route_enabled_round_trips_for_every_contract_route() {
    use crate::robinhood::governance::governance_route_byte;

    for (route, byte) in [
        (Route::GlcToRhn, 0x01u8),
        (Route::RhnToGlc, 0x02),
        (Route::SolToRhn, 0x03),
        (Route::RhnToSol, 0x04),
    ] {
        for enabled in [true, false] {
            let payload = GovernancePayload::SetRouteEnabled { route, enabled };
            let expected = auth(payload.clone());
            let doc = document(payload);
            assert_eq!(doc.route.as_deref(), Some(route.as_str()));
            assert_eq!(doc.route_enabled, Some(enabled));

            let decision = policy()
                .evaluate(&doc, NOW)
                .unwrap_or_else(|e| panic!("{} must be accepted: {e}", route.as_str()));
            assert_eq!(decision.auth, expected, "{}", route.as_str());
            assert_eq!(decision.digest, expected.digest(domain()).unwrap());
            assert!(
                decision
                    .summary
                    .contains(&format!("setRouteEnabled({}, {enabled})", route.as_str())),
                "{}",
                decision.summary
            );
            assert_eq!(governance_route_byte(route), Ok(byte), "{}", route.as_str());
            assert_eq!(route.contract_route_id(), Some(byte), "{}", route.as_str());
        }
    }
}

/// The two routes the contract does not model have no flag to set, so a
/// document naming one is refused — and refused by the encoder, so no
/// digest exists for it.
#[test]
fn set_route_enabled_for_a_route_the_contract_does_not_model_is_refused() {
    for route in [Route::GlcToSol, Route::SolToGlc] {
        let payload = GovernancePayload::SetRouteEnabled {
            route,
            enabled: true,
        };
        assert!(
            EvmGovernanceSignRequest::from_auth(&auth(payload), domain()).is_err(),
            "{} has no contract discriminator and must not encode",
            route.as_str()
        );
        // A document that names it by hand is refused by the signer too.
        let mut doc = document(GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        });
        doc.route = Some(route.as_str().to_string());
        assert!(
            policy().evaluate(&doc, NOW).is_err(),
            "{} must not verify",
            route.as_str()
        );
    }
}

/// A cross-route governance document is a DIFFERENT document from a
/// Goldcoin-route one: relabelling the route without rebuilding the
/// digest is a mismatch, never a signature.
#[test]
fn relabelling_a_governance_document_to_a_cross_route_is_a_digest_mismatch() {
    for route in ["SolToRhn", "RhnToSol"] {
        let mut doc = document(GovernancePayload::SetRouteEnabled {
            route: Route::RhnToGlc,
            enabled: true,
        });
        doc.route = Some(route.to_string());
        let err = policy()
            .evaluate(&doc, NOW)
            .expect_err(&format!("{route} relabelled must not verify"));
        assert!(
            matches!(&err, EvmGovernanceError::DigestMismatch { .. }),
            "{route}: {err}"
        );
    }
}

#[test]
fn a_route_the_bridge_does_not_model_is_refused() {
    let mut doc = document(GovernancePayload::SetRouteEnabled {
        route: Route::RhnToGlc,
        enabled: true,
    });
    doc.route = Some("EthToRhn".to_string());
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::MalformedField { field: "route", .. })
    ));
}

// =====================================================================
// The signer applies the contract's own limit rules
// =====================================================================

/// A limit set the contract would revert on never reaches a signing key:
/// a quorum's attention and a consumed nonce are both too expensive to
/// spend discovering it on chain.
#[test]
fn a_limit_set_the_contract_would_reject_is_refused_by_the_signer() {
    let mut bad = limits();
    bad.outbound_rolling_limit = EvmU256::from_u128(CANONICAL_SCALE); // below max
    let doc = document(GovernancePayload::SetLimits(bad));
    let err = policy().evaluate(&doc, NOW).expect_err("would revert");
    assert!(matches!(err, EvmGovernanceError::Encoding(_)), "{err}");

    let mut bad = limits();
    bad.inbound_max = EvmU256::from_u128(20 * CANONICAL_SCALE + 1); // not canonical
    let doc = document(GovernancePayload::SetLimits(bad));
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::Encoding(_))
    ));
}

#[test]
fn a_malformed_amount_is_refused_rather_than_coerced() {
    let mut doc = document(GovernancePayload::SetLimits(limits()));
    doc.limits.as_mut().unwrap().inbound_max = "twenty".to_string();
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::MalformedField {
            field: "limits.inbound_max",
            ..
        })
    ));

    let mut doc = document(GovernancePayload::SetLimits(limits()));
    doc.limits.as_mut().unwrap().inbound_max = "-1".to_string();
    assert!(matches!(
        policy().evaluate(&doc, NOW),
        Err(EvmGovernanceError::MalformedField { .. })
    ));
}

// =====================================================================
// Configuration vocabulary
// =====================================================================

#[test]
fn the_action_names_map_to_the_contract_bytes() {
    assert_eq!(
        governance_action_from_name("set_limits"),
        Some(ACTION_SET_LIMITS)
    );
    assert_eq!(
        governance_action_from_name("set_pause"),
        Some(ACTION_SET_PAUSE)
    );
    assert_eq!(
        governance_action_from_name("set_route_enabled"),
        Some(ACTION_SET_ROUTE_ENABLED)
    );
    // Nothing else, in particular nothing that rotates a signer set or
    // commits a migration.
    for name in [
        "rotate_signers",
        "commit_migration",
        "abandon",
        "payout",
        "",
    ] {
        assert_eq!(governance_action_from_name(name), None, "{name}");
    }
}

/// The document is JSON, and it survives a round trip through it — a
/// signer parses what the proposer serialized.
#[test]
fn the_document_survives_a_json_round_trip() {
    for payload in [
        GovernancePayload::SetLimits(limits()),
        GovernancePayload::SetPaused {
            deposits_paused: true,
            payouts_paused: false,
        },
        GovernancePayload::SetRouteEnabled {
            route: Route::GlcToRhn,
            enabled: true,
        },
    ] {
        let doc = document(payload);
        let json = serde_json::to_string(&doc).unwrap();
        let back: EvmGovernanceSignRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(doc, back);
        assert!(policy().evaluate(&back, NOW).is_ok(), "{json}");
        // Absent fields are omitted rather than serialized as null, so
        // the wire shape is the documented one.
        assert!(!json.contains("null"), "{json}");
    }
}
