//! Tests for the CLI -> daemon refund execution bridge.
//!
//! Two layers are covered here:
//!
//! 1. **The signing layer** — the daemon's 2-of-3 vault signing at, above
//!    and below threshold, plus refusal, timeout and bogus-signature
//!    handling.
//! 2. **A stub executor** used by the HTTP/authorization tests in
//!    `admin_api::tests` to prove the capability gate without needing a
//!    real signer or node.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::*;
use crate::goldcoin::coin::VaultUtxo;
use crate::goldcoin::payout::PayoutInputContext;
use crate::signing::goldcoin_vault::DevVaultSigner;
use crate::signing::signers::SignerError;

// ------------------------------------------------------------- signer layer --

fn vault_of(keys: Vec<[u8; 33]>) -> MultisigVault {
    MultisigVault::new(keys, 2, Network::Testnet).unwrap()
}

fn plan_for(vault: &MultisigVault) -> PayoutPlan {
    PayoutPlan {
        inputs: vec![VaultUtxo {
            txid: [0xAB; 32],
            vout: 0,
            amount_atomic: 5_000_000,
            script_pubkey_hex: vault.script_pubkey_hex(),
        }],
        input_contexts: vec![PayoutInputContext {
            vault: vault.clone(),
            funding_request_id: None,
        }],
        dest_p2pkh_hash: [0x5A; 20],
        payout_atomic: 4_000_000,
        change_outputs: vec![900_000],
        vault_script_pubkey: vault.script_pubkey(),
        fee_atomic: 100_000,
    }
}

struct RefusingSigner {
    pubkey: [u8; 33],
}
impl VaultSigner for RefusingSigner {
    fn public_key(&self) -> [u8; 33] {
        self.pubkey
    }
    fn sign_sighash<'a>(
        &'a self,
        _sighash: &'a [u8; 32],
    ) -> crate::signing::signers::BoxFut<'a, Result<Vec<u8>, SignerError>> {
        Box::pin(async {
            Err(SignerError::Rejected {
                identity: "refusing-signer".to_string(),
                detail: "this custody domain's policy refused the request".to_string(),
            })
        })
    }
}

/// Returns a well-formed signature over a DIFFERENT message — the shape a
/// buggy or malicious signer would produce.
struct BogusSigner {
    secret: libsecp256k1::SecretKey,
    pubkey: [u8; 33],
}
impl VaultSigner for BogusSigner {
    fn public_key(&self) -> [u8; 33] {
        self.pubkey
    }
    fn sign_sighash<'a>(
        &'a self,
        _sighash: &'a [u8; 32],
    ) -> crate::signing::signers::BoxFut<'a, Result<Vec<u8>, SignerError>> {
        let der = crate::goldcoin::multisig::sign_low_s(&[0x77u8; 32], &self.secret);
        Box::pin(async move { Ok(der) })
    }
}

struct SlowSigner {
    pubkey: [u8; 33],
}
impl VaultSigner for SlowSigner {
    fn public_key(&self) -> [u8; 33] {
        self.pubkey
    }
    fn sign_sighash<'a>(
        &'a self,
        _sighash: &'a [u8; 32],
    ) -> crate::signing::signers::BoxFut<'a, Result<Vec<u8>, SignerError>> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            unreachable!("the timeout must fire first")
        })
    }
}

#[tokio::test]
async fn two_of_three_signers_produce_an_assembled_transaction() {
    let a = DevVaultSigner::generate();
    let b = DevVaultSigner::generate();
    let c = DevVaultSigner::generate();
    let vault = vault_of(vec![a.public_key(), b.public_key(), c.public_key()]);
    let plan = plan_for(&vault);
    let tx = crate::goldcoin::payout::build_unsigned_tx(&plan);

    let signers: Vec<Box<dyn VaultSigner>> = vec![Box::new(a), Box::new(b), Box::new(c)];
    let signed = sign_with_vault(&signers, 2, &plan, tx, Duration::from_secs(5))
        .await
        .expect("2-of-3 must succeed");

    assert!(!signed.inputs[0].script_sig.is_empty());
    // Signing must not be able to change where the money goes.
    assert_eq!(signed.outputs[0].value_atomic, plan.payout_atomic);
    assert_eq!(signed.outputs.len(), 2);
}

#[tokio::test]
async fn one_of_three_is_below_threshold_and_signs_nothing() {
    let a = DevVaultSigner::generate();
    let b = DevVaultSigner::generate();
    let c = DevVaultSigner::generate();
    let vault = vault_of(vec![a.public_key(), b.public_key(), c.public_key()]);
    let plan = plan_for(&vault);
    let tx = crate::goldcoin::payout::build_unsigned_tx(&plan);

    // Only one signer client available for a 2-of-3 vault.
    let signers: Vec<Box<dyn VaultSigner>> = vec![Box::new(a)];
    let err = sign_with_vault(&signers, 2, &plan, tx, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        err.contains("not satisfiable"),
        "the threshold must never be silently reduced, got: {err}"
    );
}

#[tokio::test]
async fn a_refusing_signer_aborts_the_whole_refund() {
    let a = DevVaultSigner::generate();
    let b = DevVaultSigner::generate();
    let c = DevVaultSigner::generate();
    let vault = vault_of(vec![a.public_key(), b.public_key(), c.public_key()]);
    let plan = plan_for(&vault);
    let tx = crate::goldcoin::payout::build_unsigned_tx(&plan);

    let refusing = RefusingSigner {
        pubkey: b.public_key(),
    };
    let signers: Vec<Box<dyn VaultSigner>> = vec![Box::new(a), Box::new(refusing), Box::new(c)];
    let err = sign_with_vault(&signers, 2, &plan, tx, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(err.contains("refused"), "got: {err}");
}

#[tokio::test]
async fn a_signer_returning_a_wrong_signature_is_caught_before_assembly() {
    let a = DevVaultSigner::generate();
    let b = DevVaultSigner::generate();
    let c = DevVaultSigner::generate();
    let vault = vault_of(vec![a.public_key(), b.public_key(), c.public_key()]);
    let plan = plan_for(&vault);
    let tx = crate::goldcoin::payout::build_unsigned_tx(&plan);

    let bogus = BogusSigner {
        secret: b.secret_key,
        pubkey: b.pubkey,
    };
    let signers: Vec<Box<dyn VaultSigner>> = vec![Box::new(a), Box::new(bogus), Box::new(c)];
    let err = sign_with_vault(&signers, 2, &plan, tx, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(
        err.contains("does not verify"),
        "a wrong signature must never reach assembly, got: {err}"
    );
}

#[tokio::test]
async fn a_hanging_signer_times_out_rather_than_stalling_the_refund() {
    let a = DevVaultSigner::generate();
    let b = DevVaultSigner::generate();
    let c = DevVaultSigner::generate();
    let vault = vault_of(vec![a.public_key(), b.public_key(), c.public_key()]);
    let plan = plan_for(&vault);
    let tx = crate::goldcoin::payout::build_unsigned_tx(&plan);

    let slow = SlowSigner {
        pubkey: b.public_key(),
    };
    let signers: Vec<Box<dyn VaultSigner>> = vec![Box::new(a), Box::new(slow), Box::new(c)];
    let err = sign_with_vault(&signers, 2, &plan, tx, Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(err.contains("timed out"), "got: {err}");
}

// -------------------------------------------------------------- stub executor --

/// Records what it was asked to do; touches no chain and no signer. Used
/// by the HTTP authorization tests, which are about the capability gate
/// rather than the refund itself.
pub(crate) struct RecordingExecutor {
    pub(crate) calls: Mutex<Vec<(i64, String, String)>>,
    pub(crate) count: AtomicUsize,
}

impl RecordingExecutor {
    pub(crate) fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            count: AtomicUsize::new(0),
        }
    }
    pub(crate) fn call_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl GlcRefundExecutor for RecordingExecutor {
    fn execute(
        &self,
        request_id: i64,
        note: String,
        actor: String,
    ) -> BoxFut<'_, Result<GlcRefundExecuteView, AdminError>> {
        self.calls
            .lock()
            .unwrap()
            .push((request_id, note.clone(), actor.clone()));
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(GlcRefundExecuteView {
                request_id,
                action: GlcRefundAction::Broadcast,
                lifecycle_state: "Broadcast".to_string(),
                request_state: "RefundBroadcast".to_string(),
                source_txid: "aa".repeat(32),
                source_vout: 1,
                observed_amount_atomic: 2_905_000_000_000,
                observed_amount_glc: "29050.00000000".to_string(),
                refund_destination: "mokhM9inegFGeyHpj52nQj8fNwH6eoBxT6".to_string(),
                refund_principal_atomic: 2_905_000_000_000,
                refund_principal_glc: "29050.00000000".to_string(),
                fee_atomic: 6_760,
                fee_glc: "0.00006760".to_string(),
                txid: Some("bb".repeat(32)),
                confirmations: 0,
                checks: vec![GlcRefundCheckView {
                    name: "example".to_string(),
                    passed: true,
                    detail: String::new(),
                }],
                note,
                actor,
            })
        })
    }
}
