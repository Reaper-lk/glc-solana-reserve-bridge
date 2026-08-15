use std::sync::Mutex;

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use super::*;

struct MockRpc {
    status: Mutex<Option<Result<(), String>>>,
    blockhash_valid: Mutex<bool>,
}

impl SolanaRpc for MockRpc {
    async fn get_account(&self, _: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_multiple_accounts(
        &self,
        _: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!()
    }
    async fn send_transaction(
        &self,
        _: &solana_sdk::transaction::Transaction,
    ) -> Result<Signature, SolanaRpcError> {
        unimplemented!()
    }
    async fn get_signature_status(
        &self,
        _: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        Ok(self.status.lock().unwrap().clone())
    }
    async fn is_blockhash_valid(&self, _: &Hash) -> Result<bool, SolanaRpcError> {
        Ok(*self.blockhash_valid.lock().unwrap())
    }
}

#[tokio::test]
async fn returns_ok_once_status_reports_success() {
    let rpc = MockRpc {
        status: Mutex::new(Some(Ok(()))),
        blockhash_valid: Mutex::new(true),
    };
    let result = confirm_transaction(
        &rpc,
        &Signature::default(),
        &Hash::default(),
        ConfirmPolicy {
            deadline: std::time::Duration::from_millis(200),
            poll_interval: std::time::Duration::from_millis(10),
        },
    )
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn reports_rejection_distinct_from_expiry() {
    let rpc = MockRpc {
        status: Mutex::new(Some(Err("custom program error: 0x1".into()))),
        blockhash_valid: Mutex::new(true),
    };
    let result = confirm_transaction(
        &rpc,
        &Signature::default(),
        &Hash::default(),
        ConfirmPolicy {
            deadline: std::time::Duration::from_millis(200),
            poll_interval: std::time::Duration::from_millis(10),
        },
    )
    .await;
    assert!(matches!(result, Err(ConfirmFailure::Rejected { .. })));
}

#[tokio::test]
async fn reports_expiry_when_blockhash_is_no_longer_valid_and_no_status_exists() {
    let rpc = MockRpc {
        status: Mutex::new(None),
        blockhash_valid: Mutex::new(false),
    };
    let result = confirm_transaction(
        &rpc,
        &Signature::default(),
        &Hash::default(),
        ConfirmPolicy {
            deadline: std::time::Duration::from_millis(200),
            poll_interval: std::time::Duration::from_millis(10),
        },
    )
    .await;
    assert!(matches!(result, Err(ConfirmFailure::Expired { .. })));
}

#[tokio::test]
async fn never_reports_expiry_if_status_arrives_in_the_final_recheck() {
    // Simulates the race the module docs describe: blockhash reads as
    // invalid, but the status read immediately after (the "ask once more")
    // finds a genuine success — must never be reported as Expired.
    let rpc = MockRpc {
        status: Mutex::new(Some(Ok(()))),
        blockhash_valid: Mutex::new(false),
    };
    let result = confirm_transaction(
        &rpc,
        &Signature::default(),
        &Hash::default(),
        ConfirmPolicy {
            deadline: std::time::Duration::from_millis(200),
            poll_interval: std::time::Duration::from_millis(10),
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "a completed action must never be reported as expired"
    );
}

#[tokio::test]
async fn times_out_when_neither_confirmed_nor_expired() {
    let rpc = MockRpc {
        status: Mutex::new(None),
        blockhash_valid: Mutex::new(true),
    };
    let result = confirm_transaction(
        &rpc,
        &Signature::default(),
        &Hash::default(),
        ConfirmPolicy {
            deadline: std::time::Duration::from_millis(30),
            poll_interval: std::time::Duration::from_millis(10),
        },
    )
    .await;
    assert!(
        matches!(result, Err(ConfirmFailure::TimedOut { .. })),
        "unknown outcome must be reported as UNKNOWN, never assumed either way"
    );
}
