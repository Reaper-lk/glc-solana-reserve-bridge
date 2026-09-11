//! Phase H real-node acceptance for the two Solana<->Robinhood routes:
//! `SolToRhn` and `RhnToSol` driven end to end against a real
//! `solana-test-validator` (this repository's own compiled program baked
//! into genesis) and a real `anvil` (this repository's own compiled
//! `GlcRobinhoodBridge` deployed with throwaway signers) — no mock on
//! either chain under test.
//!
//! The Goldcoin leg is not on either route's path, so the orchestrator
//! is handed an inert `GoldcoinRpc` that reports an empty chain; nothing
//! it touches is exercised or asserted here (the Goldcoin<->Solana
//! routes have their own real-node rehearsal in `regtest_acceptance.rs`).
//!
//! Every key, database, node datadir and contract here is created fresh
//! and thrown away. Nothing contacts a non-loopback address; no
//! production RPC, key, contract, route flag or fund is involved.
//!
//! Skipped (never failed) unless the program is built
//! (`GLC_RESERVE_BRIDGE_SO` or `../target/deploy/glc_reserve_bridge.so`),
//! the contracts are built (`../contracts/out/`), and
//! `solana-test-validator`, `anvil` and `forge` are on `PATH`.
//!
//! One session, numbered sections, so the expensive node startup happens
//! once:
//!
//! 1. `SolToRhn` — Solana deposit → `executePayout` on anvil →
//!    `record_goldcoin_completion` on Solana → `Settled`, with a
//!    crash/restart between the payout finalizing and the completion.
//! 2. Reconciliation of BOTH reserves inside the `DestinationConfirmed`
//!    window and after `Settled`.
//! 3. `RhnToSol` — anvil deposit on route `0x04` → `release_from_reserve`
//!    → `executeSettlement` → `Settled`, with a crash/restart while the
//!    release is `DestinationSubmitted`.
//! 4. Refunds: an undeliverable `SolToRhn` destination refunded by
//!    `refund_withdraw` on Solana; an undeliverable `RhnToSol` destination
//!    refunded by `executeRefund` on anvil.
//! 5. Contract-disabled route: `routeEnabled(0x03)` flipped off by real
//!    governance; a `SolToRhn` request is refused before broadcast and
//!    the submitter's nonce does not move.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::amount_conversion::{
    compute_fee_at_bps, CanonicalAtomic, SolanaAtomic,
};
use glc_reserve_bridge_service::chains::ChainRegistry;
use glc_reserve_bridge_service::evm::secp;
use glc_reserve_bridge_service::evm::{
    EvmAddress, EvmChainId, EvmSecretKey, EvmSignature, TxEnvelope,
};
use glc_reserve_bridge_service::goldcoin::address::Network;
use glc_reserve_bridge_service::goldcoin::indexer::{GoldcoinRpc, Indexer, IndexerConfig};
use glc_reserve_bridge_service::goldcoin::rpc::{
    BlockHeader, BroadcastOutcome, DecodedTransaction, ListUnspentEntry, RpcError, TxOut,
};
use glc_reserve_bridge_service::goldcoin::vault::MultisigVault;
use glc_reserve_bridge_service::ledger::{
    Direction, Ledger, RequestState, ReserveDirection, RobinhoodFinality, RobinhoodTxKind,
    RobinhoodTxState,
};
use glc_reserve_bridge_service::orchestrator::{CrossRouteFold, Orchestrator, OrchestratorConfig};
use glc_reserve_bridge_service::reconciliation::{self, Classification};
use glc_reserve_bridge_service::robinhood::auth::BridgeDomain;
use glc_reserve_bridge_service::robinhood::calls::{BridgeReader, TokenReader};
use glc_reserve_bridge_service::robinhood::governance::{GovernanceAuth, GovernancePayload};
use glc_reserve_bridge_service::robinhood::governance_session::{
    self, GovernanceQuorumSigner, ReceiptWait,
};
use glc_reserve_bridge_service::robinhood::rpc::{
    EvmBlockTag, EvmRpcClient, EvmRpcConfig, EvmSubmitRpc,
};
use glc_reserve_bridge_service::robinhood::{
    self, preflight, DevEvmAuthSigner, EvmAuthSigner, ReserveReconciler, ReserveTickOutcome,
    RobinhoodHealth, RobinhoodIndexer, RobinhoodIndexerConfig, RobinhoodSettlementConfig,
    SettlementReport, Settler, Submitter,
};
use glc_reserve_bridge_service::routes::{Route, RouteGate, RoutesConfig};
use glc_reserve_bridge_service::signing::attestation::DevAttestationSigner;
use glc_reserve_bridge_service::signing::goldcoin_vault::DevVaultSigner;
use glc_reserve_bridge_service::signing::signers::{AttestationSigner, VaultSigner};
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::confirm::ConfirmPolicy;
use glc_reserve_bridge_service::solana::indexer::SolanaIndexer;
use glc_reserve_bridge_service::solana::instructions;
use glc_reserve_bridge_service::solana::refund::{self, RefundExecuteOutcome};
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

use support::LocalValidator;

// ------------------------------------------------------------ constants --

/// The canonical Solana GLC mint's real decimals (docs/18).
const SOLANA_GLC_DECIMALS: u8 = 6;
/// Both cross routes priced at 3% for this rehearsal.
const CROSS_ROUTE_FEE_BPS: u64 = 300;
/// anvil's default chain id.
const ANVIL_CHAIN_ID: u64 = 31337;
/// The contract's protocol-namespace chain ids — the same three the
/// in-tree KMS signer serves (`signing::evm_kms::config`).
const PROTOCOL_GOLDCOIN: u64 = 1001;
const PROTOCOL_ROBINHOOD: u64 = 2001;
const PROTOCOL_SOLANA: u64 = 3001;
/// anvil's first pre-funded account: deployer and depositor.
const ANVIL_PK0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const ANVIL_ADDR0: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
const ONE_GLC_18DP: u128 = 1_000_000_000_000_000_000;
/// Depth at which both the indexer and the settler treat a Robinhood
/// transaction as final; anvil mines one block per second.
const RHN_CONFIRMATION_DEPTH: u64 = 2;
/// The bridge's ERC-20 reserve seed: 1,000,000 GLC.
const RHN_RESERVE_SEED_GLC: u128 = 1_000_000;
/// The Solana reserve vault's seed, in mint units: 100,000 GLC.
const SOL_RESERVE_SEED: u64 = 100_000_000_000;

// ------------------------------------------------------------- prereqs --

struct Prereqs {
    so: PathBuf,
    contracts_dir: PathBuf,
}

fn tool_available(tool: &str, arg: &str) -> bool {
    Command::new(tool)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn prereqs() -> Option<Prereqs> {
    let so = support::program_so_path()?;
    let contracts_dir = PathBuf::from("../contracts");
    let artifact = contracts_dir.join("out/GlcRobinhoodBridge.sol/GlcRobinhoodBridge.json");
    if !artifact.exists() {
        eprintln!(
            "skipping: {} not built (run `forge build` in contracts/)",
            artifact.display()
        );
        return None;
    }
    if !support::solana_test_validator_available() {
        eprintln!("skipping: solana-test-validator not on PATH");
        return None;
    }
    if !tool_available("anvil", "--version") || !tool_available("forge", "--version") {
        eprintln!("skipping: anvil/forge not on PATH");
        return None;
    }
    Some(Prereqs { so, contracts_dir })
}

fn hex32(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ------------------------------------------------------ inert Goldcoin --

/// A Goldcoin node with an empty chain and an empty wallet. Neither cross
/// route touches Goldcoin; the orchestrator's Goldcoin phases become
/// no-ops against it and reconcile a zero reserve against a zero balance.
struct InertGoldcoinRpc;

impl GoldcoinRpc for InertGoldcoinRpc {
    async fn get_block_count(&self) -> Result<i64, RpcError> {
        Ok(-1)
    }
    async fn get_block_hash(&self, _height: i64) -> Result<String, RpcError> {
        Err(RpcError::Method {
            code: -8,
            message: "height out of range".into(),
        })
    }
    async fn get_block(&self, _hash: &str) -> Result<BlockHeader, RpcError> {
        Err(RpcError::Method {
            code: -5,
            message: "block not found".into(),
        })
    }
    async fn get_raw_transaction(&self, txid_hex: &str) -> Result<DecodedTransaction, RpcError> {
        Ok(DecodedTransaction {
            vin: Vec::new(),
            txid: txid_hex.to_string(),
            vout: Vec::new(),
            confirmations: None,
        })
    }
    async fn get_tx_out_confirmed(&self, _: &str, _: u32) -> Result<Option<TxOut>, RpcError> {
        Ok(None)
    }
    async fn send_raw_transaction(&self, _hex: &str) -> Result<BroadcastOutcome, RpcError> {
        Err(RpcError::Method {
            code: -1,
            message: "no Goldcoin node in this rehearsal".into(),
        })
    }
    async fn list_unspent(&self, _: i64, _: &[String]) -> Result<Vec<ListUnspentEntry>, RpcError> {
        Ok(Vec::new())
    }
}

// --------------------------------------------------------------- anvil --

struct Anvil {
    child: Child,
    url: String,
}

impl Anvil {
    fn start() -> Anvil {
        let port = support::free_port();
        let child = Command::new("anvil")
            .arg("--port")
            .arg(port.to_string())
            .arg("--chain-id")
            .arg(ANVIL_CHAIN_ID.to_string())
            .arg("--block-time")
            .arg("1")
            .arg("--silent")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn anvil");
        let url = format!("http://127.0.0.1:{port}");
        // Owned before the readiness probe, so `Drop` reaps the child on
        // every exit path, including the panic below.
        let anvil = Anvil { child, url };
        for _ in 0..100 {
            if Command::new("cast")
                .args(["chain-id", "--rpc-url", &anvil.url])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return anvil;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("anvil did not become ready");
    }

    fn cast(&self, args: &[&str]) -> String {
        let out = Command::new("cast")
            .args(args)
            .args(["--rpc-url", &self.url])
            .output()
            .expect("run cast");
        assert!(
            out.status.success(),
            "cast {:?} failed:\n{}\n{}",
            args,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// `cast send` from anvil's first pre-funded account.
    fn send_from_user(&self, to: &str, sig: &str, args: &[&str]) -> String {
        let mut full = vec!["send", to, sig];
        full.extend_from_slice(args);
        full.extend_from_slice(["--private-key", ANVIL_PK0, "--json"].as_slice());
        let out = self.cast(&full);
        let v: serde_json::Value = serde_json::from_str(&out).expect("cast send --json");
        assert_eq!(
            v["status"].as_str(),
            Some("0x1"),
            "transaction reverted: {out}"
        );
        v["transactionHash"].as_str().unwrap().to_string()
    }

    fn deploy(
        &self,
        contracts_dir: &Path,
        target: &str,
        constructor_args: &[String],
    ) -> EvmAddress {
        let mut cmd = Command::new("forge");
        cmd.current_dir(contracts_dir).args([
            "create",
            target,
            "--rpc-url",
            &self.url,
            "--private-key",
            ANVIL_PK0,
            "--broadcast",
            "--json",
        ]);
        if !constructor_args.is_empty() {
            cmd.arg("--constructor-args");
            cmd.args(constructor_args);
        }
        let out = cmd.output().expect("run forge create");
        assert!(
            out.status.success(),
            "forge create {target} failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        let json = &stdout[stdout.find('{').expect("forge create --json output")..];
        let v: serde_json::Value = serde_json::from_str(json).expect("forge json");
        v["deployedTo"]
            .as_str()
            .expect("deployedTo")
            .parse::<EvmAddress>()
            .expect("deployed address")
    }

    fn set_balance(&self, address: &EvmAddress, wei_hex: &str) {
        self.cast(&[
            "rpc",
            "anvil_setBalance",
            &address.to_checksum_string(),
            wei_hex,
        ]);
    }

    fn rpc_client(&self) -> EvmRpcClient {
        EvmRpcClient::new(&EvmRpcConfig {
            url: self.url.clone(),
            connect_timeout_ms: 5_000,
            read_timeout_ms: 10_000,
        })
        .expect("evm rpc client")
    }
}

impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------- EVM test signers --

fn evm_key(tag: u8) -> EvmSecretKey {
    let mut bytes = [0u8; 32];
    bytes[0] = 0x51;
    bytes[31] = tag;
    EvmSecretKey::from_bytes(&bytes).expect("a deterministic throwaway key")
}

/// A governance signer holding a throwaway key in memory — the same
/// shape the real custody domain has, deriving the digest from the auth
/// itself and never accepting a pre-built one.
struct LocalGovernanceSigner {
    key: EvmSecretKey,
}

impl GovernanceQuorumSigner for LocalGovernanceSigner {
    fn identity(&self) -> String {
        self.key.address().to_checksum_string()
    }
    fn sign_governance<'a>(
        &'a self,
        auth: &'a GovernanceAuth,
        domain: BridgeDomain,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<EvmSignature, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            let digest = auth.digest(domain).map_err(|e| e.to_string())?;
            Ok(secp::sign_digest(&self.key, &digest))
        })
    }
}

// ------------------------------------------------------------ the world --

/// Everything one session holds: both nodes, the deployment, the keys,
/// and the ledger path every component opens its own handle on.
struct World {
    _validator: LocalValidator,
    solana_url: String,
    anvil: Anvil,
    contracts_dir: PathBuf,
    db_path: PathBuf,
    _dir: tempfile::TempDir,

    // Solana
    admin: Keypair,
    solana_submitter: Keypair,
    attestation_keys: Vec<Keypair>,
    mint: Pubkey,
    token_program: Pubkey,
    next_obligation: u64,

    // EVM
    token: EvmAddress,
    bridge: EvmAddress,
    signer_tags: [u8; 3],
    submitter_tag: u8,
    indexer_cfg: RobinhoodIndexerConfig,
    settlement_cfg: RobinhoodSettlementConfig,
    verified: preflight::VerifiedDeployment,
    route_gate: Arc<RouteGate>,
}

impl World {
    fn solana_rpc(&self) -> RealSolanaRpc {
        RealSolanaRpc::new(self.solana_url.clone())
    }

    fn blocking(&self) -> solana_client::rpc_client::RpcClient {
        solana_client::rpc_client::RpcClient::new_with_commitment(
            self.solana_url.clone(),
            solana_sdk::commitment_config::CommitmentConfig::confirmed(),
        )
    }

    fn ledger(&self) -> Ledger {
        Ledger::open(&self.db_path).unwrap()
    }

    fn attestation_signers(&self) -> Vec<Box<dyn AttestationSigner>> {
        self.attestation_keys
            .iter()
            .map(|k| {
                Box::new(DevAttestationSigner {
                    keypair: k.insecure_clone(),
                }) as Box<dyn AttestationSigner>
            })
            .collect()
    }

    fn evm_auth_signers(&self) -> Vec<Box<dyn EvmAuthSigner>> {
        self.signer_tags
            .iter()
            .map(|t| Box::new(DevEvmAuthSigner::new(evm_key(*t))) as Box<dyn EvmAuthSigner>)
            .collect()
    }

    fn submitter(&self) -> Submitter {
        Submitter::from_key(evm_key(self.submitter_tag), &self.settlement_cfg).unwrap()
    }

    /// A fresh orchestrator against the same on-disk ledger and the same
    /// real nodes — what a process restart builds.
    fn orchestrator(&self) -> Orchestrator<InertGoldcoinRpc, RealSolanaRpc> {
        let vault_signers = vec![
            DevVaultSigner::generate(),
            DevVaultSigner::generate(),
            DevVaultSigner::generate(),
        ];
        let vault = MultisigVault::new(
            vault_signers.iter().map(|s| s.pubkey).collect(),
            2,
            Network::Testnet,
        )
        .unwrap();
        let goldcoin_indexer = Indexer::new(
            InertGoldcoinRpc,
            self.ledger(),
            IndexerConfig {
                vault_script_hex: vault.script_pubkey_hex(),
                confirmation_depth: 3,
                max_reorg_depth: 50,
                initial_checkpoint: None,
            },
        );
        let solana_indexer = SolanaIndexer::new(
            self.solana_rpc(),
            self.ledger(),
            glc_reserve_bridge_service::amount_conversion::BRIDGE_FEE_BPS,
        );
        Orchestrator::new(
            goldcoin_indexer,
            solana_indexer,
            self.ledger(),
            InertGoldcoinRpc,
            self.solana_rpc(),
            vault,
            vault_signers
                .into_iter()
                .map(|s| Box::new(s) as Box<dyn VaultSigner>)
                .collect(),
            self.attestation_signers(),
            self.solana_submitter.insecure_clone(),
            OrchestratorConfig {
                attestation_threshold: 2,
                vault_threshold: 2,
                required_goldcoin_confirmations: 3,
                fee_rate_per_kb: 100_000,
                dust_threshold: 1_000,
                max_inputs: 10,
                change_fanout_target_atomic: 2_500 * 100_000_000,
                change_fanout_max_outputs: 10,
                zero_conf_change_max_depth: 0,
                zero_conf_change_mode:
                    glc_reserve_bridge_service::goldcoin::payout::ZeroConfChangeMode::DepthLimited,
                zero_conf_change_recursive_chain_limit: 20,
                reconciliation_tolerance: 0,
                vault_min_confirmations: 1,
                goldcoin_network: Network::Testnet,
                signer_timeout: Duration::from_secs(5),
                max_auto_resumes_per_tick: 20,
                utxo_shaping_enabled: false,
                utxo_shaping_target_available_count: 15,
                utxo_shaping_min_source_atomic: 4 * 2_500 * 100_000_000,
                utxo_shaping_max_outputs_per_split: 25,
            },
            now_unix(),
        )
        .with_sol_to_rhn(CrossRouteFold {
            fee_bps: CROSS_ROUTE_FEE_BPS,
            route_gate: Arc::clone(&self.route_gate),
        })
        .with_rhn_to_sol(CrossRouteFold {
            fee_bps: CROSS_ROUTE_FEE_BPS,
            route_gate: Arc::clone(&self.route_gate),
        })
    }

    /// A fresh settler — the Robinhood settlement engine as the daemon
    /// builds it, against the real anvil RPC.
    fn settler(&self) -> Settler<EvmRpcClient> {
        Settler::new(
            self.anvil.rpc_client(),
            self.submitter(),
            self.evm_auth_signers(),
            self.verified.clone(),
            self.settlement_cfg.clone(),
            Duration::from_secs(5),
            Network::Testnet,
            3,
            CROSS_ROUTE_FEE_BPS,
        )
    }

    fn robinhood_indexer(&self) -> RobinhoodIndexer<EvmRpcClient> {
        RobinhoodIndexer::new(
            self.anvil.rpc_client(),
            self.ledger(),
            self.indexer_cfg.clone(),
            RobinhoodHealth::new(self.indexer_cfg.chain_id, &self.anvil.url, now_unix()),
        )
    }

    fn reserve_reconciler(&self) -> ReserveReconciler<EvmRpcClient> {
        ReserveReconciler::new(self.anvil.rpc_client(), self.indexer_cfg.clone(), 0)
    }

    fn reader(&self) -> BridgeReader {
        BridgeReader::new(self.bridge)
    }

    // ---- driving the engines, one phase at a time ----

    /// One Robinhood settlement-loop tick, phase by phase, exactly as
    /// `robinhood::daemon::run_settlement` orders them, gated per route
    /// through the same `RouteGate`.
    async fn settle_tick(
        &self,
        settler: &Settler<EvmRpcClient>,
        ledger: &mut Ledger,
    ) -> SettlementReport {
        let at = now_unix();
        let mut report = SettlementReport::default();
        let gate = Arc::clone(&self.route_gate);
        let open = move |ledger: &Ledger, route: Route| gate.is_enabled(ledger, route);
        settler.tick_fold(ledger, open(ledger, Route::RhnToGlc), at, &mut report);
        let is_open = {
            let snapshot: std::collections::BTreeMap<Route, bool> = Route::ALL
                .into_iter()
                .filter(|r| r.contract_route_id().is_some())
                .map(|r| (r, open(ledger, r)))
                .collect();
            move |route: Route| snapshot.get(&route).copied().unwrap_or(false)
        };
        settler
            .tick_authorize_gated(ledger, &is_open, at, &mut report)
            .await;
        settler.tick_broadcast(ledger, at, &mut report).await;
        settler.tick_receipts(ledger, at, &mut report).await;
        report
    }

    async fn index_robinhood_until_final(
        &self,
        indexer: &mut RobinhoodIndexer<EvmRpcClient>,
        obligation_index: u64,
    ) {
        for _ in 0..60 {
            indexer
                .tick(now_unix())
                .await
                .expect("robinhood indexer tick");
            let observations = self.ledger().robinhood_observations().unwrap();
            if observations.iter().any(|o| {
                o.observation.obligation_index == obligation_index
                    && o.finality == RobinhoodFinality::Final
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("obligation {obligation_index} never reached Final on the Robinhood indexer");
    }

    // ---- user actions ----

    /// A user's own `deposit_to_reserve` on the real validator.
    fn solana_deposit(&mut self, user: &Keypair, amount_atomic: u64, destination: &[u8]) -> u64 {
        let blocking = self.blocking();
        let index = self.next_obligation;
        let ix = instructions::deposit_to_reserve(
            &user.pubkey(),
            &self.mint,
            &self.token_program,
            index,
            amount_atomic,
            destination,
        );
        let bh = blocking.get_latest_blockhash().unwrap();
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&user.pubkey()), &[user], bh);
        blocking
            .send_and_confirm_transaction(&tx)
            .expect("real deposit_to_reserve must land");
        self.next_obligation += 1;
        index
    }

    /// A user's own `deposit(0x04, amount, destination)` on anvil, after
    /// approving the bridge. Returns the obligation index the contract
    /// assigned (read back from `obligationCount`).
    async fn evm_deposit_rhn_to_sol(&self, amount_18dp: u128, destination: &[u8]) -> u64 {
        let rpc = self.anvil.rpc_client();
        let before = self
            .reader()
            .obligation_count(&rpc, EvmBlockTag::Latest)
            .await
            .unwrap();
        let amount = amount_18dp.to_string();
        self.anvil.send_from_user(
            &self.token.to_checksum_string(),
            "approve(address,uint256)",
            &[&self.bridge.to_checksum_string(), &amount],
        );
        let dest_hex = format!("0x{}", hex32(destination));
        self.anvil.send_from_user(
            &self.bridge.to_checksum_string(),
            "deposit(uint8,uint256,bytes)",
            &["4", &amount, &dest_hex],
        );
        let after = self
            .reader()
            .obligation_count(&rpc, EvmBlockTag::Latest)
            .await
            .unwrap();
        assert_eq!(after, before + 1, "one obligation created");
        before
    }

    // ---- governance on the real contract ----

    async fn set_contract_route(&self, route: Route, enabled: bool) {
        self.govern(GovernancePayload::SetRouteEnabled { route, enabled })
            .await;
        let now_flag = self
            .reader()
            .route_enabled(
                &self.anvil.rpc_client(),
                route.contract_route_id().unwrap(),
                EvmBlockTag::Latest,
            )
            .await
            .unwrap();
        assert_eq!(now_flag, enabled, "routeEnabled({route:?}) on chain");
    }

    /// One governance action through the service's own 2-of-3 session —
    /// plan, quorum, simulate, broadcast, verify — as `glc-admin` runs it.
    async fn govern(&self, payload: GovernancePayload) {
        let rpc = self.anvil.rpc_client();
        let reader = self.reader();
        let before = governance_session::read_state(&reader, &rpc, EvmBlockTag::Latest)
            .await
            .expect("read governance state");
        let plan = governance_session::plan(
            before,
            self.verified.domain(),
            self.settlement_cfg.chain_id,
            payload.clone(),
            now_unix() as u64 + 600,
        )
        .expect("plan governance");
        let signers = [
            LocalGovernanceSigner {
                key: evm_key(self.signer_tags[0]),
            },
            LocalGovernanceSigner {
                key: evm_key(self.signer_tags[1]),
            },
        ];
        let submitter = self.submitter();
        let execution = governance_session::execute(
            &plan,
            &reader,
            &rpc,
            &submitter,
            &[&signers[0], &signers[1]],
            2,
            ReceiptWait {
                timeout_secs: 60,
                poll_interval_secs: 1,
            },
            |secs| Box::pin(tokio::time::sleep(Duration::from_secs(secs))),
        )
        .await;
        let execution = match execution {
            Ok(e) => e,
            Err(e) => {
                let hash = format!("{e:?}");
                let hash = hash.split('"').nth(1).unwrap_or("0x0").to_string();
                eprintln!("DIAG governance failed: {e:?}");
                for args in [
                    vec!["block", "latest"],
                    vec!["tx", hash.as_str()],
                    vec!["rpc", "txpool_content"],
                    vec!["nonce", &submitter.address().to_checksum_string()],
                    vec!["balance", &submitter.address().to_checksum_string()],
                ] {
                    let out = Command::new("cast")
                        .args(&args)
                        .args(["--rpc-url", &self.anvil.url])
                        .output()
                        .unwrap();
                    eprintln!(
                        "DIAG cast {:?}:\n{}{}",
                        args,
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                panic!("execute governance");
            }
        };
        println!(
            "  governance {payload:?} landed in {} (gas {})",
            execution.tx_hash, execution.gas_used
        );
    }

    /// The on-chain global pause the Solana refund path requires
    /// (`glc-admin onchain-pause --scope global`), set by the program
    /// admin and awaited at finalized commitment.
    async fn set_solana_global_pause(&self, paused: bool) {
        let ix = instructions::set_paused(
            &self.admin.pubkey(),
            instructions::PauseScope::Global,
            paused,
        );
        let blocking = self.blocking();
        let bh = blocking.get_latest_blockhash().unwrap();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.admin.pubkey()),
            &[&self.admin],
            bh,
        );
        blocking
            .send_and_confirm_transaction(&tx)
            .expect("set_paused");
        let rpc = self.solana_rpc();
        for _ in 0..200 {
            if let Ok(Some(account)) = rpc.get_account(&accounts::bridge_config_pda()).await {
                if let Ok(cfg) = accounts::decode_bridge_config(&account.data) {
                    if cfg.paused == paused {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        panic!("BridgeConfig.paused never reached {paused} at finalized commitment");
    }

    async fn submitter_nonce(&self) -> u64 {
        self.anvil
            .rpc_client()
            .pending_nonce(evm_key(self.submitter_tag).address())
            .await
            .unwrap()
    }

    async fn glc_balance_18dp(&self, holder: EvmAddress) -> u128 {
        let word = TokenReader::new(self.token)
            .balance_of(&self.anvil.rpc_client(), holder, EvmBlockTag::Latest)
            .await
            .unwrap();
        word.try_to_u128().expect("balance fits u128")
    }
}

// ------------------------------------------------------------- set-up --

async fn build_world(prereqs: Prereqs) -> World {
    // ---- Solana localnet: program, throwaway mint, reserve funding ----
    let admin = Keypair::new();
    let validator = LocalValidator::start(&prereqs.so, &accounts::PROGRAM_ID, &admin.pubkey());
    let blocking = validator.blocking_client();
    support::airdrop(&blocking, &admin.pubkey(), 20_000_000_000);
    let attestation_keys: Vec<Keypair> = (0..3).map(|_| Keypair::new()).collect();
    let attestation_pubkeys: Vec<Pubkey> = attestation_keys.iter().map(|k| k.pubkey()).collect();
    let token_program = spl_token_2022::ID;
    let mint = support::create_throwaway_token2022_mint(&blocking, &admin, SOLANA_GLC_DECIMALS);
    support::bootstrap_program(
        &blocking,
        &admin,
        &attestation_pubkeys,
        2,
        &mint.pubkey(),
        &token_program,
    );
    let reserve_ata = accounts::associated_token_address(
        &accounts::reserve_authority_pda(),
        &mint.pubkey(),
        &token_program,
    );
    support::mint_to(
        &blocking,
        &admin,
        &mint.pubkey(),
        &token_program,
        &reserve_ata,
        &admin,
        SOL_RESERVE_SEED,
    );
    support::wait_for_finalized_balance(&validator.real_rpc(), &reserve_ata, SOL_RESERVE_SEED)
        .await;
    let solana_submitter = Keypair::new();
    support::airdrop(&blocking, &solana_submitter.pubkey(), 20_000_000_000);
    println!(
        "  solana validator     = {} (ephemeral)",
        validator.rpc_url()
    );
    println!("  program              = {}", accounts::PROGRAM_ID);
    println!(
        "  reserve mint         = {} ({SOLANA_GLC_DECIMALS} dp, Token-2022)",
        mint.pubkey()
    );

    // ---- anvil: token + bridge with throwaway signers ----
    let anvil = Anvil::start();
    let signer_keys = [evm_key(1), evm_key(2), evm_key(3)];
    let submitter_key = evm_key(9);
    let _ = &submitter_key;
    let token = anvil.deploy(
        &prereqs.contracts_dir,
        "test/mocks/MockGlc.sol:MockGlc",
        &[],
    );
    let signers_arg = format!(
        "[{},{},{}]",
        signer_keys[0].address().to_checksum_string(),
        signer_keys[1].address().to_checksum_string(),
        signer_keys[2].address().to_checksum_string()
    );
    // Guardians and treasury: other pre-funded anvil accounts, distinct
    // from every signer and from the submitter.
    let guardians_arg = "[0x14dC79964da2C08b23698B3D3cc7Ca32193d9955,0x23618e81E3f5cdF7f54C3d65f7FBc0aBf5B21E8f,0xa0Ee7A142d267C1f36714E4a8F75612F20a79720]";
    let treasury = "0x976EA74026E726554dB657fA54763abd0C3a0aa9";
    let one = ONE_GLC_18DP;
    let limits_arg = format!(
        "({},{},{},{},{},{},0)",
        one,           // inboundMin
        20_000 * one,  // inboundMax
        100_000 * one, // inboundRollingLimit
        one,           // outboundMin
        20_000 * one,  // outboundMax
        100_000 * one  // outboundRollingLimit
    );
    let bridge = anvil.deploy(
        &prereqs.contracts_dir,
        "src/GlcRobinhoodBridge.sol:GlcRobinhoodBridge",
        &[
            token.to_checksum_string(),
            signers_arg,
            guardians_arg.to_string(),
            PROTOCOL_GOLDCOIN.to_string(),
            PROTOCOL_ROBINHOOD.to_string(),
            PROTOCOL_SOLANA.to_string(),
            limits_arg,
            treasury.to_string(),
        ],
    );
    // Reserve seed into the bridge; a depositor stake for the user.
    anvil.send_from_user(
        &token.to_checksum_string(),
        "mint(address,uint256)",
        &[
            &bridge.to_checksum_string(),
            &(RHN_RESERVE_SEED_GLC * one).to_string(),
        ],
    );
    anvil.send_from_user(
        &token.to_checksum_string(),
        "mint(address,uint256)",
        &[ANVIL_ADDR0, &(1_000 * one).to_string()],
    );
    anvil.set_balance(&submitter_key.address(), "0x3635C9ADC5DEA00000"); // 1000 ETH
    println!(
        "  anvil                = {} (ephemeral, chain {ANVIL_CHAIN_ID})",
        anvil.url
    );
    println!("  MockGlc token        = {}", token.to_checksum_string());
    println!("  GlcRobinhoodBridge   = {}", bridge.to_checksum_string());

    let chain_id = EvmChainId::new(ANVIL_CHAIN_ID).unwrap();
    let indexer_cfg = RobinhoodIndexerConfig::new(
        anvil.url.clone(),
        chain_id,
        bridge,
        token,
        0,                      // start_block
        RHN_CONFIRMATION_DEPTH, // confirmation_depth
        500,                    // poll_interval_ms
        10_000,                 // request_timeout_ms
        1_000,                  // max_log_block_range
    )
    .expect("a valid indexer config");
    // Through the validating constructor, exactly as `Config::load`
    // builds it, so no config invariant is bypassed by this rehearsal.
    let settlement_cfg = RobinhoodSettlementConfig::new(
        Some(&indexer_cfg),
        chain_id,
        bridge,
        TxEnvelope::Eip1559,
        "UNUSED_IN_THIS_REHEARSAL".to_string(),
        submitter_key.address(),
        [
            signer_keys[0].address(),
            signer_keys[1].address(),
            signer_keys[2].address(),
        ],
        600,                    // authorization_ttl_secs
        RHN_CONFIRMATION_DEPTH, // required_confirmations
        130,                    // gas_limit_margin_percent (30% headroom)
        2_000_000,              // max_gas_limit
        100_000_000_000,        // max_fee_per_gas_wei
        1_000_000_000,          // priority_fee_wei
        30,                     // rebroadcast_after_secs
        2,                      // max_replacements
        1_000_000_000_000_000,  // min_submitter_balance_wei
    )
    .expect("a valid settlement config");
    let verified = preflight::verify(&anvil.rpc_client(), &indexer_cfg, &settlement_cfg)
        .await
        .expect("Robinhood settlement preflight against the real anvil deployment");
    println!(
        "  preflight            = PASSED (token {} dp, envelope {}, chains 0x03={:?} 0x04={:?})",
        verified.token_decimals,
        verified.tx_envelope.as_str(),
        verified.chains_for(Route::SolToRhn).unwrap(),
        verified.chains_for(Route::RhnToSol).unwrap()
    );

    // ---- ledger: three reserves, both cross routes open on the service side ----
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ledger.sqlite3");
    {
        let mut ledger = Ledger::open(&db_path).unwrap();
        let now = now_unix();
        ledger
            .configure_reserve(
                ReserveDirection::SolanaReserve,
                SOL_RESERVE_SEED,
                0,
                SOL_RESERVE_SEED,
                SOL_RESERVE_SEED / 2,
                SOL_RESERVE_SEED / 4,
                now,
            )
            .unwrap();
        // The Robinhood reserve row is CANONICAL units (8 dp).
        let rhn_canonical = (RHN_RESERVE_SEED_GLC * 100_000_000) as u64;
        ledger
            .configure_reserve(
                ReserveDirection::RobinhoodReserve,
                rhn_canonical,
                0,
                rhn_canonical,
                rhn_canonical / 2,
                rhn_canonical / 4,
                now,
            )
            .unwrap();
        ledger
            // Balance 0 to match the inert node's empty wallet; nominal
            // thresholds only because the row's own invariants need them.
            .configure_reserve(ReserveDirection::GoldcoinReserve, 0, 0, 3, 2, 1, now)
            .unwrap();
        for route in [Route::SolToRhn, Route::RhnToSol] {
            ledger
                .set_route_enabled(route, true, Some("real-node acceptance"))
                .unwrap();
        }
    }
    let route_gate = Arc::new(RouteGate::new(
        RoutesConfig::default().with_robinhood(false, false, true, true),
        ChainRegistry::with_verified_robinhood(verified.clone()),
    ));

    let solana_url = validator.rpc_url();
    World {
        _validator: validator,
        solana_url,
        anvil,
        contracts_dir: prereqs.contracts_dir,
        db_path,
        _dir: dir,
        admin,
        solana_submitter,
        attestation_keys,
        mint: mint.pubkey(),
        token_program,
        next_obligation: 0,
        token,
        bridge,
        signer_tags: [1, 2, 3],
        submitter_tag: 9,
        indexer_cfg,
        settlement_cfg,
        verified,
        route_gate,
    }
}

const EVERY_STATE: [RequestState; 19] = [
    RequestState::LiquidityReserved,
    RequestState::AwaitingDeposit,
    RequestState::DepositObserved,
    RequestState::Confirming,
    RequestState::SourceFinalized,
    RequestState::SettlementAuthorized,
    RequestState::DestinationSubmitted,
    RequestState::DestinationConfirmed,
    RequestState::Settled,
    RequestState::Expired,
    RequestState::Cancelled,
    RequestState::Reorged,
    RequestState::InsufficientReserveAtSettlement,
    RequestState::DestinationSubmissionFailed,
    RequestState::ManualReview,
    RequestState::Failed,
    RequestState::RefundPending,
    RequestState::RefundBroadcast,
    RequestState::Refunded,
];

fn state_of(ledger: &Ledger, id: i64) -> RequestState {
    ledger.get_request(id).unwrap().unwrap().state
}

fn request_for_solana_obligation(ledger: &Ledger, index: u64) -> Option<i64> {
    for state in EVERY_STATE {
        for d in [Direction::SolToRhn, Direction::SolToGlc] {
            for r in ledger.requests_by_state(d, state).unwrap() {
                if r.source_obligation_index == Some(index) && r.direction == d {
                    return Some(r.id);
                }
            }
        }
    }
    None
}

fn request_for_robinhood_obligation(ledger: &Ledger, index: u64) -> Option<i64> {
    for state in EVERY_STATE {
        for r in ledger
            .requests_by_state(Direction::RhnToSol, state)
            .unwrap()
        {
            if r.source_obligation_index == Some(index) {
                return Some(r.id);
            }
        }
    }
    None
}

fn payout_op(
    ledger: &Ledger,
    request_id: i64,
) -> Option<glc_reserve_bridge_service::ledger::RobinhoodTx> {
    ledger
        .get_robinhood_tx_for(RobinhoodTxKind::Payout, request_id)
        .unwrap()
}

// ---------------------------------------------------------- the session --

#[tokio::test(flavor = "multi_thread")]
async fn sol_to_rhn_and_rhn_to_sol_settle_end_to_end_on_real_nodes() {
    let Some(prereqs) = prereqs() else {
        return;
    };
    println!("\n===== 0. ISOLATED ENVIRONMENT =====");
    let mut world = build_world(prereqs).await;
    let blocking = world.blocking();

    // The contract deploys PAUSED on both sides; governance opens it,
    // then both cross routes, through the real 2-of-3 session.
    world
        .govern(GovernancePayload::SetPaused {
            deposits_paused: false,
            payouts_paused: false,
        })
        .await;
    world.set_contract_route(Route::SolToRhn, true).await;
    world.set_contract_route(Route::RhnToSol, true).await;
    for route in [Route::SolToRhn, Route::RhnToSol] {
        assert!(
            world.route_gate.is_enabled(&world.ledger(), route),
            "{route:?} open on config + ledger + adapter"
        );
    }

    // A Solana depositor with 10 GLC.
    let sol_user = Keypair::new();
    support::airdrop(&blocking, &sol_user.pubkey(), 10_000_000_000);
    let sol_user_ata = support::create_ata(
        &blocking,
        &world.admin,
        &sol_user.pubkey(),
        &world.mint,
        &world.token_program,
    );
    support::mint_to(
        &blocking,
        &world.admin,
        &world.mint,
        &world.token_program,
        &sol_user_ata,
        &world.admin,
        10_000_000,
    );
    // An EVM recipient for SolToRhn payouts (a fresh, never-funded address).
    let evm_recipient = evm_key(0x42).address();
    // A Solana recipient for RhnToSol releases, whose ATA must exist.
    let sol_recipient = Keypair::new();
    let sol_recipient_ata = support::create_ata(
        &blocking,
        &world.admin,
        &sol_recipient.pubkey(),
        &world.mint,
        &world.token_program,
    );

    // =================================================================
    println!("\n===== 1. SolToRhn: Solana deposit -> executePayout -> completion -> Settled =====");
    // 2 GLC in mint units; net at 3% = 1.94 GLC.
    let deposit_atomic: u64 = 2_000_000;
    let gross_canonical = SolanaAtomic(deposit_atomic)
        .to_canonical(SOLANA_GLC_DECIMALS)
        .unwrap();
    let fb = compute_fee_at_bps(gross_canonical, CROSS_ROUTE_FEE_BPS).unwrap();
    let expected_payout_18dp = fb.net.to_robinhood().unwrap().get();
    let obligation = world.solana_deposit(
        &sol_user,
        deposit_atomic,
        evm_recipient.to_checksum_string().as_bytes(),
    );
    println!(
        "  deposit_to_reserve   = obligation {obligation}, {deposit_atomic} mint units -> {}",
        evm_recipient.to_checksum_string()
    );

    let mut orchestrator = world.orchestrator();
    let mut request_id = None;
    for _ in 0..40 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if let Some(id) = request_for_solana_obligation(&world.ledger(), obligation) {
            request_id = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let sol_to_rhn_id = request_id.expect("the deposit was folded");
    {
        let ledger = world.ledger();
        let request = ledger.get_request(sol_to_rhn_id).unwrap().unwrap();
        assert_eq!(
            request.direction,
            Direction::SolToRhn,
            "classified by its 0x destination"
        );
        assert_eq!(request.state, RequestState::SourceFinalized);
        assert_eq!(request.gross_amount_atomic, gross_canonical.0);
        assert_eq!(request.net_amount_atomic, fb.net.0);
        assert_eq!(request.recipient, evm_recipient.to_bytes().to_vec());
        println!("  folded               = request {sol_to_rhn_id} SolToRhn SourceFinalized (net {} canonical)", fb.net.0);
    }

    // The Robinhood settlement engine pays it out under route 0x03.
    let settler = world.settler();
    let mut settle_ledger = world.ledger();
    let mut payout_final = false;
    for _ in 0..60 {
        let report = world.settle_tick(&settler, &mut settle_ledger).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if let Some(op) = payout_op(&settle_ledger, sol_to_rhn_id) {
            if op.state == RobinhoodTxState::Finalized {
                payout_final = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    assert!(
        payout_final,
        "the SolToRhn payout must reach Finalized on anvil"
    );
    let op = payout_op(&settle_ledger, sol_to_rhn_id).unwrap();
    assert_eq!(op.route, Some(Route::SolToRhn));
    assert_eq!(
        state_of(&settle_ledger, sol_to_rhn_id),
        RequestState::DestinationConfirmed
    );
    assert_eq!(
        world.glc_balance_18dp(evm_recipient).await,
        expected_payout_18dp,
        "the recipient holds exactly the net, widened to 18 dp"
    );
    println!(
        "  executePayout        = tx {} finalized at block {:?}; recipient balance {expected_payout_18dp} wei-GLC; request DestinationConfirmed",
        op.tx_hash.map(|h| hex32(&h)).unwrap_or_default(),
        op.receipt_block_number
    );

    // =================================================================
    println!("\n===== 2a. RECONCILIATION inside the SolToRhn DestinationConfirmed window =====");
    {
        let mut ledger = world.ledger();
        let outcome = world
            .reserve_reconciler()
            .tick(&mut ledger, now_unix())
            .await;
        let ReserveTickOutcome::Reconciled {
            report,
            dust_remainder,
            ..
        } = outcome
        else {
            panic!("expected a real balanceOf reconciliation, got {outcome:?}");
        };
        assert_eq!(dust_remainder, 0);
        assert!(
            matches!(report.classification, Classification::WithinTolerance),
            "Robinhood reserve: the payout was debited at finality and must not be read as a \
             drop, nor be pending twice: {report:?}"
        );
        assert!(!report.auto_paused);
        println!(
            "  Robinhood reserve    = {:?} (cached {} -> observed {})",
            report.classification, report.cached_balance_before, report.observed_balance
        );
        assert_eq!(
            ledger
                .pending_destination_settlement_amount(
                    ReserveDirection::RobinhoodReserve,
                    now_unix()
                )
                .unwrap(),
            0,
            "a DestinationConfirmed SolToRhn row is not pending against the book"
        );
    }

    // =================================================================
    println!("\n===== 6a. RESTART between payout finality and the Solana completion =====");
    drop(orchestrator);
    drop(settler);
    let mut orchestrator = world.orchestrator();
    let mut settled = false;
    for _ in 0..80 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if state_of(&world.ledger(), sol_to_rhn_id) == RequestState::Settled {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        settled,
        "SolToRhn must reach Settled after the completion confirms"
    );
    {
        let ledger = world.ledger();
        let (sig, _) = ledger
            .robinhood_payout_completion_submission(sol_to_rhn_id)
            .unwrap()
            .unwrap();
        let signature = solana_sdk::signature::Signature::from(sig);
        assert_eq!(
            world
                .solana_rpc()
                .get_signature_status(&signature)
                .await
                .unwrap(),
            Some(Ok(()))
        );
        let account = world
            .solana_rpc()
            .get_account(&accounts::withdrawal_obligation_pda(obligation))
            .await
            .unwrap()
            .expect("obligation account");
        let onchain = accounts::decode_withdrawal_obligation(&account.data).unwrap();
        assert_eq!(
            onchain.status,
            accounts::WITHDRAWAL_STATUS_COMPLETED,
            "the Solana obligation is closed on chain: {onchain:?}"
        );
        println!("  record_goldcoin_completion = {signature}; obligation {obligation} closed; request Settled");
        // Accounting: Robinhood reserve down by the net (canonical), fee on the Solana row.
        let (rhn_balance, _, rhn_reserved, rhn_pending) = ledger
            .reserve_snapshot(ReserveDirection::RobinhoodReserve)
            .unwrap();
        assert_eq!(rhn_reserved, 0);
        assert_eq!(rhn_pending, 0);
        assert_eq!(
            rhn_balance,
            (RHN_RESERVE_SEED_GLC * 100_000_000) as u64 - fb.net.0
        );
        assert_eq!(
            ledger
                .accrued_fees(ReserveDirection::SolanaReserve)
                .unwrap(),
            fb.fee.0
        );
    }

    // =================================================================
    println!("\n===== 3. RhnToSol: anvil deposit(0x04) -> release_from_reserve -> executeSettlement -> Settled =====");
    let deposit_18dp = 5 * ONE_GLC_18DP;
    let gross_canonical_2 = CanonicalAtomic(500_000_000);
    let fb2 = compute_fee_at_bps(gross_canonical_2, CROSS_ROUTE_FEE_BPS).unwrap();
    let expected_release_mint = fb2.net.to_solana(SOLANA_GLC_DECIMALS).unwrap().0;
    let user_before = world.glc_balance_18dp(ANVIL_ADDR0.parse().unwrap()).await;
    let rhn_index = world
        .evm_deposit_rhn_to_sol(deposit_18dp, &sol_recipient.pubkey().to_bytes())
        .await;
    println!(
        "  deposit(0x04)        = obligation {rhn_index}, {deposit_18dp} wei-GLC -> {}",
        sol_recipient.pubkey()
    );

    let mut rhn_indexer = world.robinhood_indexer();
    world
        .index_robinhood_until_final(&mut rhn_indexer, rhn_index)
        .await;
    println!("  observed FINAL at depth {RHN_CONFIRMATION_DEPTH}");

    // The orchestrator folds it and submits the release.
    let mut rhn_to_sol_id = None;
    for _ in 0..40 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if let Some(id) = request_for_robinhood_obligation(&world.ledger(), rhn_index) {
            let s = state_of(&world.ledger(), id);
            if s == RequestState::DestinationSubmitted || s == RequestState::DestinationConfirmed {
                rhn_to_sol_id = Some(id);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let rhn_to_sol_id = rhn_to_sol_id.expect("folded and released");
    {
        let ledger = world.ledger();
        let request = ledger.get_request(rhn_to_sol_id).unwrap().unwrap();
        assert_eq!(request.direction, Direction::RhnToSol);
        assert_eq!(request.gross_amount_atomic, gross_canonical_2.0);
        assert_eq!(request.net_destination_atomic, expected_release_mint);
        assert_eq!(
            request.recipient,
            sol_recipient.pubkey().to_bytes().to_vec()
        );
        println!(
            "  folded + released    = request {rhn_to_sol_id} {:?}, release sig {}",
            request.state,
            ledger
                .get_destination_txid(rhn_to_sol_id)
                .unwrap()
                .map(|b| solana_sdk::bs58::encode(b).into_string())
                .unwrap_or_default()
        );
    }

    // ===== 6b. RESTART while the release is in flight =====
    println!("\n===== 6b. RESTART while the release is DestinationSubmitted =====");
    drop(orchestrator);
    let mut orchestrator = world.orchestrator();
    let mut confirmed = false;
    for _ in 0..80 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if state_of(&world.ledger(), rhn_to_sol_id) == RequestState::DestinationConfirmed {
            confirmed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(confirmed, "the release must confirm at finalized");
    assert_eq!(
        support::token_balance(&blocking, &sol_recipient_ata),
        expected_release_mint,
        "the recipient's ATA holds exactly the net in mint units"
    );
    println!("  release finalized    = recipient ATA {expected_release_mint} mint units; request DestinationConfirmed");

    // ===== 2b. RECONCILIATION of the Solana reserve inside the window =====
    println!("\n===== 2b. RECONCILIATION inside the RhnToSol DestinationConfirmed window =====");
    {
        let mut ledger = world.ledger();
        let reserve_ata = accounts::associated_token_address(
            &accounts::reserve_authority_pda(),
            &world.mint,
            &world.token_program,
        );
        let observed = support::token_balance(&blocking, &reserve_ata);
        let report = reconciliation::reconcile(
            &mut ledger,
            ReserveDirection::SolanaReserve,
            observed,
            0,
            now_unix(),
        )
        .unwrap();
        assert!(
            matches!(report.classification, Classification::WithinTolerance),
            "Solana reserve: the release was debited at finality: {report:?}"
        );
        assert!(!report.auto_paused);
        // The SolToRhn deposit in section 1 moved INTO this vault.
        assert_eq!(
            observed,
            SOL_RESERVE_SEED + deposit_atomic - expected_release_mint
        );
        assert_eq!(
            ledger
                .pending_destination_settlement_amount(ReserveDirection::SolanaReserve, now_unix())
                .unwrap(),
            0
        );
        println!(
            "  Solana reserve       = {:?} (observed {observed})",
            report.classification
        );
    }

    // The settlement engine closes the obligation on anvil.
    let settler = world.settler();
    let mut settle_ledger = world.ledger();
    let mut settled = false;
    for _ in 0..60 {
        let report = world.settle_tick(&settler, &mut settle_ledger).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if state_of(&settle_ledger, rhn_to_sol_id) == RequestState::Settled {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    assert!(
        settled,
        "RhnToSol must reach Settled once executeSettlement finalizes"
    );
    {
        let settlement = settle_ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Settlement, rhn_to_sol_id)
            .unwrap()
            .unwrap();
        assert_eq!(settlement.route, Some(Route::RhnToSol));
        assert_eq!(settlement.state, RobinhoodTxState::Finalized);
        let onchain = world
            .reader()
            .obligation(&world.anvil.rpc_client(), rhn_index, EvmBlockTag::Latest)
            .await
            .unwrap();
        assert_eq!(onchain.route, 0x04);
        assert!(
            !onchain.is_pending(),
            "obligation settled on chain: {}",
            onchain.status_name()
        );
        let user_after = world.glc_balance_18dp(ANVIL_ADDR0.parse().unwrap()).await;
        assert_eq!(user_before - user_after, deposit_18dp);
        let ledger = world.ledger();
        let (sol_balance, _, sol_reserved, sol_pending) = ledger
            .reserve_snapshot(ReserveDirection::SolanaReserve)
            .unwrap();
        assert_eq!((sol_reserved, sol_pending), (0, 0));
        assert_eq!(
            sol_balance,
            SOL_RESERVE_SEED + deposit_atomic - expected_release_mint
        );
        assert_eq!(
            ledger
                .accrued_fees(ReserveDirection::RobinhoodReserve)
                .unwrap(),
            fb2.fee.0
        );
        println!(
            "  executeSettlement    = tx {} finalized; obligation {rhn_index} {} on chain; request Settled",
            settlement.tx_hash.map(|h| hex32(&h)).unwrap_or_default(),
            onchain.status_name()
        );
    }

    // =================================================================
    println!("\n===== 4a. REFUND on Solana: an undeliverable SolToRhn destination =====");
    let bad_obligation = world.solana_deposit(&sol_user, 1_000_000, b"0xnotanaddress");
    let mut parked = None;
    for _ in 0..40 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if let Some(id) = request_for_solana_obligation(&world.ledger(), bad_obligation) {
            parked = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let parked_id = parked.expect("folded");
    {
        let ledger = world.ledger();
        let request = ledger.get_request(parked_id).unwrap().unwrap();
        assert_eq!(
            request.direction,
            Direction::SolToRhn,
            "a 0x payload is never a Goldcoin request"
        );
        assert_eq!(request.state, RequestState::ManualReview);
        assert!(request
            .manual_review_note
            .as_deref()
            .unwrap_or("")
            .starts_with("undeliverable destination"));
        println!(
            "  parked               = request {parked_id} ManualReview ({})",
            request.manual_review_note.clone().unwrap_or_default()
        );
    }
    let user_ata_before = support::token_balance(&blocking, &sol_user_ata);
    // The refund path refuses without the program's global pause — the
    // runbook's own sequence: pause, refund, unpause explicitly.
    world.set_solana_global_pause(true).await;
    let mut refund_ledger = world.ledger();
    let outcome = refund::execute_refund(
        &world.solana_rpc(),
        &mut refund_ledger,
        &world.attestation_signers(),
        &world.admin,
        &world.solana_submitter,
        parked_id,
        "real-node acceptance: refunding an undeliverable SolToRhn destination",
        "cli:acceptance",
        ConfirmPolicy::default(),
    )
    .await
    .expect("execute the Solana refund");
    let RefundExecuteOutcome::Confirmed { signature } = &outcome else {
        panic!("expected Confirmed, got {outcome:?}");
    };
    assert_eq!(state_of(&refund_ledger, parked_id), RequestState::Refunded);
    assert_eq!(
        support::token_balance(&blocking, &sol_user_ata),
        user_ata_before + 1_000_000,
        "the depositor got the full principal back"
    );
    println!("  refund_withdraw      = {signature}; request Refunded; principal returned");
    world.set_solana_global_pause(false).await;

    // =================================================================
    println!("\n===== 4b. REFUND on anvil: an undeliverable RhnToSol destination =====");
    let user_before_refund = world.glc_balance_18dp(ANVIL_ADDR0.parse().unwrap()).await;
    let bad_rhn_index = world
        .evm_deposit_rhn_to_sol(2 * ONE_GLC_18DP, b"nope")
        .await;
    world
        .index_robinhood_until_final(&mut rhn_indexer, bad_rhn_index)
        .await;
    let mut parked_rhn = None;
    for _ in 0..40 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if let Some(id) = request_for_robinhood_obligation(&world.ledger(), bad_rhn_index) {
            parked_rhn = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let parked_rhn_id = parked_rhn.expect("folded");
    {
        let ledger = world.ledger();
        let request = ledger.get_request(parked_rhn_id).unwrap().unwrap();
        assert_eq!(request.state, RequestState::ManualReview);
        assert!(request
            .manual_review_note
            .as_deref()
            .unwrap_or("")
            .starts_with("undeliverable destination"));
        println!(
            "  parked               = request {parked_rhn_id} ManualReview ({})",
            request.manual_review_note.clone().unwrap_or_default()
        );
    }
    let mut settle_ledger = world.ledger();
    let refund_tx =
        robinhood::begin_refund(&settler, &mut settle_ledger, parked_rhn_id, now_unix())
            .await
            .expect("authorize the Robinhood refund");
    let mut refunded = false;
    for _ in 0..60 {
        let report = world.settle_tick(&settler, &mut settle_ledger).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if state_of(&settle_ledger, parked_rhn_id) == RequestState::Refunded {
            refunded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(700)).await;
    }
    assert!(
        refunded,
        "executeRefund must finalize and the request reach Refunded"
    );
    {
        let tx = settle_ledger
            .get_robinhood_tx_for(RobinhoodTxKind::Refund, parked_rhn_id)
            .unwrap()
            .unwrap();
        assert_eq!(tx.id, refund_tx);
        assert_eq!(tx.route, Some(Route::RhnToSol));
        assert_eq!(tx.state, RobinhoodTxState::Finalized);
        let user_after_refund = world.glc_balance_18dp(ANVIL_ADDR0.parse().unwrap()).await;
        assert_eq!(
            user_after_refund, user_before_refund,
            "the depositor's exact principal came back"
        );
        let onchain = world
            .reader()
            .obligation(
                &world.anvil.rpc_client(),
                bad_rhn_index,
                EvmBlockTag::Latest,
            )
            .await
            .unwrap();
        assert!(!onchain.is_pending());
        println!("  executeRefund        = tx {} finalized; obligation {bad_rhn_index} {}; principal returned", tx.tx_hash.map(|h| hex32(&h)).unwrap_or_default(), onchain.status_name());
    }

    // =================================================================
    println!(
        "\n===== 5. CONTRACT-DISABLED SolToRhn: refused before broadcast, no nonce consumed ====="
    );
    world.set_contract_route(Route::SolToRhn, false).await;
    let nonce_before = world.submitter_nonce().await;
    let gated_obligation = world.solana_deposit(
        &sol_user,
        1_500_000,
        evm_recipient.to_checksum_string().as_bytes(),
    );
    let mut gated = None;
    for _ in 0..40 {
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if let Some(id) = request_for_solana_obligation(&world.ledger(), gated_obligation) {
            gated = Some(id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let gated_id = gated.expect("folded");
    // The service-side gates are still open, so the fold is payable...
    assert_eq!(
        state_of(&world.ledger(), gated_id),
        RequestState::SourceFinalized
    );
    let mut settle_ledger = world.ledger();
    let report = world.settle_tick(&settler, &mut settle_ledger).await;
    // ...the authorization is minted, but the live contract flag refuses
    // the broadcast before a nonce is allocated.
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("SolToRhn") || e.contains("disabled")),
        "the contract gate must refuse: {:?}",
        report.errors
    );
    let op = payout_op(&settle_ledger, gated_id).expect("an authorization row exists");
    assert_eq!(op.state, RobinhoodTxState::Authorized);
    assert_eq!(op.nonce, None, "no nonce allocated");
    assert_eq!(op.tx_hash, None, "nothing signed");
    let nonce_after = world.submitter_nonce().await;
    assert_eq!(
        nonce_before, nonce_after,
        "the submitter's on-chain nonce did not move"
    );
    // A second tick does not sneak it through either.
    let report = world.settle_tick(&settler, &mut settle_ledger).await;
    assert!(!report.errors.is_empty());
    assert_eq!(world.submitter_nonce().await, nonce_before);
    assert_eq!(
        world.glc_balance_18dp(evm_recipient).await,
        expected_payout_18dp,
        "the recipient's balance is unchanged"
    );
    println!("  refused              = {}", report.errors[0]);
    println!("  submitter nonce      = {nonce_before} before and after; operation Authorized with no nonce/tx");
    // Re-enabling the route lets the SAME authorization proceed to Settled.
    world.set_contract_route(Route::SolToRhn, true).await;
    let mut done = false;
    for _ in 0..80 {
        let report = world.settle_tick(&settler, &mut settle_ledger).await;
        assert_eq!(report.errors, Vec::<String>::new());
        let report = orchestrator.tick(now_unix()).await;
        assert_eq!(report.errors, Vec::<String>::new());
        if state_of(&world.ledger(), gated_id) == RequestState::Settled {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(done, "after re-enabling, the held authorization settles");
    println!("  re-enabled           = request {gated_id} Settled with the same authorization");

    // =================================================================
    println!("\n===== FINAL BOOK =====");
    {
        let ledger = world.ledger();
        for (direction, ids) in [
            (
                Direction::SolToRhn,
                vec![sol_to_rhn_id, parked_id, gated_id],
            ),
            (Direction::RhnToSol, vec![rhn_to_sol_id, parked_rhn_id]),
        ] {
            for id in ids {
                let r = ledger.get_request(id).unwrap().unwrap();
                println!("  request {id:>3} {:<9} {:?}", direction.as_str(), r.state);
                assert_eq!(r.direction, direction);
            }
        }
        // Final reconciliation of both reserves against both chains.
        let mut l = world.ledger();
        let outcome = world.reserve_reconciler().tick(&mut l, now_unix()).await;
        let ReserveTickOutcome::Reconciled { report, .. } = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(
            report.classification,
            Classification::WithinTolerance,
            "{report:?}"
        );
        let reserve_ata = accounts::associated_token_address(
            &accounts::reserve_authority_pda(),
            &world.mint,
            &world.token_program,
        );
        let observed = support::token_balance(&blocking, &reserve_ata);
        let report = reconciliation::reconcile(
            &mut l,
            ReserveDirection::SolanaReserve,
            observed,
            0,
            now_unix(),
        )
        .unwrap();
        assert_eq!(
            report.classification,
            Classification::WithinTolerance,
            "{report:?}"
        );
        assert!(!l.is_paused(ReserveDirection::SolanaReserve).unwrap());
        assert!(!l.is_paused(ReserveDirection::RobinhoodReserve).unwrap());
        println!("  both reserves WithinTolerance against live on-chain balances; nothing paused");
    }
    let _ = &world.contracts_dir;
}
