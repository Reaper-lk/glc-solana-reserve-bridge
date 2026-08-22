//! Shared real-node/real-validator test harness for the Phase 6
//! acceptance rehearsal (docs/13-phase6-readiness-audit.md). Ported
//! pattern from the old bridge's `relayer/tests/{regtest_indexer,
//! local_validator_e2e}.rs` (docs/01-reuse-inventory.md: chain-mechanics
//! test infrastructure, no mint/burn or federation coupling) — env-var
//! gated, skip-not-fail when the required binaries aren't present,
//! throwaway credentials/keys/ports everywhere, torn down at the end of
//! every test that uses it.
//!
//! Nothing in this module ever touches a non-loopback address, a real
//! wallet path, or a fixed port. No key material here is ever written to
//! a committed file — every keypair is generated fresh in-process.

#![allow(dead_code)] // not every test file in this directory uses every helper

pub mod load_harness;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use solana_client::rpc_client::RpcClient as BlockingRpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
// `solana_sdk::system_instruction` is deprecated in favor of a dedicated
// `solana_system_interface` crate; not worth a new dependency for the one
// function this module needs (`create_account`), which is unchanged.
#[allow(deprecated)]
use solana_sdk::system_instruction;
use solana_sdk::transaction::Transaction;

use glc_reserve_bridge_service::goldcoin::deposit::encode_request_binding;
use glc_reserve_bridge_service::goldcoin::hex as glc_hex;
use glc_reserve_bridge_service::goldcoin::rpc::{
    RpcClient as GoldcoinRpcClient, RpcConfig as GoldcoinRpcConfig,
};
use glc_reserve_bridge_service::solana::accounts;
use glc_reserve_bridge_service::solana::instructions;
use glc_reserve_bridge_service::solana::rpc::{RealSolanaRpc, SolanaRpc};

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A random high base for `solana-test-validator --dynamic-port-range`,
/// well clear of the low fixed range (1024+) that any other already-
/// running validator on this host would use by default.
fn dynamic_port_range_base() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    20_000 + (nanos % 20_000) as u16
}

pub fn goldcoind_bin() -> Option<PathBuf> {
    std::env::var_os("GOLDCOIND_BIN").map(PathBuf::from)
}

pub fn goldcoin_cli_bin() -> Option<PathBuf> {
    std::env::var_os("GOLDCOIN_CLI_BIN").map(PathBuf::from)
}

pub fn program_so_path() -> Option<PathBuf> {
    let p = std::env::var("GLC_RESERVE_BRIDGE_SO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../target/deploy/glc_reserve_bridge.so"));
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

pub fn solana_test_validator_available() -> bool {
    Command::new("solana-test-validator")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `Some((goldcoind, goldcoin_cli, program_so))` only when every Phase 6
/// prerequisite is present; `None` otherwise. Callers print a `skipping:`
/// line and return early on `None` — never fail — matching exactly how
/// the old bridge's own real-node tests behaved in an environment without
/// these binaries.
pub fn phase6_prereqs() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let goldcoind = goldcoind_bin();
    let cli = goldcoin_cli_bin();
    let so = program_so_path();
    match (goldcoind, cli, so) {
        (Some(d), Some(c), Some(s)) if solana_test_validator_available() => Some((d, c, s)),
        _ => None,
    }
}

// --------------------------------------------------------- Goldcoin regtest --

pub struct RegtestNode {
    child: Child,
    cli_bin: PathBuf,
    datadir: tempfile::TempDir,
    rpc_port: u16,
    rpc_user: String,
    rpc_password: String,
}

impl RegtestNode {
    /// Starts a fresh, throwaway regtest node: `-txindex=1` (mandatory,
    /// docs/goldcoin-rpc-notes.md), bound to localhost only, OS-assigned
    /// free ports, single-process throwaway credentials.
    pub fn start(goldcoind: &Path, cli: &Path) -> Self {
        let datadir = tempfile::tempdir().expect("tempdir");
        let rpc_port = free_port();
        let p2p_port = free_port();
        let rpc_user = "glc_reserve_test_user".to_string();
        let rpc_password = format!("glc_reserve_test_pw_{}", std::process::id());

        let child = Command::new(goldcoind)
            .arg("-regtest")
            .arg(format!("-datadir={}", datadir.path().display()))
            .arg("-daemon=0")
            .arg("-printtoconsole=0")
            .arg(format!("-rpcuser={rpc_user}"))
            .arg(format!("-rpcpassword={rpc_password}"))
            .arg(format!("-rpcport={rpc_port}"))
            .arg(format!("-port={p2p_port}"))
            .arg("-rpcbind=127.0.0.1")
            .arg("-rpcallowip=127.0.0.1")
            .arg("-bind=127.0.0.1")
            .arg("-fallbackfee=0.0001")
            .arg("-txindex=1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn goldcoind — check GOLDCOIND_BIN");

        let node = RegtestNode {
            child,
            cli_bin: cli.to_path_buf(),
            datadir,
            rpc_port,
            rpc_user,
            rpc_password,
        };
        node.wait_for_rpc_ready();
        node
    }

    fn cli_cmd(&self) -> Command {
        let mut cmd = Command::new(&self.cli_bin);
        cmd.arg("-regtest")
            .arg(format!("-datadir={}", self.datadir.path().display()))
            .arg(format!("-rpcport={}", self.rpc_port))
            .arg(format!("-rpcuser={}", self.rpc_user))
            .arg(format!("-rpcpassword={}", self.rpc_password));
        cmd
    }

    pub fn cli(&self, args: &[&str]) -> String {
        let out = self
            .cli_cmd()
            .args(args)
            .output()
            .expect("failed to run goldcoin-cli");
        assert!(
            out.status.success(),
            "goldcoin-cli {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn wait_for_rpc_ready(&self) {
        for _ in 0..100 {
            let ok = self
                .cli_cmd()
                .arg("getblockcount")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("goldcoind did not become RPC-ready in time");
    }

    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rpc_port)
    }

    pub fn rpc_config(&self) -> GoldcoinRpcConfig {
        GoldcoinRpcConfig {
            url: self.rpc_url(),
            user: self.rpc_user.clone(),
            password: self.rpc_password.clone(),
            connect_timeout_ms: 5_000,
            read_timeout_ms: 30_000,
        }
    }

    pub fn rpc_client(&self) -> GoldcoinRpcClient {
        GoldcoinRpcClient::new(&self.rpc_config()).expect("build goldcoin rpc client")
    }

    pub fn new_address(&self) -> String {
        self.cli(&["getnewaddress"])
    }

    pub fn vault_script_hex(&self, address: &str) -> String {
        let json = self.cli(&["validateaddress", address]);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v["scriptPubKey"].as_str().unwrap().to_string()
    }

    pub fn generate(&self, n: u32, address: &str) {
        self.cli(&["generatetoaddress", &n.to_string(), address]);
    }

    pub fn block_count(&self) -> i64 {
        self.cli(&["getblockcount"]).parse().unwrap()
    }

    pub fn best_block_hash(&self) -> String {
        self.cli(&["getbestblockhash"])
    }

    pub fn invalidate_block(&self, hash: &str) {
        self.cli(&["invalidateblock", hash]);
    }

    /// Registers a P2SH vault so its outputs become visible/solvable —
    /// the exact two-call sequence docs/goldcoin-rpc-notes.md confirms is
    /// required (`importaddress`ing the address alone leaves outputs
    /// `solvable: false`; the redeem script must be imported too, with the
    /// `p2sh` flag).
    pub fn import_vault(&self, address: &str, redeem_script_hex: &str) {
        self.cli(&["importaddress", address, "glc-reserve-vault", "false"]);
        self.cli(&[
            "importaddress",
            redeem_script_hex,
            "glc-reserve-vault-redeem",
            "false",
            "true",
        ]);
    }

    /// Builds, funds, signs, and broadcasts a real regtest deposit paying
    /// `amount_glc` to `vault_address` with a 32-byte OP_RETURN binding
    /// this bridge's own `bridge_requests.id` (never the recipient —
    /// docs/01-reuse-inventory.md's FIFO-ambiguity rationale). Returns the
    /// RPC txid.
    pub fn send_deposit_with_binding(
        &self,
        vault_address: &str,
        amount_glc: f64,
        request_id: i64,
    ) -> String {
        let payload_hex = glc_hex::encode(&encode_request_binding(request_id));
        let outputs = format!("{{\"{vault_address}\":{amount_glc},\"data\":\"{payload_hex}\"}}");
        let raw = self.cli(&["createrawtransaction", "[]", &outputs]);
        let funded_json = self.cli(&["fundrawtransaction", &raw]);
        let funded: serde_json::Value = serde_json::from_str(&funded_json).unwrap();
        let funded_hex = funded["hex"].as_str().unwrap();
        let signed_json = self.cli(&["signrawtransaction", funded_hex]);
        let signed: serde_json::Value = serde_json::from_str(&signed_json).unwrap();
        let signed_hex = signed["hex"].as_str().unwrap();
        self.cli(&["sendrawtransaction", signed_hex])
    }

    /// The output index paying `vault_script_hex` in `txid` — never assume
    /// vout 0 (docs/goldcoin-rpc-notes.md: `fundrawtransaction` chooses the
    /// change position, so the vault output's index genuinely varies).
    pub fn vault_vout_of(&self, txid_hex: &str, vault_script_hex: &str) -> u32 {
        let tx: serde_json::Value =
            serde_json::from_str(&self.cli(&["getrawtransaction", txid_hex, "true"])).unwrap();
        tx["vout"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["scriptPubKey"]["hex"].as_str() == Some(vault_script_hex))
            .map(|o| o["n"].as_u64().unwrap() as u32)
            .expect("the deposit must pay the vault in some output")
    }

    /// Sum of confirmed `listunspent` amounts paying `address` — used to
    /// verify a real Goldcoin payout landed for exactly the right amount.
    pub fn confirmed_balance_of(&self, address: &str) -> f64 {
        let json = self.cli(&["listunspent", "1", "9999999", &format!("[\"{address}\"]")]);
        let entries: serde_json::Value = serde_json::from_str(&json).unwrap();
        entries
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["amount"].as_f64().unwrap())
            .sum()
    }
}

impl Drop for RegtestNode {
    fn drop(&mut self) {
        let _ = self.cli_cmd().arg("stop").output();
        for _ in 0..50 {
            if self.child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --------------------------------------------------------- Solana localnet --

pub struct LocalValidator {
    child: Child,
    _ledger: tempfile::TempDir,
    rpc_port: u16,
}

impl LocalValidator {
    /// Bakes `so_path` directly into genesis at `program_id` (no deploy
    /// transaction, no signature from that address ever required — the
    /// proven pattern from the old bridge's `local_validator_e2e.rs`).
    pub fn start(so_path: &Path, program_id: &Pubkey, upgrade_authority: &Pubkey) -> Self {
        let ledger = tempfile::tempdir().expect("tempdir");
        let rpc_port = free_port();
        let faucet_port = free_port();
        let gossip_port = free_port();
        // solana-test-validator's gossip/TVU/TPU/repair ports default to a
        // FIXED low base (1024+) regardless of --rpc-port, which silently
        // collides with any other already-running validator on the same
        // host (confirmed in this environment: a pre-existing, unrelated
        // validator from prior manual rehearsal work already owns that
        // range, which broke this validator's whole transaction-processing
        // pipeline — RPC accepted transactions but they never landed,
        // diagnosed via `solana_gossip::cluster_info`'s "Insert self
        // failed" error). A random high port-range base avoids it.
        let range_base = dynamic_port_range_base();

        let child = Command::new("solana-test-validator")
            .arg("--reset")
            .arg("--quiet")
            .arg("--ledger")
            .arg(ledger.path())
            .arg("--rpc-port")
            .arg(rpc_port.to_string())
            .arg("--faucet-port")
            .arg(faucet_port.to_string())
            .arg("--gossip-port")
            .arg(gossip_port.to_string())
            .arg("--dynamic-port-range")
            .arg(format!("{range_base}-{}", range_base + 100))
            .arg("--bind-address")
            .arg("127.0.0.1")
            .arg("--upgradeable-program")
            .arg(program_id.to_string())
            .arg(so_path)
            .arg(upgrade_authority.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn solana-test-validator — check it is on PATH");

        let validator = LocalValidator {
            child,
            _ledger: ledger,
            rpc_port,
        };
        validator.wait_ready();
        validator
    }

    fn wait_ready(&self) {
        let client =
            BlockingRpcClient::new_with_commitment(self.rpc_url(), CommitmentConfig::confirmed());
        for _ in 0..200 {
            if client.get_health().is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("solana-test-validator did not become healthy within 20s");
    }

    pub fn rpc_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.rpc_port)
    }

    /// A plain blocking client for test setup (mint creation, program
    /// bootstrap, simulating a user's own `deposit_to_reserve`) — none of
    /// this is the code under test, so the simpler synchronous
    /// `send_and_confirm_transaction` is used rather than the async
    /// `RealSolanaRpc`/`solana::confirm` path the orchestrator itself uses.
    pub fn blocking_client(&self) -> BlockingRpcClient {
        BlockingRpcClient::new_with_commitment(self.rpc_url(), CommitmentConfig::confirmed())
    }

    /// The real async RPC implementation the orchestrator itself uses —
    /// what makes this a genuine, not mocked, acceptance rehearsal.
    pub fn real_rpc(&self) -> RealSolanaRpc {
        RealSolanaRpc::new(self.rpc_url())
    }
}

impl Drop for LocalValidator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn airdrop(client: &BlockingRpcClient, to: &Pubkey, lamports: u64) {
    let sig = client
        .request_airdrop(to, lamports)
        .expect("airdrop request");
    for _ in 0..100 {
        if client.confirm_transaction(&sig).unwrap_or(false) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("airdrop did not confirm within 10s");
}

fn submit(client: &BlockingRpcClient, payer: &Pubkey, ixs: &[Instruction], signers: &[&Keypair]) {
    let blockhash = client.get_latest_blockhash().expect("get_latest_blockhash");
    let tx = Transaction::new_signed_with_payer(ixs, Some(payer), signers, blockhash);
    client
        .send_and_confirm_transaction(&tx)
        .unwrap_or_else(|e| panic!("transaction failed: {e}"));
}

/// Creates a fresh, valueless throwaway legacy SPL Token mint standing in
/// for the Solana GLC token. The canonical Solana GLC mint is actually
/// Token-2022 (docs/18-token-2022-support.md) — use
/// [`create_throwaway_token2022_mint`] for the real-shaped case; this
/// legacy variant is kept so tests can still cover the "other legitimate
/// SPL program" path this bridge also supports. `payer` also becomes the
/// mint authority.
pub fn create_throwaway_mint(client: &BlockingRpcClient, payer: &Keypair, decimals: u8) -> Keypair {
    let mint = Keypair::new();
    let space: usize = 82; // spl_token::state::Mint::LEN (via the Pack trait) — a stable, well-known SPL account size
    let rent = client
        .get_minimum_balance_for_rent_exemption(space)
        .expect("get_minimum_balance_for_rent_exemption");
    let create_account_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        space as u64,
        &spl_token::ID,
    );
    let init_mint_ix = spl_token::instruction::initialize_mint2(
        &spl_token::ID,
        &mint.pubkey(),
        &payer.pubkey(),
        None,
        decimals,
    )
    .expect("build initialize_mint2");
    submit(
        client,
        &payer.pubkey(),
        &[create_account_ix, init_mint_ix],
        &[payer, &mint],
    );
    mint
}

/// Creates a fresh, valueless throwaway Token-2022 mint carrying a
/// self-referential `MetadataPointer` extension — the same shape the real
/// canonical Solana GLC mint has (verified read-only against mainnet:
/// Token-2022, `MetadataPointer` + `TokenMetadata`, docs/18-token-2022-
/// support.md). Exercises the real on-chain extension-allowlist path
/// (`crate::token_extensions::validate_mint_extensions`) end-to-end
/// against a real validator, not just in a unit test. `payer` also
/// becomes the mint authority and the metadata-pointer authority.
pub fn create_throwaway_token2022_mint(
    client: &BlockingRpcClient,
    payer: &Keypair,
    decimals: u8,
) -> Keypair {
    use spl_token_2022::extension::ExtensionType;
    use spl_token_2022::state::Mint as Token2022Mint;

    let mint = Keypair::new();
    let space = ExtensionType::try_calculate_account_len::<Token2022Mint>(&[
        ExtensionType::MetadataPointer,
    ])
    .expect("calculate mint-with-metadata-pointer account length");
    let rent = client
        .get_minimum_balance_for_rent_exemption(space)
        .expect("get_minimum_balance_for_rent_exemption");
    let create_account_ix = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        space as u64,
        &spl_token_2022::ID,
    );
    // Extension init must precede `InitializeMint2` (spl-token-2022
    // requirement — see the instruction's own doc comment).
    let init_pointer_ix = spl_token_2022::extension::metadata_pointer::instruction::initialize(
        &spl_token_2022::ID,
        &mint.pubkey(),
        Some(payer.pubkey()),
        Some(mint.pubkey()),
    )
    .expect("build metadata_pointer initialize");
    // Must be `spl_token_2022`'s own `initialize_mint2` builder, not
    // `spl_token`'s (legacy crate): both encode an identical
    // `InitializeMint2` instruction, but the legacy crate's builder calls
    // `check_program_account`, which only accepts `spl_token::ID` and
    // rejects the Token-2022 program id with `IncorrectProgramId` before
    // the instruction is even sent — a client-side check, not an on-chain
    // one. `spl_token_2022`'s own builder calls the more permissive
    // `check_spl_token_program_account`, which accepts both.
    let init_mint_ix = spl_token_2022::instruction::initialize_mint2(
        &spl_token_2022::ID,
        &mint.pubkey(),
        &payer.pubkey(),
        None,
        decimals,
    )
    .expect("build initialize_mint2");
    submit(
        client,
        &payer.pubkey(),
        &[create_account_ix, init_pointer_ix, init_mint_ix],
        &[payer, &mint],
    );
    mint
}

/// Creates `owner`'s associated token account for `mint` under
/// `token_program` — required up-front for a `release_from_reserve`
/// recipient (the on-chain instruction deliberately has no
/// `init_if_needed` for it — see `programs/glc-reserve-bridge/src/
/// instructions/release_from_reserve.rs`). `token_program` must match
/// whichever program actually owns `mint` — legacy SPL Token or
/// Token-2022 (docs/18-token-2022-support.md) — since the ATA address
/// itself is derived from it.
pub fn create_ata(
    client: &BlockingRpcClient,
    payer: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Pubkey {
    let ix = spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        owner,
        mint,
        token_program,
    );
    submit(client, &payer.pubkey(), &[ix], &[payer]);
    spl_associated_token_account::get_associated_token_address_with_program_id(
        owner,
        mint,
        token_program,
    )
}

/// `token_program` must match whichever program actually owns `mint` —
/// the base `MintTo` instruction has identical accounts/encoding under
/// either legacy SPL Token or Token-2022. Built via `spl_token_2022`'s own
/// `mint_to` (not `spl_token`'s): both encode the identical instruction,
/// but `spl_token`'s builder calls `check_program_account`, which only
/// accepts `spl_token::ID` and client-side-rejects the Token-2022 program
/// id with `IncorrectProgramId` before anything is even sent — the
/// `spl_token_2022` crate's own builder calls the more permissive
/// `check_spl_token_program_account`, which accepts both, so it is the one
/// builder that actually works for both program ids as this function's own
/// contract promises.
pub fn mint_to(
    client: &BlockingRpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    token_program: &Pubkey,
    dest: &Pubkey,
    mint_authority: &Keypair,
    amount: u64,
) {
    let ix = spl_token_2022::instruction::mint_to(
        token_program,
        mint,
        dest,
        &mint_authority.pubkey(),
        &[],
        amount,
    )
    .expect("build mint_to");
    submit(client, &payer.pubkey(), &[ix], &[payer, mint_authority]);
}

pub fn token_balance(client: &BlockingRpcClient, token_account: &Pubkey) -> u64 {
    client
        .get_token_account_balance(token_account)
        .expect("get_token_account_balance")
        .amount
        .parse()
        .unwrap()
}

/// Polls at `finalized` commitment until `token_account` reports exactly
/// `expected_amount`. Bootstrap transfers (e.g. funding the reserve vault)
/// are normally submitted and awaited at `confirmed` commitment for speed
/// (`blocking_client`), but `finalized` lags `confirmed` by ~32 slots on a
/// fresh single-node validator. Any reconciliation or ledger baseline that
/// reads the reserve at `finalized` before that lag has elapsed observes a
/// stale (pre-funding) balance — an unexplained drop from the ledger's
/// configured expectation that permanently pauses the reserve (reconcile
/// never auto-unpauses). Callers must wait here before configuring the
/// ledger's starting balance or starting the orchestrator against a
/// just-funded reserve.
pub async fn wait_for_finalized_balance(
    rpc: &RealSolanaRpc,
    token_account: &Pubkey,
    expected_amount: u64,
) {
    for _ in 0..100 {
        if let Ok(Some(account)) = rpc.get_account(token_account).await {
            if let Ok(amount) = accounts::decode_token_account_amount(&account.data) {
                if amount == expected_amount {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    panic!(
        "reserve token account {token_account} never reached finalized balance {expected_amount}"
    );
}

/// One-time program bootstrap: `initialize` then `initialize_reserve_vault`.
/// `authority` must be the validator's baked-in upgrade authority (see
/// [`LocalValidator::start`]) and becomes the initial `BridgeConfig.admin`.
#[allow(clippy::too_many_arguments)]
pub fn bootstrap_program(
    client: &BlockingRpcClient,
    authority: &Keypair,
    attestation_keys: &[Pubkey],
    threshold: u8,
    reserve_mint: &Pubkey,
    reserve_token_program: &Pubkey,
) {
    let init_ix = instructions::initialize(
        &authority.pubkey(),
        attestation_keys,
        threshold,
        3_600,           // governance_timelock_seconds
        1,               // min_transfer_amount
        10_000_000_000,  // per_transfer_limit
        0,               // protected_minimum
        100_000_000_000, // rolling_volume_limit
        3_600,           // rolling_window_seconds
        3_600,           // upgrade_timelock_seconds
    );
    submit(client, &authority.pubkey(), &[init_ix], &[authority]);

    let vault_ix = instructions::initialize_reserve_vault(
        &authority.pubkey(),
        reserve_mint,
        reserve_token_program,
    );
    submit(client, &authority.pubkey(), &[vault_ix], &[authority]);
}
