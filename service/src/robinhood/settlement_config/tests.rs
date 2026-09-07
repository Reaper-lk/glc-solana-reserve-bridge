//! Settlement-configuration validation tests. Every refusal below is a
//! misconfiguration that would otherwise run happily and be wrong.

use super::*;
use crate::robinhood::RobinhoodIndexerConfig;

fn addr(byte: u8) -> EvmAddress {
    EvmAddress::from_bytes([byte; 20])
}

fn chain() -> EvmChainId {
    EvmChainId::new(4663).unwrap()
}

fn indexer() -> RobinhoodIndexerConfig {
    RobinhoodIndexerConfig::new(
        "https://rpc.example".to_string(),
        chain(),
        addr(0xb1),
        addr(0x70),
        100,
        12,
        2_000,
        5_000,
        1_000,
    )
    .unwrap()
}

struct Params {
    chain_id: EvmChainId,
    bridge_contract: EvmAddress,
    envelope: TxEnvelope,
    key_env: String,
    submitter: EvmAddress,
    signers: [EvmAddress; 3],
    ttl: u64,
    confirmations: u64,
    margin: u64,
    max_gas: u64,
    max_fee: u128,
    priority: u128,
    rebroadcast: u64,
    replacements: u32,
    min_balance: u128,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            chain_id: chain(),
            bridge_contract: addr(0xb1),
            envelope: TxEnvelope::Eip1559,
            key_env: "GLC_RHN_SUBMITTER_KEY".to_string(),
            submitter: addr(0x5a),
            signers: [addr(0xa1), addr(0xa2), addr(0xa3)],
            ttl: 900,
            confirmations: 12,
            margin: 130,
            max_gas: 500_000,
            max_fee: 100_000_000_000,
            priority: 1_000_000_000,
            rebroadcast: 120,
            replacements: 3,
            min_balance: 10_000_000_000_000_000,
        }
    }
}

fn build(p: Params) -> Result<RobinhoodSettlementConfig, RobinhoodSettlementConfigError> {
    RobinhoodSettlementConfig::new(
        Some(&indexer()),
        p.chain_id,
        p.bridge_contract,
        p.envelope,
        p.key_env,
        p.submitter,
        p.signers,
        p.ttl,
        p.confirmations,
        p.margin,
        p.max_gas,
        p.max_fee,
        p.priority,
        p.rebroadcast,
        p.replacements,
        p.min_balance,
    )
}

#[test]
fn a_well_formed_section_resolves() {
    let cfg = build(Params::default()).unwrap();
    assert_eq!(cfg.chain_id, chain());
    assert_eq!(cfg.tx_envelope, TxEnvelope::Eip1559);
    assert_eq!(cfg.authorization_ttl, Duration::from_secs(900));
    assert_eq!(cfg.domain().verifying_contract, addr(0xb1));
}

#[test]
fn settling_without_observing_is_refused() {
    let p = Params::default();
    let result = RobinhoodSettlementConfig::new(
        None,
        p.chain_id,
        p.bridge_contract,
        p.envelope,
        p.key_env,
        p.submitter,
        p.signers,
        p.ttl,
        p.confirmations,
        p.margin,
        p.max_gas,
        p.max_fee,
        p.priority,
        p.rebroadcast,
        p.replacements,
        p.min_balance,
    );
    assert_eq!(result, Err(RobinhoodSettlementConfigError::IndexerMissing));
}

#[test]
fn watching_one_network_and_settling_on_another_is_refused() {
    let result = build(Params {
        chain_id: EvmChainId::new(46630).unwrap(),
        ..Default::default()
    });
    assert!(matches!(
        result,
        Err(RobinhoodSettlementConfigError::ChainIdMismatch {
            settlement: 46630,
            indexer: 4663
        })
    ));
}

#[test]
fn watching_one_contract_and_settling_against_another_is_refused() {
    let result = build(Params {
        bridge_contract: addr(0xbe),
        ..Default::default()
    });
    assert!(matches!(
        result,
        Err(RobinhoodSettlementConfigError::BridgeContractMismatch { .. })
    ));
}

#[test]
fn a_private_key_pasted_into_the_env_var_name_field_is_refused_without_echoing_it() {
    let secret = "0x".to_string() + &"ab".repeat(32);
    let result = build(Params {
        key_env: secret.clone(),
        ..Default::default()
    });
    match result {
        Err(RobinhoodSettlementConfigError::SubmitterKeyEnvLooksLikeASecret { name }) => {
            assert_eq!(name, "<redacted>");
            // The whole point: the refusal must not put the secret into a
            // log line, an error message, or a terminal scrollback.
            let rendered = RobinhoodSettlementConfigError::SubmitterKeyEnvLooksLikeASecret { name }
                .to_string();
            assert!(!rendered.contains("abab"), "{rendered}");
        }
        other => panic!("expected a redacted refusal, got {other:?}"),
    }
}

#[test]
fn an_empty_env_var_name_is_refused() {
    assert_eq!(
        build(Params {
            key_env: "   ".to_string(),
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::EmptySubmitterKeyEnv)
    );
}

#[test]
fn the_gas_payer_may_not_also_be_a_bridge_authority() {
    // The separation this module exists to state. The contract would not
    // notice — it never reads `msg.sender` on an authorized path — but an
    // operator who configured one key for both roles has collapsed two
    // custody domains without meaning to.
    let result = build(Params {
        submitter: addr(0xa2),
        ..Default::default()
    });
    assert!(matches!(
        result,
        Err(RobinhoodSettlementConfigError::SubmitterIsAnAuthorizedSigner { .. })
    ));
}

#[test]
fn a_duplicated_signer_is_refused() {
    let result = build(Params {
        signers: [addr(0xa1), addr(0xa1), addr(0xa3)],
        ..Default::default()
    });
    assert!(matches!(
        result,
        Err(RobinhoodSettlementConfigError::DuplicateAuthorizedSigner { .. })
    ));
}

#[test]
fn zero_addresses_are_refused_everywhere_they_could_appear() {
    assert!(matches!(
        build(Params {
            submitter: EvmAddress::ZERO,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::ZeroSubmitterAddress)
    ));
    assert!(matches!(
        build(Params {
            signers: [addr(0xa1), EvmAddress::ZERO, addr(0xa3)],
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::ZeroAuthorizedSigner { index: 1 })
    ));
}

#[test]
fn the_bridge_address_may_not_double_as_the_submitter_or_a_signer() {
    let result = build(Params {
        submitter: addr(0xb1),
        ..Default::default()
    });
    assert!(matches!(
        result,
        Err(RobinhoodSettlementConfigError::BridgeContractReused { .. })
    ));
}

#[test]
fn the_authorization_ttl_has_both_a_floor_and_a_ceiling() {
    assert!(matches!(
        build(Params {
            ttl: 30,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::AuthorizationTtlOutOfRange { .. })
    ));
    assert!(matches!(
        build(Params {
            ttl: 48 * 3600,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::AuthorizationTtlOutOfRange { .. })
    ));
    // The exact boundaries are inclusive.
    assert!(build(Params {
        ttl: RobinhoodSettlementConfig::MIN_AUTHORIZATION_TTL_SECS,
        ..Default::default()
    })
    .is_ok());
    assert!(build(Params {
        ttl: RobinhoodSettlementConfig::MAX_AUTHORIZATION_TTL_SECS,
        ..Default::default()
    })
    .is_ok());
}

#[test]
fn a_zero_confirmation_depth_is_refused() {
    assert_eq!(
        build(Params {
            confirmations: 0,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::ZeroRequiredConfirmations)
    );
}

#[test]
fn a_gas_margin_below_the_estimate_is_refused() {
    // Below 100% means signing a gas limit BELOW the node's own estimate,
    // which is an out-of-gas revert that still consumes the nonce.
    assert!(matches!(
        build(Params {
            margin: 99,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::GasMarginOutOfRange { actual: 99, .. })
    ));
    assert!(matches!(
        build(Params {
            margin: 1000,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::GasMarginOutOfRange { .. })
    ));
}

#[test]
fn a_tip_at_or_above_the_fee_ceiling_is_refused() {
    // The tip is paid out of the ceiling, so a tip at the ceiling leaves
    // nothing for the base fee and the transaction can never be included.
    assert!(matches!(
        build(Params {
            priority: 100_000_000_000,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::PriorityFeeExceedsMaxFee { .. })
    ));
}

#[test]
fn a_zero_fee_ceiling_is_refused() {
    assert_eq!(
        build(Params {
            max_fee: 0,
            priority: 0,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::ZeroMaxFee)
    );
}

#[test]
fn an_unbounded_replacement_budget_is_refused() {
    assert!(matches!(
        build(Params {
            replacements: 100,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::TooManyReplacements { actual: 100, .. })
    ));
}

#[test]
fn a_max_gas_limit_below_the_evm_floor_is_refused() {
    assert!(matches!(
        build(Params {
            max_gas: 1_000,
            ..Default::default()
        }),
        Err(RobinhoodSettlementConfigError::MaxGasLimitTooLow { .. })
    ));
}

#[test]
fn the_gas_limit_applies_the_margin_and_then_the_cap() {
    let cfg = build(Params::default()).unwrap();
    assert_eq!(cfg.gas_limit_for(100_000), 130_000, "estimate + 30%");
    assert_eq!(
        cfg.gas_limit_for(1_000_000),
        500_000,
        "the cap wins over the margin"
    );
    // A hostile RPC returning an enormous estimate lands on the cap
    // rather than overflowing or committing unbounded gas.
    assert_eq!(cfg.gas_limit_for(u64::MAX), 500_000);
    assert!(cfg.gas_limit_for(0) >= 1, "never a zero gas limit");
}

#[test]
fn both_envelopes_are_expressible_and_neither_is_a_default() {
    for envelope in [TxEnvelope::Legacy, TxEnvelope::Eip1559] {
        let cfg = build(Params {
            envelope,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(cfg.tx_envelope, envelope);
    }
    // There is deliberately no `Default` impl to fall back on — the
    // choice must be made explicitly and is verified against the chain.
    assert!(!TxEnvelope::Legacy.requires_base_fee());
    assert!(TxEnvelope::Eip1559.requires_base_fee());
}
