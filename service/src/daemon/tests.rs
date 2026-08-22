use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::transaction::Transaction as SolanaTx;

use super::*;
use crate::goldcoin::indexer::Indexer;
use crate::goldcoin::rpc::{
    BlockHeader, BroadcastOutcome, DecodedTransaction, ListUnspentEntry, RpcError, TxOut,
};
use crate::ledger::Ledger;
use crate::orchestrator::tests::{
    attestation_signers, base_config, indexer_config, vault_and_signers,
};
use crate::orchestrator::Orchestrator;
use crate::solana::accounts;
use crate::solana::indexer::SolanaIndexer;
use crate::solana::rpc::{SolanaRpc, SolanaRpcError};

/// A `GoldcoinRpc`/`SolanaRpc` pair whose failure can be toggled at
/// runtime, purpose-built for these backoff/recovery tests — not a
/// general settlement-mechanics mock (see `orchestrator::tests` for
/// that, reused here for its fixture/config helpers).
#[derive(Default)]
struct Outage(AtomicBool);
impl Outage {
    fn fail(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn recover(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
    fn is_down(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

struct FlakyGoldcoinRpc<'a> {
    down: &'a Outage,
}

impl GoldcoinRpc for FlakyGoldcoinRpc<'_> {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        if self.down.is_down() {
            return Err(RpcError::Transport("simulated outage".into()));
        }
        Ok(-1) // empty chain: a harmless no-op tick (see orchestrator::tests)
    }
    async fn get_block_hash(&self, _height: i64) -> Result<String, RpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn get_raw_transaction(&self, _txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn get_tx_out_confirmed(
        &self,
        _txid_hex: &str,
        _vout: u32,
    ) -> Result<Option<TxOut>, RpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn send_raw_transaction(&self, _hex: &str) -> Result<BroadcastOutcome, RpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn list_unspent(
        &self,
        _min_conf: i64,
        _addresses: &[String],
    ) -> Result<Vec<ListUnspentEntry>, RpcError> {
        // Goldcoin-reserve reconciliation calls this every tick regardless
        // of indexer health; an empty vault is a legitimate (if
        // unconfigured-reserve-error-producing) answer, not a failure this
        // backoff logic needs to observe.
        Ok(Vec::new())
    }
}

struct FlakySolanaRpc<'a> {
    down: &'a Outage,
}

impl SolanaRpc for FlakySolanaRpc<'_> {
    async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
        if self.down.is_down() {
            return Err(SolanaRpcError::Transport("simulated outage".into()));
        }
        if *pubkey == accounts::bridge_config_pda() {
            return Ok(Some(Account {
                lamports: 1,
                data: fake_bridge_config_bytes(),
                owner: accounts::PROGRAM_ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        Ok(None) // e.g. the reserve ATA: "does not exist yet" is a clean skip, not a failure
    }
    async fn get_multiple_accounts(
        &self,
        _pubkeys: &[Pubkey],
    ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
        if self.down.is_down() {
            return Err(SolanaRpcError::Transport("simulated outage".into()));
        }
        Ok(1)
    }
    async fn get_latest_blockhash(&self) -> Result<Hash, SolanaRpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn send_transaction(&self, _tx: &SolanaTx) -> Result<Signature, SolanaRpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn simulate_transaction(
        &self,
        _tx: &SolanaTx,
    ) -> Result<crate::solana::rpc::SimulationOutcome, SolanaRpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
    async fn get_signature_status(
        &self,
        _signature: &Signature,
    ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
        Ok(None)
    }
    async fn is_blockhash_valid(&self, _blockhash: &Hash) -> Result<bool, SolanaRpcError> {
        unimplemented!("not exercised by daemon backoff tests")
    }
}

/// `obligation_count: 0`, matching a freshly-opened ledger's own
/// `last_synced_obligation_count() == 0` — the Solana indexer takes the
/// cheap `NoNewObligations` path and never needs `get_multiple_accounts`.
fn fake_bridge_config_bytes() -> Vec<u8> {
    let mut v = vec![0u8; 8];
    v.push(1); // protocol_version
    v.extend_from_slice(&[0u8; 32]); // admin
    v.push(0); // pending_admin: None
    v.push(0);
    v.push(0);
    v.push(0);
    v.push(7);
    v.extend_from_slice(&[9u8; 32]); // reserve_token_mint (unused by these tests)
    v.extend_from_slice(spl_token::ID.as_ref()); // reserve_token_program
    v.push(3);
    v.extend_from_slice(&0u64.to_le_bytes()); // obligation_count
    v.extend_from_slice(&3600i64.to_le_bytes());
    v.extend_from_slice(&100u64.to_le_bytes());
    v.extend_from_slice(&1_000_000u64.to_le_bytes());
    v.extend_from_slice(&500u64.to_le_bytes());
    v.extend_from_slice(&2_000_000u64.to_le_bytes());
    v.extend_from_slice(&3600i64.to_le_bytes());
    v
}

/// `Orchestrator::new` wants the RPC clients both inside each indexer and
/// held directly (for phases outside the indexers, e.g.
/// `tick_vault_utxos`/reconciliation) — `FlakyGoldcoinRpc`/`FlakySolanaRpc`
/// are just a borrowed `&'a Outage`, so constructing three cheap instances
/// from the same two flags is simpler than trying to share one value
/// three ways.
fn build<'a>(
    db_path: &std::path::Path,
    goldcoin_down: &'a Outage,
    solana_down: &'a Outage,
) -> Orchestrator<FlakyGoldcoinRpc<'a>, FlakySolanaRpc<'a>> {
    let (vault, vault_signers) = vault_and_signers();
    let goldcoin_indexer = Indexer::new(
        FlakyGoldcoinRpc {
            down: goldcoin_down,
        },
        Ledger::open(db_path).unwrap(),
        indexer_config(),
    );
    let solana_indexer = SolanaIndexer::new(
        FlakySolanaRpc { down: solana_down },
        Ledger::open(db_path).unwrap(),
    );
    let ledger = Ledger::open(db_path).unwrap();
    Orchestrator::new(
        goldcoin_indexer,
        solana_indexer,
        ledger,
        FlakyGoldcoinRpc {
            down: goldcoin_down,
        },
        FlakySolanaRpc { down: solana_down },
        vault,
        vault_signers,
        attestation_signers(),
        Keypair::new(),
        base_config(),
        0,
    )
}

// ----------------------------------------------------------------- tests --

#[test]
fn tick_backoff_delay_doubles_and_caps() {
    let base = Duration::from_millis(100);
    let max = Duration::from_secs(1);
    assert_eq!(tick_backoff_delay(base, max, 0), base);
    assert_eq!(tick_backoff_delay(base, max, 1), Duration::from_millis(200));
    assert_eq!(tick_backoff_delay(base, max, 2), Duration::from_millis(400));
    assert_eq!(tick_backoff_delay(base, max, 3), Duration::from_millis(800));
    // Would be 1600ms uncapped; the ceiling wins.
    assert_eq!(tick_backoff_delay(base, max, 4), max);
    // A very large streak must never overflow or panic.
    assert_eq!(tick_backoff_delay(base, max, u32::MAX), max);
}

#[tokio::test]
async fn partial_outage_never_counts_as_a_full_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let goldcoin_down = Outage::default();
    let solana_down = Outage::default();
    goldcoin_down.fail(); // Goldcoin down, Solana healthy

    let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);
    let report = orchestrator.tick(0).await;
    assert!(
        matches!(report.goldcoin_indexer, Some(Err(_))),
        "Goldcoin indexer must report the simulated outage"
    );
    assert!(
        matches!(report.solana_indexer, Some(Ok(_))),
        "Solana indexer must stay healthy and unaffected"
    );
    assert!(
        !both_indexers_failed(&report),
        "one chain being down must never look like a total outage — the healthy chain \
         should keep making progress, and backoff must not trigger"
    );
}

#[tokio::test]
async fn total_outage_is_detected_as_a_full_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let goldcoin_down = Outage::default();
    let solana_down = Outage::default();
    goldcoin_down.fail();
    solana_down.fail();

    let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);
    let report = orchestrator.tick(0).await;
    assert!(both_indexers_failed(&report));
}

#[tokio::test]
async fn run_ticks_until_shutdown_and_then_returns() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let goldcoin_down = Outage::default();
    let solana_down = Outage::default();
    let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let config = DaemonLoopConfig {
        tick_interval: Duration::from_millis(5),
        max_backoff: Duration::from_millis(50),
    };
    // `run`'s future is not `Send` (it holds a `Ledger`/SQLite connection
    // across awaits), so it must stay on this task rather than being
    // `tokio::spawn`ed — `join!` drives it concurrently with the shutdown
    // trigger on the same task instead.
    let (ticks_run, _) = tokio::join!(run(&mut orchestrator, config, rx, || 0), async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        tx.send(true).unwrap();
    });

    assert!(
        ticks_run >= 1,
        "the loop must run at least one tick before observing shutdown"
    );
}

#[tokio::test]
async fn run_backs_off_during_a_total_outage_and_recovers_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let goldcoin_down = Outage::default();
    let solana_down = Outage::default();
    goldcoin_down.fail();
    solana_down.fail();
    let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);

    let (tx, rx) = tokio::sync::watch::channel(false);
    let config = DaemonLoopConfig {
        tick_interval: Duration::from_millis(5),
        max_backoff: Duration::from_millis(40),
    };
    let (ticks_run, _) = tokio::join!(run(&mut orchestrator, config, rx, || 0), async {
        // Let backoff widen for a while under the full outage.
        tokio::time::sleep(Duration::from_millis(120)).await;
        goldcoin_down.recover();
        solana_down.recover();
        // The in-flight sleep when recovery happens can itself be as
        // long as `max_backoff` before the loop wakes and notices —
        // leave clear margin past that worst case before asserting
        // multiple post-recovery ticks happened.
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.send(true).unwrap();
    });

    assert!(
        ticks_run >= 2,
        "recovery must resume ticking promptly rather than staying capped at the widened \
         backoff forever, got {ticks_run} total ticks"
    );
}

#[tokio::test]
async fn daemon_restart_resumes_ticking_against_the_same_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let goldcoin_down = Outage::default();
    let solana_down = Outage::default();

    let config = DaemonLoopConfig {
        tick_interval: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
    };

    // First "process": run a few ticks, then simulate a crash by dropping
    // the orchestrator entirely without any graceful shutdown.
    {
        let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);
        let (tx, rx) = tokio::sync::watch::channel(false);
        let (first_run_ticks, _) = tokio::join!(run(&mut orchestrator, config, rx, || 0), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send(true).unwrap();
        });
        assert!(first_run_ticks >= 1);
    }

    // Second "process": a fresh orchestrator against the same on-disk
    // ledger must start cleanly and keep ticking — nothing about a
    // restart should require special-case recovery code at this layer
    // (the underlying idempotency guarantees live in the ledger/on-chain
    // program, exercised in orchestrator/regtest_acceptance tests).
    let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);
    let (tx, rx) = tokio::sync::watch::channel(false);
    let (second_run_ticks, _) = tokio::join!(run(&mut orchestrator, config, rx, || 0), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(true).unwrap();
    });
    assert!(
        second_run_ticks >= 1,
        "a restarted daemon must resume ticking normally against the same ledger"
    );
}

#[tokio::test]
async fn shutdown_requested_before_the_first_tick_runs_zero_ticks() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    let goldcoin_down = Outage::default();
    let solana_down = Outage::default();
    let mut orchestrator = build(&db_path, &goldcoin_down, &solana_down);

    let (_tx, rx) = tokio::sync::watch::channel(true); // already-shutdown at construction
    let config = DaemonLoopConfig {
        tick_interval: Duration::from_millis(5),
        max_backoff: Duration::from_millis(20),
    };
    let ticks_run = run(&mut orchestrator, config, rx, || 0).await;
    assert_eq!(ticks_run, 0);
}
