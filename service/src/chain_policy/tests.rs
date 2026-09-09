use super::*;

fn policy(fee_bps: u64, per_transfer: u64, rolling: u64) -> Result<ChainPolicy, ChainPolicyError> {
    ChainPolicy::new(
        Chain::Robinhood,
        fee_bps,
        CanonicalAtomic(per_transfer),
        CanonicalAtomic(rolling),
    )
}

/// The exact launch policy this change exists to make configurable:
/// 6.00%, 20,000 GLC per transfer, 10,000,000 GLC per strict 24h.
#[test]
fn robinhood_launch_policy_is_accepted_exactly_as_specified() {
    let p = policy(600, 2_000_000_000_000, 1_000_000_000_000_000).expect("the approved policy");
    assert_eq!(p.chain(), Chain::Robinhood);
    assert_eq!(p.fee_bps(), 600);
    assert_eq!(p.per_transfer_limit(), CanonicalAtomic(2_000_000_000_000));
    assert_eq!(
        p.rolling_daily_limit(),
        CanonicalAtomic(1_000_000_000_000_000)
    );
    // Sanity on the human figures the runbook states, so a typo in the
    // atomic values cannot pass review.
    assert_eq!(p.per_transfer_limit().0 / 100_000_000, 20_000);
    assert_eq!(p.rolling_daily_limit().0 / 100_000_000, 10_000_000);
}

#[test]
fn solana_can_never_be_given_a_configured_policy() {
    // The whole point of POLICY_GOVERNED_CHAINS: no config file, however
    // written, can change the Goldcoin<->Solana fee or limits.
    assert!(!POLICY_GOVERNED_CHAINS.contains(&Chain::Solana));
    assert!(!POLICY_GOVERNED_CHAINS.contains(&Chain::Goldcoin));
    for chain in [Chain::Solana, Chain::Goldcoin] {
        assert!(matches!(
            ChainPolicy::new(
                chain,
                600,
                CanonicalAtomic(2_000_000_000_000),
                CanonicalAtomic(1_000_000_000_000_000)
            ),
            Err(ChainPolicyError::ChainNotPolicyGoverned { .. })
        ));
    }
}

#[test]
fn a_robinhood_policy_does_not_change_any_other_chains_fee() {
    let mut policies = ChainPolicies::new();
    policies
        .insert(policy(600, 2_000_000_000_000, 1_000_000_000_000_000).unwrap())
        .unwrap();

    assert_eq!(policies.fee_bps_for(Chain::Robinhood), 600);
    // Solana and Goldcoin keep the compiled-in rate. Asserted against
    // BRIDGE_FEE_BPS rather than against `300` so this test keeps meaning
    // the right thing if the global rate ever changes.
    assert_eq!(policies.fee_bps_for(Chain::Solana), BRIDGE_FEE_BPS);
    assert_eq!(policies.fee_bps_for(Chain::Goldcoin), BRIDGE_FEE_BPS);
    assert_ne!(policies.fee_bps_for(Chain::Solana), 600);
    assert!(policies.get(Chain::Solana).is_none());
    assert!(policies.get(Chain::Goldcoin).is_none());
}

#[test]
fn an_empty_policy_set_leaves_every_chain_on_the_compiled_in_rate() {
    let policies = ChainPolicies::new();
    assert!(policies.is_empty());
    for chain in Chain::ALL {
        assert_eq!(policies.fee_bps_for(chain), BRIDGE_FEE_BPS);
        assert!(policies.get(chain).is_none());
    }
}

#[test]
fn duplicate_policies_for_one_chain_are_refused() {
    let mut policies = ChainPolicies::new();
    policies
        .insert(policy(600, 2_000_000_000_000, 1_000_000_000_000_000).unwrap())
        .unwrap();
    assert!(matches!(
        policies.insert(policy(300, 1_000_000_000_000, 1_000_000_000_000_000).unwrap()),
        Err(ChainPolicyError::DuplicatePolicy { .. })
    ));
    // The first policy stands; a refused insert never half-applies.
    assert_eq!(policies.fee_bps_for(Chain::Robinhood), 600);
}

#[test]
fn a_zero_fee_rate_is_refused() {
    assert!(matches!(
        policy(0, 2_000_000_000_000, 1_000_000_000_000_000),
        Err(ChainPolicyError::ZeroFeeBps { .. })
    ));
}

#[test]
fn a_fee_rate_at_or_above_one_hundred_percent_is_refused() {
    for fee_bps in [BPS_DENOMINATOR, BPS_DENOMINATOR + 1, u64::MAX] {
        assert!(matches!(
            policy(fee_bps, 2_000_000_000_000, 1_000_000_000_000_000),
            Err(ChainPolicyError::FeeBpsOutOfRange { .. })
        ));
    }
}

#[test]
fn a_fee_rate_the_protocol_does_not_know_is_refused() {
    // 450 bps is in range and still refused: a request priced at it would
    // be rejected by `verify_fee_breakdown` at settlement, so accepting it
    // here would only defer the failure to after a user's money moved.
    assert!(!HISTORICAL_FEE_BPS.contains(&450));
    assert!(matches!(
        policy(450, 2_000_000_000_000, 1_000_000_000_000_000),
        Err(ChainPolicyError::UnknownFeeBps { .. })
    ));
    // Every rate the protocol does know is accepted.
    for rate in HISTORICAL_FEE_BPS {
        assert!(policy(*rate, 2_000_000_000_000, 1_000_000_000_000_000).is_ok());
    }
}

#[test]
fn a_zero_transfer_limit_is_refused() {
    assert!(matches!(
        policy(600, 0, 1_000_000_000_000_000),
        Err(ChainPolicyError::ZeroPerTransferLimit { .. })
    ));
    assert!(matches!(
        policy(600, 2_000_000_000_000, 0),
        Err(ChainPolicyError::ZeroRollingDailyLimit { .. })
    ));
}

#[test]
fn a_rolling_limit_below_the_per_transfer_limit_is_refused() {
    assert!(matches!(
        policy(600, 2_000_000_000_000, 1_999_999_999_999),
        Err(ChainPolicyError::RollingBelowPerTransfer { .. })
    ));
    // Equal is the boundary and is allowed: exactly one maximum-size
    // transfer per day is a coherent, if strict, policy.
    assert!(policy(600, 2_000_000_000_000, 2_000_000_000_000).is_ok());
}

#[test]
fn the_largest_representable_limits_are_accepted() {
    // No arbitrary ceiling of this module's own invention: the canonical
    // unit's own range is the range.
    let p = policy(600, u64::MAX, u64::MAX).expect("u64::MAX canonical is a valid statement");
    assert_eq!(p.rolling_daily_limit(), CanonicalAtomic(u64::MAX));
}

#[test]
fn every_error_message_names_the_chain() {
    let errors = [
        ChainPolicyError::ChainNotPolicyGoverned { chain: "solana" },
        ChainPolicyError::DuplicatePolicy { chain: "robinhood" },
        ChainPolicyError::ZeroFeeBps { chain: "robinhood" },
        ChainPolicyError::FeeBpsOutOfRange {
            chain: "robinhood",
            fee_bps: 10_000,
        },
        ChainPolicyError::UnknownFeeBps {
            chain: "robinhood",
            fee_bps: 450,
            known: HISTORICAL_FEE_BPS,
        },
        ChainPolicyError::ZeroPerTransferLimit { chain: "robinhood" },
        ChainPolicyError::ZeroRollingDailyLimit { chain: "robinhood" },
        ChainPolicyError::RollingBelowPerTransfer {
            chain: "robinhood",
            per_transfer: 2,
            rolling: 1,
        },
    ];
    for error in errors {
        let text = error.to_string();
        assert!(
            text.contains("robinhood") || text.contains("solana"),
            "error does not name its chain: {text}"
        );
    }
}
