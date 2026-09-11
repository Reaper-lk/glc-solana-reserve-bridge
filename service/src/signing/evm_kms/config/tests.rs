//! The environment contract.
//!
//! Every case here runs against an in-memory lookup rather than the
//! process environment: `std::env::set_var` is global mutable state
//! shared with every other test in this binary, and a config test that
//! raced with one of them would fail somewhere else entirely.

use std::collections::HashMap;

use super::*;

const ADDRESS: &str = "0x1111111111111111111111111111111111111111";
const CONTRACT: &str = "0x2222222222222222222222222222222222222222";
const TOKEN: &str = "0xaf0172DDEa4ce60dB3EBab05748A00B14fC8e433";
const TOKEN_LEN: usize = 40;
const GOOD_BEARER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

/// The production shape: chain 4663, the real token address, a loopback
/// bind, and no optional narrowing.
fn base() -> HashMap<&'static str, String> {
    HashMap::from([
        (ENV_BIND, "127.0.0.1:8443".to_string()),
        (ENV_KMS_KEY_ID, "alias/glc-robinhood-signer-a".to_string()),
        (ENV_BEARER_TOKEN, GOOD_BEARER.to_string()),
        (ENV_EVM_ADDRESS, ADDRESS.to_string()),
        (ENV_CHAIN_ID, "4663".to_string()),
        (ENV_VERIFYING_CONTRACT, CONTRACT.to_string()),
        (ENV_TOKEN, TOKEN.to_string()),
        (ENV_MAX_AMOUNT_ATOMIC, "10000000000000000000000".to_string()),
        (ENV_MAX_TTL_SECS, "900".to_string()),
    ])
}

fn load(env: &HashMap<&'static str, String>) -> Result<SignerConfig, SignerConfigError> {
    from_lookup(&|var| env.get(var).cloned())
}

fn with(key: &'static str, value: &str) -> HashMap<&'static str, String> {
    let mut env = base();
    env.insert(key, value.to_string());
    env
}

fn without(key: &'static str) -> HashMap<&'static str, String> {
    let mut env = base();
    env.remove(key);
    env
}

// ------------------------------------------------------------ happy path --

#[test]
fn a_complete_environment_produces_the_deployment_policy() {
    let config = load(&base()).expect("a complete environment loads");

    assert_eq!(config.bind, "127.0.0.1:8443".parse().unwrap());
    assert_eq!(config.kms_key_id, "alias/glc-robinhood-signer-a");
    assert_eq!(config.aws_region, None);
    assert_eq!(config.evm_address.to_checksum_string(), ADDRESS);

    let policy = &config.policy;
    assert_eq!(policy.chain_id.get(), 4663);
    assert_eq!(policy.verifying_contract.to_checksum_string(), CONTRACT);
    assert_eq!(policy.token.to_checksum_string(), TOKEN);
    assert_eq!(
        policy.allowed_actions,
        vec![ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE]
    );
    assert_eq!(
        policy.allowed_routes,
        vec![Route::GlcToRhn, Route::RhnToGlc]
    );
    assert_eq!(policy.max_amount_robinhood_atomic, 10_000 * 10u128.pow(18));
    assert_eq!(policy.max_authorization_ttl_secs, 900);
    // No independent source for the epoch means "do not check" — never a
    // value taken from a request.
    assert_eq!(policy.expected_signer_epoch, None);
}

/// The protocol chain pairs are this binary's own constants, not a
/// deployment parameter, and each route resolves to the pair the
/// contract reads for that direction.
#[test]
fn the_route_chain_table_is_held_independently_and_is_direction_correct() {
    let config = load(&base()).unwrap();
    let chains = &config.policy.route_chains;
    assert_eq!(chains.len(), 2);

    let glc_to_rhn = chains
        .iter()
        .find(|(r, _)| *r == Route::GlcToRhn)
        .unwrap()
        .1;
    assert_eq!(glc_to_rhn.source, PROTOCOL_CHAIN_GOLDCOIN);
    assert_eq!(glc_to_rhn.dest, PROTOCOL_CHAIN_ROBINHOOD);

    let rhn_to_glc = chains
        .iter()
        .find(|(r, _)| *r == Route::RhnToGlc)
        .unwrap()
        .1;
    assert_eq!(rhn_to_glc.source, PROTOCOL_CHAIN_ROBINHOOD);
    assert_eq!(rhn_to_glc.dest, PROTOCOL_CHAIN_GOLDCOIN);

    assert_eq!(PROTOCOL_CHAIN_GOLDCOIN, 1001);
    assert_eq!(PROTOCOL_CHAIN_ROBINHOOD, 2001);
}

#[test]
fn an_explicit_region_and_signer_epoch_are_carried_through() {
    let mut env = base();
    env.insert(ENV_AWS_REGION, "us-east-1".to_string());
    env.insert(ENV_SIGNER_EPOCH, "7".to_string());
    let config = load(&env).unwrap();
    assert_eq!(config.aws_region.as_deref(), Some("us-east-1"));
    assert_eq!(config.policy.expected_signer_epoch, Some(7));
}

// ----------------------------------------------------- nothing defaults --

/// The rule `EvmSignerPolicy` states: a domain provisions every value.
/// A missing one is a startup failure, never a guess — and in particular
/// the verifying contract, which is unknowable before deployment.
#[test]
fn every_required_variable_is_actually_required() {
    for var in [
        ENV_BIND,
        ENV_KMS_KEY_ID,
        ENV_BEARER_TOKEN,
        ENV_EVM_ADDRESS,
        ENV_CHAIN_ID,
        ENV_VERIFYING_CONTRACT,
        ENV_TOKEN,
        ENV_MAX_AMOUNT_ATOMIC,
        ENV_MAX_TTL_SECS,
    ] {
        assert_eq!(
            load(&without(var)),
            Err(SignerConfigError::Missing { var }),
            "{var} must be required"
        );
        assert_eq!(
            load(&with(var, "   ")),
            Err(SignerConfigError::Empty { var }),
            "{var} must reject a blank value"
        );
    }
}

/// An optional that is SET but blank is a mistake, not "unset": treating
/// it as absent would silently give the operator a policy they did not
/// write.
#[test]
fn a_blank_optional_is_an_error_rather_than_an_absence() {
    for var in [
        ENV_ALLOWED_ACTIONS,
        ENV_ALLOWED_ROUTES,
        ENV_SIGNER_EPOCH,
        ENV_AWS_REGION,
    ] {
        assert_eq!(
            load(&with(var, "")),
            Err(SignerConfigError::Empty { var }),
            "{var}"
        );
    }
}

// ---------------------------------------------------------------- binding --

#[test]
fn the_wildcard_bind_is_refused_by_name() {
    for addr in ["0.0.0.0:8443", "[::]:8443"] {
        let err = load(&with(ENV_BIND, addr)).unwrap_err();
        assert!(
            matches!(err, SignerConfigError::BindUnspecified { .. }),
            "{addr} produced {err:?}"
        );
    }
}

#[test]
fn a_publicly_routable_bind_is_refused() {
    // TEST-NET-3 and a documentation IPv6 prefix — deliberately not
    // anybody's real address, but routable in shape.
    for addr in ["203.0.113.5:8443", "[2001:db8::1]:8443", "8.8.8.8:443"] {
        let err = load(&with(ENV_BIND, addr)).unwrap_err();
        assert!(
            matches!(err, SignerConfigError::BindNotPrivate { .. }),
            "{addr} produced {err:?}"
        );
    }
}

#[test]
fn loopback_and_private_binds_are_accepted() {
    for addr in [
        "127.0.0.1:8443",
        "127.0.0.53:9000",
        "10.0.4.7:8443",
        "172.16.0.1:8443",
        "192.168.1.20:8443",
        "100.100.4.4:8443",
        "169.254.1.1:8443",
        "[::1]:8443",
        "[fd00::1]:8443",
        "[fe80::1]:8443",
    ] {
        assert!(
            load(&with(ENV_BIND, addr)).is_ok(),
            "{addr} must be allowed"
        );
    }
}

// ------------------------------------------------------------ the credential --

#[test]
fn a_short_bearer_token_will_not_start_the_service() {
    let err = load(&with(ENV_BEARER_TOKEN, "hunter2")).unwrap_err();
    assert_eq!(
        err,
        SignerConfigError::BearerTokenTooShort {
            var: ENV_BEARER_TOKEN,
            minimum: MIN_BEARER_TOKEN_CHARS,
        }
    );
    // The message must not be able to carry the value it refused.
    assert!(!err.to_string().contains("hunter2"));
}

#[test]
fn the_token_authenticates_only_the_exact_bearer_header() {
    let token = BearerToken::new(GOOD_BEARER).unwrap();
    assert!(token.authenticates(&format!("Bearer {GOOD_BEARER}")));

    for header in [
        // Wrong token.
        &format!("Bearer {}", "f".repeat(48)) as &str,
        // Right token, wrong scheme spelling.
        &format!("bearer {GOOD_BEARER}"),
        &format!("Bearer  {GOOD_BEARER}"),
        &format!("Basic {GOOD_BEARER}"),
        // Right token, no scheme.
        GOOD_BEARER,
        // Prefix of the real token.
        &format!("Bearer {}", &GOOD_BEARER[..20]),
        "Bearer ",
        "",
    ] {
        assert!(
            !token.authenticates(header),
            "{header:?} must not authenticate"
        );
    }
}

#[test]
fn the_token_never_renders_itself() {
    let token = BearerToken::new(GOOD_BEARER).unwrap();
    let rendered = format!("{token:?}");
    assert_eq!(rendered, "BearerToken(<redacted>)");
    assert!(!rendered.contains(GOOD_BEARER));

    // And the whole config, which is what a startup log line would print.
    let config = load(&base()).unwrap();
    assert!(!format!("{config:?}").contains(GOOD_BEARER));
}

// --------------------------------------------------------------- addresses --

#[test]
fn the_zero_address_is_refused_wherever_it_appears() {
    let zero = "0x0000000000000000000000000000000000000000";
    for var in [ENV_EVM_ADDRESS, ENV_VERIFYING_CONTRACT, ENV_TOKEN] {
        assert_eq!(
            load(&with(var, zero)),
            Err(SignerConfigError::ZeroAddress { var }),
            "{var}"
        );
    }
}

#[test]
fn two_roles_may_not_be_the_same_address() {
    let err = load(&with(ENV_VERIFYING_CONTRACT, ADDRESS)).unwrap_err();
    assert!(
        matches!(err, SignerConfigError::AddressesCollide { .. }),
        "{err:?}"
    );
    let err = load(&with(ENV_TOKEN, CONTRACT)).unwrap_err();
    assert!(
        matches!(err, SignerConfigError::AddressesCollide { .. }),
        "{err:?}"
    );
}

/// EIP-55 is checked by `EvmAddress`'s own parser; a mistyped digit in a
/// checksummed address is a startup failure rather than a different
/// deployment.
#[test]
fn a_mistyped_checksummed_address_is_refused() {
    assert_eq!(TOKEN.len(), 2 + TOKEN_LEN);
    let mut broken = TOKEN.to_string();
    broken.replace_range(3..4, "F");
    let err = load(&with(ENV_TOKEN, &broken)).unwrap_err();
    assert!(
        matches!(err, SignerConfigError::Malformed { var: ENV_TOKEN, .. }),
        "{err:?}"
    );
}

// ------------------------------------------------------------- narrowing --

#[test]
fn allowed_actions_can_be_narrowed_but_not_invented() {
    let config = load(&with(ENV_ALLOWED_ACTIONS, "refund, settlement")).unwrap();
    assert_eq!(
        config.policy.allowed_actions,
        vec![ACTION_REFUND, ACTION_SETTLE]
    );

    // Duplicates collapse rather than producing two policy rows.
    let config = load(&with(ENV_ALLOWED_ACTIONS, "payout,payout")).unwrap();
    assert_eq!(config.policy.allowed_actions, vec![ACTION_PAYOUT]);

    let err = load(&with(ENV_ALLOWED_ACTIONS, "payout,abandonment")).unwrap_err();
    assert_eq!(
        err,
        SignerConfigError::UnknownAction {
            var: ENV_ALLOWED_ACTIONS,
            action: "abandonment".to_string(),
        }
    );
    assert_eq!(
        load(&with(ENV_ALLOWED_ACTIONS, " , ")),
        Err(SignerConfigError::EmptyAllowList {
            var: ENV_ALLOWED_ACTIONS
        })
    );
}

/// A route this bridge models but this binary does not serve is an
/// explicit refusal, not a silently dropped entry — an operator who
/// wrote it believed it would do something.
#[test]
fn only_the_two_robinhood_routes_may_be_named() {
    let config = load(&with(ENV_ALLOWED_ROUTES, "GlcToRhn")).unwrap();
    assert_eq!(config.policy.allowed_routes, vec![Route::GlcToRhn]);
    // The chain table narrows with it: an unserved route has no entry to
    // disagree about.
    assert_eq!(config.policy.route_chains.len(), 1);
    assert_eq!(config.policy.route_chains[0].0, Route::GlcToRhn);

    for route in ["GlcToSol", "SolToGlc", "SolToRhn", "RhnToSol", "nonsense"] {
        assert_eq!(
            load(&with(ENV_ALLOWED_ROUTES, route)),
            Err(SignerConfigError::RouteNotServed {
                var: ENV_ALLOWED_ROUTES,
                route: route.to_string(),
            }),
            "{route}"
        );
    }
    assert_eq!(
        load(&with(ENV_ALLOWED_ROUTES, " , ")),
        Err(SignerConfigError::EmptyAllowList {
            var: ENV_ALLOWED_ROUTES
        })
    );
}

// ------------------------------------------------------------- numeric --

#[test]
fn zero_ceilings_and_a_zero_chain_id_are_refused() {
    assert_eq!(
        load(&with(ENV_MAX_AMOUNT_ATOMIC, "0")),
        Err(SignerConfigError::MustBePositive {
            var: ENV_MAX_AMOUNT_ATOMIC
        })
    );
    assert_eq!(
        load(&with(ENV_MAX_TTL_SECS, "0")),
        Err(SignerConfigError::MustBePositive {
            var: ENV_MAX_TTL_SECS
        })
    );
    let err = load(&with(ENV_CHAIN_ID, "0")).unwrap_err();
    assert!(
        matches!(
            err,
            SignerConfigError::Malformed {
                var: ENV_CHAIN_ID,
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn a_non_numeric_ceiling_is_refused_rather_than_defaulted() {
    for (var, value) in [
        (ENV_MAX_AMOUNT_ATOMIC, "1e18"),
        (ENV_MAX_TTL_SECS, "15m"),
        (ENV_CHAIN_ID, "0x1237"),
        (ENV_SIGNER_EPOCH, "latest"),
    ] {
        let err = load(&with(var, value)).unwrap_err();
        assert!(
            matches!(&err, SignerConfigError::Malformed { var: v, .. } if *v == var),
            "{var}={value} produced {err:?}"
        );
    }
}

// =====================================================================
// The governance opt-in
// =====================================================================
//
// Unset means NONE. That default is the whole security property: shipping
// a signer binary that understands the governance protocol must not, by
// itself, widen what the custody key will sign.

#[test]
fn governance_is_absent_from_a_configuration_that_does_not_mention_it() {
    let config = load(&base()).expect("the base environment is valid");
    assert!(
        config.governance.allowed_actions.is_empty(),
        "governance must be off unless a domain opts in"
    );
    assert!(!config.governance.is_enabled());
    // And the value-moving policy is untouched by its absence.
    assert_eq!(
        config.policy.allowed_actions,
        vec![ACTION_PAYOUT, ACTION_REFUND, ACTION_SETTLE]
    );
}

#[test]
fn a_domain_can_opt_in_to_some_governance_actions() {
    let config = load(&with(
        ENV_ALLOWED_GOVERNANCE_ACTIONS,
        "set_pause,set_limits",
    ))
    .expect("a valid opt-in");
    assert_eq!(
        config.governance.allowed_actions,
        vec![
            crate::robinhood::governance::ACTION_SET_PAUSE,
            crate::robinhood::governance::ACTION_SET_LIMITS,
        ],
        "sorted and de-duplicated, so the order is deterministic"
    );
    assert!(config.governance.is_enabled());
    // It inherits the deployment identity and ceilings, rather than
    // offering a second set of knobs to get wrong.
    assert_eq!(config.governance.chain_id, config.policy.chain_id);
    assert_eq!(
        config.governance.verifying_contract,
        config.policy.verifying_contract
    );
    assert_eq!(
        config.governance.max_authorization_ttl_secs,
        config.policy.max_authorization_ttl_secs
    );
}

#[test]
fn a_repeated_governance_action_is_not_a_repeated_policy_row() {
    let config = load(&with(
        ENV_ALLOWED_GOVERNANCE_ACTIONS,
        "set_pause, set_pause ,set_pause",
    ))
    .expect("valid");
    assert_eq!(
        config.governance.allowed_actions,
        vec![crate::robinhood::governance::ACTION_SET_PAUSE]
    );
}

/// The actions this tool cannot produce cannot be configured either — a
/// domain cannot grant its key an authority the encoder has no way to
/// exercise.
#[test]
fn an_action_this_tool_cannot_produce_cannot_be_granted() {
    for name in [
        "rotate_signers",
        "rotate_guardians",
        "commit_migration",
        "finalize_migration",
        "abandon",
        "payout",
        "settlement",
        "SET_PAUSE",
    ] {
        let err = load(&with(ENV_ALLOWED_GOVERNANCE_ACTIONS, name))
            .expect_err(&format!("{name} must not be grantable"));
        assert!(
            matches!(err, SignerConfigError::UnknownAction { .. }),
            "{name}: {err}"
        );
    }
}

/// Writing the variable and leaving it empty is a mistake worth naming:
/// an operator who wrote it meant to grant something. Declining is done
/// by not setting it.
#[test]
fn an_explicitly_empty_governance_allow_list_is_refused() {
    for value in ["", "  ", ",", " , "] {
        let err = load(&with(ENV_ALLOWED_GOVERNANCE_ACTIONS, value))
            .expect_err("an explicitly empty list is a mistake");
        assert!(
            matches!(
                err,
                SignerConfigError::Empty { .. } | SignerConfigError::EmptyAllowList { .. }
            ),
            "{value:?}: {err}"
        );
    }
}

// ---------------------------------------------------- treasury withdrawal --

/// Deploying this binary grants nothing: the default action set is the
/// original three and the treasury list is empty.
#[test]
fn treasury_withdrawal_is_not_granted_by_default() {
    let config = load(&base()).unwrap();
    assert!(!config
        .policy
        .allowed_actions
        .contains(&crate::robinhood::auth::ACTION_TREASURY_WITHDRAW));
    assert!(config.policy.allowed_treasuries.is_empty());
}

#[test]
fn a_domain_opts_in_with_both_the_action_and_a_treasury() {
    let mut env = with(ENV_ALLOWED_ACTIONS, "payout,treasury_withdraw");
    env.insert(
        ENV_ALLOWED_TREASURIES,
        "0x000000000000000000000000000000000000ae5B".to_string(),
    );
    let config = load(&env).unwrap();
    assert!(config
        .policy
        .allowed_actions
        .contains(&crate::robinhood::auth::ACTION_TREASURY_WITHDRAW));
    assert_eq!(
        config.policy.allowed_treasuries,
        vec!["0x000000000000000000000000000000000000ae5B"
            .parse::<crate::evm::EvmAddress>()
            .unwrap()]
    );
}

#[test]
fn a_blank_or_zero_treasury_list_is_an_error_not_an_absence() {
    assert!(matches!(
        load(&with(ENV_ALLOWED_TREASURIES, "")).unwrap_err(),
        SignerConfigError::Empty { .. } | SignerConfigError::EmptyAllowList { .. }
    ));
    assert!(matches!(
        load(&with(
            ENV_ALLOWED_TREASURIES,
            "0x0000000000000000000000000000000000000000"
        ))
        .unwrap_err(),
        SignerConfigError::Malformed { .. }
    ));
    assert!(matches!(
        load(&with(ENV_ALLOWED_TREASURIES, "not-an-address")).unwrap_err(),
        SignerConfigError::Malformed { .. }
    ));
}
