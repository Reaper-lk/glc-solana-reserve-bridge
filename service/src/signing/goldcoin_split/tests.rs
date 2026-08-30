use super::*;
use crate::goldcoin::address::Network;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::multisig;
use crate::ledger::{Ledger, LedgerError};
use crate::signing::goldcoin_vault::DevVaultSigner;

const TEST_SIGNER_TIMEOUT: Duration = Duration::from_secs(5);
const TEST_THRESHOLD: usize = 2;
const CHUNK_TARGET: u64 = 5_000 * 100_000_000; // the canonical 5,000 GLC chunk
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

/// The claim step every production split now performs before any signer
/// round-trip (`Ledger::record_vault_utxo_split_built`): plans, builds
/// the unsigned transaction, persists the `Built` row.
fn claim_split(
    ledger: &mut Ledger,
    vault: &MultisigVault,
    source: &VaultUtxo,
) -> crate::goldcoin::split::SplitPlan {
    let plan = split::plan_split(source, vault, CHUNK_TARGET, FEE_RATE).unwrap();
    let unsigned_hex =
        crate::goldcoin::hex::encode(&split::build_unsigned_split_tx(&plan).serialize());
    ledger
        .record_vault_utxo_split_built(&plan, CHUNK_TARGET, &unsigned_hex, "test split", 0)
        .unwrap();
    plan
}

#[tokio::test]
async fn splits_a_large_mature_utxo_into_smaller_ones() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);
    claim_split(&mut ledger, &vault, &source);

    let sign_source = RecoverySplitSource { ledger: &ledger };
    let (plan, mut tx, partials) = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &sign_source,
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
    // Reserve already below the protected floor: even the split's own
    // network fee is value the reserve cannot afford to lose. (Since the
    // 2026-08-30 solvency-invariant alignment, the check is
    // `balance - fee >= floor` — a split's chunks stay vault-owned,
    // ledger-tracked value, so a merely-illiquid-but-solvent reserve may
    // now be restructured; an actually-insolvent one still may not.)
    // Every SIGNER refuses independently, even though the claim row
    // already exists — an automatic trigger never bypasses this.
    configure_reserve(&mut ledger, 19_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);
    claim_split(&mut ledger, &vault, &source);

    let sign_source = RecoverySplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &sign_source,
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
async fn claim_refuses_a_source_utxo_that_is_not_available() {
    let (vault, _) = vault_and_signers();
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

    let plan = split::plan_split(&utxo, &vault, CHUNK_TARGET, FEE_RATE).unwrap();
    let err = ledger
        .record_vault_utxo_split_built(&plan, CHUNK_TARGET, "deadbeef", "test split", 0)
        .unwrap_err();
    assert!(matches!(
        err,
        LedgerError::VaultUtxoNotSplittable { state, .. } if state == "Unconfirmed"
    ));
}

#[tokio::test]
async fn signers_refuse_when_the_source_stops_being_available_after_the_claim() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);
    claim_split(&mut ledger, &vault, &source);

    // The source vanishes from the chain view between the claim and the
    // signer round-trip (spent externally / reorged away).
    ledger.sync_vault_utxos(&[], 1, 1).unwrap();

    let sign_source = RecoverySplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &sign_source,
        source.txid,
        source.vout,
        CHUNK_TARGET,
        FEE_RATE,
        TEST_THRESHOLD,
        TEST_SIGNER_TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        SplitSigningError::SourceNotAvailable { state, .. } if state == "Spent"
    ));
}

#[tokio::test]
async fn claim_refuses_an_unknown_source_outpoint() {
    let (vault, _) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);

    let phantom = VaultUtxo {
        txid: [0xEEu8; 32],
        vout: 0,
        amount_atomic: 90_100 * 100_000_000,
        script_pubkey_hex: vault.script_pubkey_hex(),
    };
    let plan = split::plan_split(&phantom, &vault, CHUNK_TARGET, FEE_RATE).unwrap();
    let err = ledger
        .record_vault_utxo_split_built(&plan, CHUNK_TARGET, "deadbeef", "test split", 0)
        .unwrap_err();
    assert!(matches!(err, LedgerError::VaultUtxoNotFound { .. }));
}

#[tokio::test]
async fn signers_refuse_a_utxo_that_does_not_belong_to_the_root_vault() {
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
    // The claim itself is script-agnostic (the CLI and the shaping tick
    // both filter to the root script before claiming), so the signer-side
    // refusal below is the defense-in-depth boundary being pinned here.
    let plan = split::plan_split(&utxo, &vault, CHUNK_TARGET, FEE_RATE).unwrap();
    let unsigned_hex =
        crate::goldcoin::hex::encode(&split::build_unsigned_split_tx(&plan).serialize());
    ledger
        .record_vault_utxo_split_built(&plan, CHUNK_TARGET, &unsigned_hex, "test split", 0)
        .unwrap();

    let sign_source = RecoverySplitSource { ledger: &ledger };
    let err = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &sign_source,
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
async fn claim_refuses_to_split_the_same_outpoint_twice() {
    let (vault, _) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);
    let plan = claim_split(&mut ledger, &vault, &source);

    let err = ledger
        .record_vault_utxo_split_built(&plan, CHUNK_TARGET, "deadbeef", "second claim", 0)
        .unwrap_err();
    assert!(matches!(err, LedgerError::VaultUtxoAlreadySplit { .. }));
}

#[tokio::test]
async fn an_abandoned_claim_releases_the_outpoint_for_a_fresh_split() {
    let (vault, _) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);
    claim_split(&mut ledger, &vault, &source);
    let snapshot = ledger
        .get_vault_utxo_split(source.txid, source.vout)
        .unwrap()
        .unwrap();
    ledger
        .abandon_vault_utxo_split(snapshot.id, "test abandonment", 1)
        .unwrap();

    // The audit row survives with its reason; the outpoint is claimable
    // again — the partial unique index no longer counts the dead row.
    assert!(ledger
        .get_vault_utxo_split(source.txid, source.vout)
        .unwrap()
        .is_none());
    claim_split(&mut ledger, &vault, &source);
}

#[tokio::test]
async fn signing_is_deterministic_across_independent_signers() {
    let (vault, vault_signers) = vault_and_signers();
    let mut ledger = Ledger::open_in_memory().unwrap();
    configure_reserve(&mut ledger, 200_000 * 100_000_000, 20_000 * 100_000_000);
    let source = sync_root_utxo(&mut ledger, &vault, 90_100 * 100_000_000);
    claim_split(&mut ledger, &vault, &source);

    let sign_source = RecoverySplitSource { ledger: &ledger };
    let (plan_a, tx_a, _) = independently_sign_split_all_signers(
        &vault_signers,
        &vault,
        &sign_source,
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
        &sign_source,
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
