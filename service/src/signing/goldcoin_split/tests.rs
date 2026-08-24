use super::*;
use crate::goldcoin::address::Network;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::multisig;
use crate::ledger::Ledger;
use crate::signing::goldcoin_vault::DevVaultSigner;

const TEST_SIGNER_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_THRESHOLD: usize = 2;
const CHUNK_TARGET: u64 = 12_500 * 100_000_000; // 12,500 GLC
const FEE_RATE: u64 = 1000;

fn vault_and_signers() -> (MultisigVault, Vec<Box<dyn VaultSigner>>) {
    let signers = vec![
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
        DevVaultSigner::generate(),
    ];
    let vault = MultisigVault::new(
        signers.iter().map(|s| s.pubkey).collect(),
        2,
        Network::Testnet,
    )
    .unwrap();
    let boxed: Vec<Box<dyn VaultSigner>> = signers
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn VaultSigner>)
        .collect();
    (vault, boxed)
}

fn configure_reserve(ledger: &mut Ledger, initial_balance: u64, protected_minimum: u64) {
    // warning/critical are set relative to protected_minimum (not
    // initial_balance) so this helper stays valid regardless of how close
    // initial_balance is to protected_minimum — `configure_reserve`
    // requires critical_reserve > protected_minimum unconditionally.
    ledger
        .configure_reserve(
            ReserveDirection::GoldcoinReserve,
            initial_balance,
            protected_minimum,
            initial_balance,
            protected_minimum + 2_000 * 100_000_000,
            protected_minimum + 1_000 * 100_000_000,
            0,
        )
        .unwrap();
}

fn sync_root_utxo(ledger: &mut Ledger, vault: &MultisigVault, amount_atomic: u64) -> VaultUtxo {
    let utxo = VaultUtxo {
        txid: [0xCCu8; 32],
        vout: 0,
        amount_atomic,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    ledger
        .sync_vault_utxos(&[(utxo.clone(), 20, vault.script_pubkey_hex())], 1, 0)
        .unwrap();
    utxo
}

#[tokio::test]
async fn splits_a_large_mature_utxo_into_smaller_ones() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let (plan, mut tx, partials) = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        source.txid,
        source.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();

    assert!(plan.output_count() >= 2);
    assert!(plan
        .output_amounts
        .iter()
        .all(|&a| a < source.amount_atomic));

    let sighash = tx.sighash_all(0, &vault.redeem_script());
    tx.inputs[0].script_sig = multisig::assemble(&vault, &sighash, &partials).unwrap();
    assert!(!tx.inputs[0].script_sig.is_empty());

    // Every output really does pay the vault's own script.
    for out in &tx.outputs {
        assert_eq!(out.script_pubkey, vault.script_pubkey());
    }
}

#[tokio::test]
async fn refuses_when_the_split_would_breach_the_protected_minimum() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    // Balance barely above protected_minimum: spending the source UTXO's
    // full value out of it must breach the floor.
    configure_reserve(&mut ledger, 21_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        source.txid,
        source.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SplitSigningError::SafetyCheckFailed { .. }));
}

#[tokio::test]
async fn refuses_a_source_utxo_that_is_not_available() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    // Confirmations below min_confirmations -> synced as Unconfirmed.
    let utxo = VaultUtxo {
        txid: [0xDDu8; 32],
        vout: 0,
        amount_atomic: 90_100 * 100_000_000,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    ledger
        .sync_vault_utxos(&[(utxo.clone(), 2, vault.script_pubkey_hex())], 20, 0)
        .unwrap();

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        utxo.txid,
        utxo.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        SplitSigningError::SourceNotAvailable { state, .. } if state == "Unconfirmed"
    ));
}

#[tokio::test]
async fn refuses_an_unknown_source_outpoint() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        [0xEEu8; 32],
        0,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SplitSigningError::SourceNotFound { .. }));
}

#[tokio::test]
async fn refuses_a_utxo_that_does_not_belong_to_the_root_vault() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let utxo = VaultUtxo {
        txid: [0xFFu8; 32],
        vout: 0,
        amount_atomic: 90_100 * 100_000_000,
        // A script that is NOT the root vault's own.
        script_pubkey_hex: "deadbeef".to_string(),
    };
    ledger
        .sync_vault_utxos(&[(utxo.clone(), 20, "deadbeef".to_string())], 1, 0)
        .unwrap();

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        utxo.txid,
        utxo.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SplitSigningError::SourceNotRootVault { .. }));
}

#[tokio::test]
async fn refuses_to_split_the_same_outpoint_twice() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);

    let plan = split::plan_split(&source, &vault, CHUNK_TARGET, FEE_RATE).unwrap();
    let tx = split::build_unsigned_split_tx(&plan);
    ledger
        .record_vault_utxo_split_built(
            &plan,
            CHUNK_TARGET,
            &crate::goldcoin::hex::encode(&tx.serialize()),
            "first split",
            0,
        )
        .unwrap();

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        source.txid,
        source.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, SplitSigningError::AlreadySplit { .. }));
}

#[tokio::test]
async fn signing_is_deterministic_across_independent_signers() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);

    let ledger_source = LedgerSplitSource { ledger: &ledger };
    let (plan_a, tx_a, _) = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        source.txid,
        source.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    let (plan_b, tx_b, _) = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &ledger_source,
        source.txid,
        source.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap();
    assert_eq!(plan_a, plan_b);
    assert_eq!(tx_a.serialize(), tx_b.serialize());
}
