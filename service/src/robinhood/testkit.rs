//! Deterministic building blocks for the Robinhood tests: an ABI encoder
//! for `DepositCreated`, and a scriptable in-memory EVM.
//!
//! # Why an encoder rather than fixtures
//!
//! Every test here builds its logs by ENCODING the event the way the
//! contract would, then hands the bytes to the production decoder. A
//! hand-written hex fixture would prove only that the decoder is
//! self-consistent with whatever was pasted; encoding independently means
//! the round trip has to hold, and a test that wants a malformed log has
//! to say exactly which byte it is corrupting.
//!
//! # Why a mock chain rather than a mock RPC
//!
//! [`MockChain`] is a list of blocks, so a reorg in a test is what a
//! reorg is on a real chain — replacing a suffix with different blocks —
//! rather than a scripted sequence of RPC answers that happens to look
//! like one. That is what makes the reorg tests meaningful: they exercise
//! the indexer against a chain that genuinely changed, not against a
//! recording of the responses somebody expected it to make.
//!
//! No test in this crate contacts a real Robinhood endpoint.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::evm::hash::{EvmBlockHash, EvmTxHash};
use crate::evm::{EvmAddress, EvmChainId};
use crate::routes::Route;

use super::deposit_event::deposit_created_topic0;
use super::rpc::{EvmBlockRef, EvmLogFilter, EvmRawLog, EvmRpc, EvmRpcError, EvmTopic};

/// A stand-in bridge contract address. Distinct from [`TOKEN`] so the
/// "bridge is not the token" check has two real values to compare.
pub(crate) const BRIDGE: EvmAddress = EvmAddress::from_bytes([0x11; 20]);
pub(crate) const TOKEN: EvmAddress = EvmAddress::from_bytes([0x22; 20]);
pub(crate) const DEPOSITOR: EvmAddress = EvmAddress::from_bytes([0x33; 20]);

/// `GlcRobinhoodBridge::CANONICAL_SCALE`.
pub(crate) const CANONICAL_SCALE: u128 = 10_000_000_000;

/// A deterministic, collision-free block hash for `(number, fork)`.
///
/// `fork` is what makes a reorg expressible: the same height on two
/// different chain histories produces two different hashes, exactly as it
/// would on a real chain.
pub(crate) fn block_hash(number: u64, fork: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[..8].copy_from_slice(&number.to_be_bytes());
    out[31] = fork;
    out
}

pub(crate) fn tx_hash(seed: u64) -> [u8; 32] {
    let mut out = [0xaa; 32];
    out[..8].copy_from_slice(&seed.to_be_bytes());
    out
}

/// A left-padded 32-byte topic word.
fn topic_from_u128(value: u128) -> EvmTopic {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&value.to_be_bytes());
    out
}

fn topic_from_address(address: EvmAddress) -> EvmTopic {
    let mut out = [0u8; 32];
    out[12..].copy_from_slice(address.as_bytes());
    out
}

/// The parameters of one `DepositCreated` emission.
#[derive(Debug, Clone)]
pub(crate) struct DepositParams {
    pub obligation_index: u64,
    pub contract_route_id: u8,
    pub depositor: EvmAddress,
    pub destination: Vec<u8>,
    /// Robinhood atomic units (18 decimals).
    pub amount: u128,
    /// Defaults to `amount / CANONICAL_SCALE`; overridable so a test can
    /// make the two words disagree.
    pub canonical_amount: Option<u128>,
    pub block_number: u64,
    pub block_fork: u8,
    pub tx_seed: u64,
    pub log_index: u64,
}

impl DepositParams {
    /// A well-formed inbound RhnToGlc deposit of `whole` canonical units.
    pub(crate) fn valid(obligation_index: u64, block_number: u64, whole: u64) -> DepositParams {
        DepositParams {
            obligation_index,
            contract_route_id: Route::RhnToGlc
                .contract_route_id()
                .expect("RhnToGlc has a contract route id"),
            depositor: DEPOSITOR,
            destination: vec![0xde, 0xad, 0xbe, 0xef],
            amount: u128::from(whole) * CANONICAL_SCALE,
            canonical_amount: None,
            block_number,
            block_fork: 0,
            tx_seed: obligation_index,
            log_index: 0,
        }
    }
}

/// ABI-encodes the non-indexed tail: `(uint256 amount, uint256
/// canonicalAmount, bytes destination)`, exactly as Solidity would.
pub(crate) fn encode_deposit_data(
    amount: u128,
    canonical_amount: u128,
    destination: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&amount.to_be_bytes());
    data.extend_from_slice(&word);
    let mut word = [0u8; 32];
    word[16..].copy_from_slice(&canonical_amount.to_be_bytes());
    data.extend_from_slice(&word);
    // The dynamic-tail offset: always 0x60 for this parameter list.
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&96u64.to_be_bytes());
    data.extend_from_slice(&word);
    let mut word = [0u8; 32];
    word[24..].copy_from_slice(&(destination.len() as u64).to_be_bytes());
    data.extend_from_slice(&word);
    data.extend_from_slice(destination);
    let pad = (32 - destination.len() % 32) % 32;
    data.extend(std::iter::repeat_n(0u8, pad));
    data
}

/// Builds the raw log a node would return for these parameters.
pub(crate) fn deposit_log(params: &DepositParams) -> EvmRawLog {
    let canonical = params
        .canonical_amount
        .unwrap_or(params.amount / CANONICAL_SCALE);
    EvmRawLog {
        address: BRIDGE,
        topics: vec![
            deposit_created_topic0(),
            topic_from_u128(u128::from(params.obligation_index)),
            topic_from_address(params.depositor),
            topic_from_u128(u128::from(params.contract_route_id)),
        ],
        data: encode_deposit_data(params.amount, canonical, &params.destination),
        block_number: params.block_number,
        block_hash: EvmBlockHash::from_bytes(block_hash(params.block_number, params.block_fork)),
        tx_hash: EvmTxHash::from_bytes(tx_hash(params.tx_seed)),
        log_index: params.log_index,
        removed: false,
    }
}

/// One block in the mock chain.
#[derive(Debug, Clone, Default)]
pub(crate) struct MockBlock {
    pub hash: [u8; 32],
    pub logs: Vec<EvmRawLog>,
}

/// A scriptable EVM: a list of blocks, a chain id, and a queue of
/// failures to inject.
#[derive(Debug)]
pub(crate) struct MockChain {
    pub chain_id: EvmChainId,
    /// Index is the block number, so `blocks.len() - 1` is the head.
    pub blocks: Vec<MockBlock>,
    /// Popped one per RPC call, so a test can make a specific call fail.
    pub fail_next: VecDeque<EvmRpcError>,
    /// Every method name called, in order — how the "no Robinhood config
    /// means no Robinhood RPC call" property is actually asserted.
    pub calls: Vec<String>,
}

impl MockChain {
    /// A chain of `len` blocks (heights `0..len`) on fork 0, all empty.
    pub(crate) fn of_length(chain_id: EvmChainId, len: u64) -> MockChain {
        MockChain {
            chain_id,
            blocks: (0..len)
                .map(|n| MockBlock {
                    hash: block_hash(n, 0),
                    logs: Vec::new(),
                })
                .collect(),
            fail_next: VecDeque::new(),
            calls: Vec::new(),
        }
    }

    pub(crate) fn head(&self) -> u64 {
        self.blocks.len() as u64 - 1
    }

    /// Appends `count` empty blocks on fork 0.
    pub(crate) fn extend(&mut self, count: u64) {
        for _ in 0..count {
            let n = self.blocks.len() as u64;
            self.blocks.push(MockBlock {
                hash: block_hash(n, 0),
                logs: Vec::new(),
            });
        }
    }

    /// Puts a deposit in the block its parameters name, creating blocks up
    /// to that height if needed.
    pub(crate) fn add_deposit(&mut self, params: &DepositParams) {
        while self.blocks.len() as u64 <= params.block_number {
            let n = self.blocks.len() as u64;
            self.blocks.push(MockBlock {
                hash: block_hash(n, 0),
                logs: Vec::new(),
            });
        }
        let block = &mut self.blocks[params.block_number as usize];
        block.hash = block_hash(params.block_number, params.block_fork);
        block.logs.push(deposit_log(params));
    }

    /// Replaces every block above `fork_block` with `new_len` fresh,
    /// empty blocks on a different fork — a reorg.
    pub(crate) fn reorg_from(&mut self, fork_block: u64, new_len: u64, fork: u8) {
        self.blocks.truncate(fork_block as usize + 1);
        while self.blocks.len() as u64 <= new_len {
            let n = self.blocks.len() as u64;
            self.blocks.push(MockBlock {
                hash: block_hash(n, fork),
                logs: Vec::new(),
            });
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MockEvmRpc {
    pub state: Arc<Mutex<MockChain>>,
}

impl MockEvmRpc {
    pub(crate) fn new(chain: MockChain) -> MockEvmRpc {
        MockEvmRpc {
            state: Arc::new(Mutex::new(chain)),
        }
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut MockChain) -> T) -> T {
        let mut guard = self.state.lock().expect("mock chain lock");
        f(&mut guard)
    }

    /// Records the call and returns an injected failure if one is queued.
    fn enter(&self, method: &str) -> Option<EvmRpcError> {
        self.with(|chain| {
            chain.calls.push(method.to_string());
            chain.fail_next.pop_front()
        })
    }
}

impl EvmRpc for MockEvmRpc {
    async fn chain_id(&self) -> Result<EvmChainId, EvmRpcError> {
        if let Some(e) = self.enter("eth_chainId") {
            return Err(e);
        }
        Ok(self.with(|chain| chain.chain_id))
    }

    async fn block_number(&self) -> Result<u64, EvmRpcError> {
        if let Some(e) = self.enter("eth_blockNumber") {
            return Err(e);
        }
        Ok(self.with(|chain| chain.head()))
    }

    async fn block_by_number(&self, number: u64) -> Result<Option<EvmBlockRef>, EvmRpcError> {
        if let Some(e) = self.enter("eth_getBlockByNumber") {
            return Err(e);
        }
        Ok(self.with(|chain| {
            chain.blocks.get(number as usize).map(|block| EvmBlockRef {
                number,
                hash: EvmBlockHash::from_bytes(block.hash),
                parent_hash: EvmBlockHash::from_bytes(
                    number
                        .checked_sub(1)
                        .and_then(|p| chain.blocks.get(p as usize))
                        .map(|p| p.hash)
                        .unwrap_or([0u8; 32]),
                ),
                timestamp: 1_700_000_000 + number,
            })
        }))
    }

    async fn logs(&self, filter: &EvmLogFilter) -> Result<Vec<EvmRawLog>, EvmRpcError> {
        if let Some(e) = self.enter("eth_getLogs") {
            return Err(e);
        }
        Ok(self.with(|chain| {
            let mut out = Vec::new();
            for number in filter.from_block..=filter.to_block {
                let Some(block) = chain.blocks.get(number as usize) else {
                    continue;
                };
                for log in &block.logs {
                    if log.address == filter.address && log.topics.first() == Some(&filter.topic0) {
                        out.push(log.clone());
                    }
                }
            }
            out
        }))
    }
}

/// A config pointed at [`BRIDGE`], with everything else spelled out so a
/// test that cares about one value changes only that one.
pub(crate) fn test_config(
    chain_id: EvmChainId,
    start_block: u64,
    confirmation_depth: u64,
    max_log_block_range: u64,
) -> super::config::RobinhoodIndexerConfig {
    super::config::RobinhoodIndexerConfig::new(
        "https://rpc.example.invalid".to_string(),
        chain_id,
        BRIDGE,
        TOKEN,
        start_block,
        confirmation_depth,
        1_000,
        5_000,
        max_log_block_range,
    )
    .expect("test config is valid")
}

// =====================================================================
// Phase F: a settlement-capable mock node
// =====================================================================

use std::collections::HashMap;

use crate::evm::abi;
use crate::evm::secp::EvmSecretKey;
use crate::evm::{EvmU256, TxEnvelope};

use super::auth::ProtocolChainPair;
use super::calls;
use super::preflight::VerifiedDeployment;
use super::rpc::{EvmBlockTag, EvmBroadcastOutcome, EvmCall, EvmCallRpc, EvmReceipt, EvmSubmitRpc};
use super::settlement_config::RobinhoodSettlementConfig;

pub(crate) const PROTOCOL_GOLDCOIN: u64 = 1001;
pub(crate) const PROTOCOL_ROBINHOOD: u64 = 2001;

/// The three authorization signer keys the tests use. Deterministic, so
/// every test's quorum recovers to the same three addresses.
pub(crate) fn signer_key(index: u8) -> EvmSecretKey {
    let mut bytes = [0u8; 32];
    bytes[30] = 0xa0;
    bytes[31] = index + 1;
    EvmSecretKey::from_bytes(&bytes).expect("a deterministic test key")
}

/// The submitter key. Deliberately unrelated to any signer key — the
/// config refuses a deployment where one account plays both roles.
pub(crate) fn submitter_key() -> EvmSecretKey {
    let mut bytes = [0u8; 32];
    bytes[30] = 0x5b;
    bytes[31] = 0x01;
    EvmSecretKey::from_bytes(&bytes).expect("a deterministic test key")
}

pub(crate) fn signer_addresses() -> [EvmAddress; 3] {
    [
        signer_key(0).address(),
        signer_key(1).address(),
        signer_key(2).address(),
    ]
}

/// The scriptable contract state a settlement test runs against.
///
/// Every field is something the production code reads over `eth_call`
/// before it will broadcast, so a test that wants to exercise a gate sets
/// exactly one of them and changes nothing else.
#[derive(Debug, Clone)]
pub(crate) struct MockContract {
    pub token: EvmAddress,
    pub token_decimals: u8,
    pub bridge_code: Vec<u8>,
    pub token_code: Vec<u8>,
    pub protocol_id: [u8; 32],
    pub signer_epoch: u64,
    /// `governanceNonce()` — the nonce the next governance action must
    /// carry. Advances by one on every accepted governance call.
    pub governance_nonce: EvmU256,
    pub signers: [EvmAddress; 3],
    pub migrated: bool,
    pub deposits_paused: bool,
    pub payouts_paused: bool,
    pub route_enabled: HashMap<u8, bool>,
    pub obligation_count: u64,
    /// `(depositor, status, route, amount)` per obligation index.
    pub obligations: HashMap<u64, calls::Obligation>,
    /// `(action, requestId)` pairs the contract has consumed.
    pub executed: Vec<(u8, [u8; 32])>,
    pub encumbered_reserve: EvmU256,
    /// `limits()`, in Robinhood 18-decimal atomic units.
    pub limits: calls::BridgeLimits,
    /// `inboundWindow()` / `outboundWindow()`.
    pub inbound_window: calls::RollingWindow,
    pub outbound_window: calls::RollingWindow,
    /// `None` = a pre-London chain with no EIP-1559 fee market.
    pub base_fee: Option<u128>,
    pub gas_price: u128,
    pub priority_fee: Option<u128>,
    pub submitter_balance: u128,
    /// `None` = `eth_estimateGas` reverts, i.e. the dry run fails.
    pub gas_estimate: Option<u64>,
    /// The domain separator this "deployment" reports. Set from the real
    /// computed one by [`MockNode::new`]; a test that wants a mismatch
    /// overwrites it.
    pub domain_separator: [u8; 32],
    /// `balanceOf(holder)` per holder, in Robinhood 18-decimal atomic
    /// units. An absent holder reads as zero, which is what a real ERC-20
    /// returns for an address that never received a transfer. Set through
    /// [`MockNode::set_token_balance`].
    pub token_balances: HashMap<EvmAddress, EvmU256>,
}

impl MockContract {
    pub(crate) fn healthy(bridge: EvmAddress) -> MockContract {
        let mut route_enabled = HashMap::new();
        route_enabled.insert(0x01, true);
        route_enabled.insert(0x02, true);
        route_enabled.insert(0x03, false);
        route_enabled.insert(0x04, false);
        MockContract {
            token: TOKEN,
            token_decimals: 18,
            bridge_code: vec![0x60, 0x80, 0x60, 0x40],
            token_code: vec![0x60, 0x80],
            token_balances: HashMap::new(),
            protocol_id: calls::bridge_protocol_id(),
            signer_epoch: 7,
            governance_nonce: EvmU256::from_u64(0),
            signers: signer_addresses(),
            migrated: false,
            deposits_paused: false,
            payouts_paused: false,
            route_enabled,
            obligation_count: 0,
            obligations: HashMap::new(),
            executed: Vec::new(),
            encumbered_reserve: EvmU256::ZERO,
            // A plausible sized deployment: 1..10_000 GLC per transfer
            // each way, 100_000 GLC per 24h bucket, 1_000 GLC floor.
            limits: calls::BridgeLimits {
                inbound_min: glc(1),
                inbound_max: glc(10_000),
                inbound_rolling_limit: glc(100_000),
                outbound_min: glc(1),
                outbound_max: glc(10_000),
                outbound_rolling_limit: glc(100_000),
                protected_min_reserve: glc(1_000),
            },
            inbound_window: calls::RollingWindow {
                window_start: 1_700_000_000,
                total: glc(250),
            },
            outbound_window: calls::RollingWindow {
                window_start: 1_700_000_000,
                total: glc(400),
            },
            base_fee: Some(1_000_000_000),
            gas_price: 1_500_000_000,
            priority_fee: Some(1_000_000_000),
            submitter_balance: 1_000_000_000_000_000_000,
            gas_estimate: Some(150_000),
            domain_separator: [0u8; 32],
            // overwritten by MockNode::new
        }
        .with_domain(bridge)
    }

    fn with_domain(mut self, bridge: EvmAddress) -> MockContract {
        self.domain_separator =
            super::auth::BridgeDomain::new(EvmChainId::new(4663).unwrap(), bridge).separator();
        self
    }
}

/// One broadcast this node received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BroadcastRecord {
    pub raw: Vec<u8>,
    pub tx_hash: [u8; 32],
    pub nonce: u64,
}

/// What the node should do with the next `eth_sendRawTransaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SendBehaviour {
    Accept,
    AlreadyKnown,
    NonceTooLow,
    ReplacementUnderpriced,
    Reject {
        code: i64,
        message: String,
    },
    /// The transport fails: the question is never answered, and the
    /// caller cannot know whether the bytes arrived.
    Transport,
}

/// A scriptable EVM node that can serve contract reads and accept
/// broadcasts.
///
/// Wraps its state in an `Arc<Mutex<..>>` so a test can mutate the
/// contract mid-run — flip a pause, rotate the epoch, mine a block —
/// while the production code holds an immutable reference to the client,
/// which is how a real chain behaves.
#[derive(Debug, Clone)]
pub(crate) struct MockNode {
    pub bridge: EvmAddress,
    pub chain_id: EvmChainId,
    pub state: Arc<Mutex<MockNodeState>>,
}

#[derive(Debug)]
pub(crate) struct MockNodeState {
    pub contract: MockContract,
    pub head: u64,
    /// The submitter's `eth_getTransactionCount(.., "pending")`.
    pub pending_nonce: u64,
    pub broadcasts: Vec<BroadcastRecord>,
    pub send_behaviour: VecDeque<SendBehaviour>,
    /// `tx_hash -> receipt`. Absent means "not mined yet".
    pub receipts: HashMap<[u8; 32], EvmReceipt>,
    /// Every JSON-RPC method invoked, in order.
    pub calls: Vec<String>,
    /// When `Some`, every `eth_call` fails with this transport error —
    /// the "the endpoint is there but the read did not succeed" case a
    /// fail-closed reader has to handle without inventing a value. Set
    /// through [`MockNode::fail_calls`].
    pub call_failure: Option<String>,
    /// When `Some`, `eth_blockNumber` fails with this transport error.
    pub head_failure: Option<String>,
}

impl MockNode {
    pub(crate) fn new(bridge: EvmAddress) -> MockNode {
        MockNode {
            bridge,
            chain_id: EvmChainId::new(4663).unwrap(),
            state: Arc::new(Mutex::new(MockNodeState {
                contract: MockContract::healthy(bridge),
                head: 100,
                pending_nonce: 0,
                broadcasts: Vec::new(),
                send_behaviour: VecDeque::new(),
                receipts: HashMap::new(),
                calls: Vec::new(),
                call_failure: None,
                head_failure: None,
            })),
        }
    }

    pub(crate) fn with<T>(&self, f: impl FnOnce(&mut MockNodeState) -> T) -> T {
        f(&mut self.state.lock().expect("mock node lock"))
    }

    /// Credits `holder` with `robinhood_atomic` 18-decimal units of the
    /// mock token.
    pub(crate) fn set_token_balance(&self, holder: EvmAddress, robinhood_atomic: u128) {
        self.with(|s| {
            s.contract
                .token_balances
                .insert(holder, EvmU256::from_u128(robinhood_atomic))
        });
    }

    /// Makes every subsequent `eth_call` fail as a transport error.
    pub(crate) fn fail_calls(&self, message: &str) {
        self.with(|s| s.call_failure = Some(message.to_string()));
    }

    /// Makes every subsequent `eth_blockNumber` fail as a transport error.
    pub(crate) fn fail_head(&self, message: &str) {
        self.with(|s| s.head_failure = Some(message.to_string()));
    }

    /// Clears the injected `eth_call` failure.
    pub(crate) fn heal_calls(&self) {
        self.with(|s| s.call_failure = None);
    }

    /// The settlement configuration matching this node.
    pub(crate) fn settlement_config(&self) -> RobinhoodSettlementConfig {
        RobinhoodSettlementConfig::new(
            Some(&self.indexer_config()),
            self.chain_id,
            self.bridge,
            TxEnvelope::Eip1559,
            "GLC_RHN_TEST_SUBMITTER_KEY".to_string(),
            submitter_key().address(),
            signer_addresses(),
            900,
            3,
            130,
            500_000,
            100_000_000_000,
            1_000_000_000,
            120,
            3,
            1_000,
        )
        .expect("a well-formed test settlement config")
    }

    pub(crate) fn indexer_config(&self) -> super::RobinhoodIndexerConfig {
        super::RobinhoodIndexerConfig::new(
            "https://rpc.test.invalid".to_string(),
            self.chain_id,
            self.bridge,
            TOKEN,
            0,
            12,
            1_000,
            5_000,
            1_000,
        )
        .expect("a well-formed test indexer config")
    }

    /// The verified deployment this node would produce — built directly
    /// rather than through `preflight::verify`, so a settlement test does
    /// not have to satisfy preflight as a precondition.
    pub(crate) fn verified_deployment(&self) -> VerifiedDeployment {
        let contract = self.with(|s| s.contract.clone());
        VerifiedDeployment {
            chain_id: self.chain_id,
            bridge_contract: self.bridge,
            token: contract.token,
            token_decimals: contract.token_decimals,
            signers: contract.signers,
            domain_separator: contract.domain_separator,
            glc_to_rhn_chains: ProtocolChainPair {
                source: PROTOCOL_GOLDCOIN,
                dest: PROTOCOL_ROBINHOOD,
            },
            rhn_to_glc_chains: ProtocolChainPair {
                source: PROTOCOL_ROBINHOOD,
                dest: PROTOCOL_GOLDCOIN,
            },
            tx_envelope: TxEnvelope::Eip1559,
            chain_has_base_fee: contract.base_fee.is_some(),
        }
    }

    /// Mines the transaction at `tx_hash` in `block`, with `success` as
    /// its receipt status and one log from the bridge contract — the
    /// event-presence check the production code performs.
    pub(crate) fn mine(&self, tx_hash: [u8; 32], block: u64, success: bool) {
        let bridge = self.bridge;
        self.with(|s| {
            s.head = s.head.max(block);
            s.receipts.insert(
                tx_hash,
                EvmReceipt {
                    tx_hash: EvmTxHash::from_bytes(tx_hash),
                    success,
                    block_number: block,
                    block_hash: EvmBlockHash::from_bytes(block_hash(block, 0)),
                    gas_used: 100_000,
                    logs: if success {
                        vec![EvmRawLog {
                            address: bridge,
                            topics: vec![[0x99u8; 32]],
                            data: Vec::new(),
                            block_number: block,
                            block_hash: EvmBlockHash::from_bytes(block_hash(block, 0)),
                            tx_hash: EvmTxHash::from_bytes(tx_hash),
                            log_index: 0,
                            removed: false,
                        }]
                    } else {
                        Vec::new()
                    },
                },
            );
        });
    }

    /// Marks `(action, requestId)` consumed, as the contract's own replay
    /// guard would after a successful execution.
    pub(crate) fn mark_executed(&self, action: u8, request_id: [u8; 32]) {
        self.with(|s| s.contract.executed.push((action, request_id)));
    }

    fn record(&self, method: &str) {
        self.with(|s| s.calls.push(method.to_string()));
    }
}

/// The nonce an already-signed raw transaction carries, recovered by
/// re-deriving it from the broadcast list's position.
///
/// The mock does not decode RLP (this crate has no decoder, deliberately)
/// — the nonce is taken from the ledger row that produced the broadcast,
/// which the test supplies.
/// Whole GLC as a Robinhood 18-decimal atomic word.
fn glc(whole: u64) -> EvmU256 {
    EvmU256::from_u128(u128::from(whole) * 1_000_000_000_000_000_000)
}

/// A `Window` struct's two static fields, returned inline.
fn encode_window(window: calls::RollingWindow) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&EvmU256::from_u64(window.window_start).to_be_bytes());
    out.extend_from_slice(&window.total.to_be_bytes());
    out
}

fn word(value: u128) -> Vec<u8> {
    abi::word_u128(value).to_vec()
}

fn word_bool(value: bool) -> Vec<u8> {
    abi::word_bool(value).to_vec()
}

fn word_address(value: EvmAddress) -> Vec<u8> {
    abi::word_address(value).to_vec()
}

impl EvmCallRpc for MockNode {
    async fn call(&self, call: &EvmCall, _block: EvmBlockTag) -> Result<Vec<u8>, EvmRpcError> {
        self.record("eth_call");
        if let Some(message) = self.with(|s| s.call_failure.clone()) {
            return Err(EvmRpcError::Transport(message));
        }
        if let Some(e) = self.with(|s| s.contract.bridge_code.is_empty()).then(|| {
            // An `eth_call` to an address with no code returns EMPTY DATA
            // rather than failing — the exact behaviour the preflight's
            // separate `eth_getCode` check exists to disambiguate.
            EvmRpcError::Malformed("no code".into())
        }) {
            let _ = e;
            return Ok(Vec::new());
        }
        let selector: [u8; 4] = call.data[..4].try_into().expect("a selector");
        let sel = |sig: &str| abi::selector(sig) == selector;
        let contract = self.with(|s| s.contract.clone());

        if call.to == contract.token {
            if sel(calls::SIG_ERC20_DECIMALS) {
                return Ok(word(u128::from(contract.token_decimals)));
            }
            if sel(calls::SIG_ERC20_BALANCE_OF) {
                // The holder is the single argument word; a real ERC-20
                // answers per address, and a reserve reconciliation test
                // is meaningless if every holder reads the same.
                let holder = EvmAddress::from_bytes(
                    call.data[16..36].try_into().expect("an address argument"),
                );
                let balance = contract
                    .token_balances
                    .get(&holder)
                    .copied()
                    .unwrap_or(EvmU256::ZERO);
                return Ok(balance.to_be_bytes().to_vec());
            }
        }

        if sel(calls::SIG_TOKEN) {
            return Ok(word_address(contract.token));
        }
        if sel(calls::SIG_BRIDGE_PROTOCOL_ID) {
            return Ok(contract.protocol_id.to_vec());
        }
        if sel(calls::SIG_DOMAIN_SEPARATOR) {
            return Ok(contract.domain_separator.to_vec());
        }
        if sel(calls::SIG_SIGNER_EPOCH) {
            return Ok(word(u128::from(contract.signer_epoch)));
        }
        if sel(crate::robinhood::governance::SIG_GOVERNANCE_NONCE) {
            return Ok(contract.governance_nonce.to_be_bytes().to_vec());
        }
        if sel(calls::SIG_MIGRATED) {
            return Ok(word_bool(contract.migrated));
        }
        if sel(calls::SIG_DEPOSITS_PAUSED) {
            return Ok(word_bool(contract.deposits_paused));
        }
        if sel(calls::SIG_PAYOUTS_PAUSED) {
            return Ok(word_bool(contract.payouts_paused));
        }
        if sel(calls::SIG_LIMITS) {
            // A struct of seven static fields returns as seven words
            // inline, in declaration order.
            let mut out = Vec::with_capacity(7 * 32);
            for value in [
                contract.limits.inbound_min,
                contract.limits.inbound_max,
                contract.limits.inbound_rolling_limit,
                contract.limits.outbound_min,
                contract.limits.outbound_max,
                contract.limits.outbound_rolling_limit,
                contract.limits.protected_min_reserve,
            ] {
                out.extend_from_slice(&value.to_be_bytes());
            }
            return Ok(out);
        }
        if sel(calls::SIG_INBOUND_WINDOW) {
            return Ok(encode_window(contract.inbound_window));
        }
        if sel(calls::SIG_OUTBOUND_WINDOW) {
            return Ok(encode_window(contract.outbound_window));
        }
        if sel(calls::SIG_ENCUMBERED_RESERVE) {
            return Ok(contract.encumbered_reserve.to_be_bytes().to_vec());
        }
        if sel(calls::SIG_OBLIGATION_COUNT) {
            return Ok(word(u128::from(contract.obligation_count)));
        }
        if sel(calls::SIG_SIGNERS) {
            let mut out = Vec::with_capacity(96);
            for signer in contract.signers {
                out.extend_from_slice(&abi::word_address(signer));
            }
            return Ok(out);
        }
        if sel(calls::SIG_ROUTE_ENABLED) || sel(calls::SIG_IS_ROUTE_LIVE) {
            let route = call.data[4 + 31];
            let enabled = contract.route_enabled.get(&route).copied().unwrap_or(false);
            if sel(calls::SIG_IS_ROUTE_LIVE) {
                let inbound = matches!(route, 0x02 | 0x04);
                let paused = if inbound {
                    contract.deposits_paused
                } else {
                    contract.payouts_paused
                };
                return Ok(word_bool(enabled && !paused && !contract.migrated));
            }
            return Ok(word_bool(enabled));
        }
        if sel(calls::SIG_ROUTE_CHAINS) {
            let route = call.data[4 + 31];
            let (source, dest) = match route {
                0x01 => (PROTOCOL_GOLDCOIN, PROTOCOL_ROBINHOOD),
                0x02 => (PROTOCOL_ROBINHOOD, PROTOCOL_GOLDCOIN),
                0x03 => (3001, PROTOCOL_ROBINHOOD),
                _ => (PROTOCOL_ROBINHOOD, 3001),
            };
            let mut out = word(u128::from(source));
            out.extend_from_slice(&abi::word_u128(u128::from(dest)));
            return Ok(out);
        }
        if sel(calls::SIG_OBLIGATION) {
            let index = EvmU256::from_be_bytes(call.data[4..36].try_into().unwrap())
                .try_to_u64()
                .unwrap_or(u64::MAX);
            let ob = contract
                .obligations
                .get(&index)
                .copied()
                .unwrap_or(calls::Obligation {
                    depositor: EvmAddress::ZERO,
                    status: calls::OBLIGATION_STATUS_NONE,
                    route: 0,
                    amount: EvmU256::ZERO,
                });
            let mut out = word_address(ob.depositor);
            out.extend_from_slice(&abi::word_u128(u128::from(ob.status)));
            out.extend_from_slice(&abi::word_u128(u128::from(ob.route)));
            out.extend_from_slice(&ob.amount.to_be_bytes());
            return Ok(out);
        }
        if sel(calls::SIG_REQUEST_EXECUTED) {
            let action = call.data[4 + 31];
            let request_id: [u8; 32] = call.data[36..68].try_into().expect("a bytes32");
            let executed = contract
                .executed
                .iter()
                .any(|(a, r)| *a == action && *r == request_id);
            return Ok(word_bool(executed));
        }
        Err(EvmRpcError::Method {
            code: -32000,
            message: format!("mock node has no handler for selector {selector:02x?}"),
        })
    }

    async fn code_at(
        &self,
        address: EvmAddress,
        _block: EvmBlockTag,
    ) -> Result<Vec<u8>, EvmRpcError> {
        self.record("eth_getCode");
        let contract = self.with(|s| s.contract.clone());
        Ok(if address == self.bridge {
            contract.bridge_code
        } else if address == contract.token {
            contract.token_code
        } else {
            Vec::new()
        })
    }
}

impl EvmSubmitRpc for MockNode {
    async fn pending_nonce(&self, _address: EvmAddress) -> Result<u64, EvmRpcError> {
        self.record("eth_getTransactionCount");
        Ok(self.with(|s| s.pending_nonce))
    }

    async fn balance(&self, _address: EvmAddress) -> Result<EvmU256, EvmRpcError> {
        self.record("eth_getBalance");
        Ok(EvmU256::from_u128(
            self.with(|s| s.contract.submitter_balance),
        ))
    }

    async fn estimate_gas(&self, _from: EvmAddress, _call: &EvmCall) -> Result<u64, EvmRpcError> {
        self.record("eth_estimateGas");
        self.with(|s| s.contract.gas_estimate)
            .ok_or_else(|| EvmRpcError::Method {
                code: 3,
                message: "execution reverted".to_string(),
            })
    }

    async fn gas_price(&self) -> Result<u128, EvmRpcError> {
        self.record("eth_gasPrice");
        Ok(self.with(|s| s.contract.gas_price))
    }

    async fn max_priority_fee_per_gas(&self) -> Result<Option<u128>, EvmRpcError> {
        self.record("eth_maxPriorityFeePerGas");
        Ok(self.with(|s| s.contract.priority_fee))
    }

    async fn latest_base_fee(&self) -> Result<Option<u128>, EvmRpcError> {
        self.record("eth_getBlockByNumber");
        Ok(self.with(|s| s.contract.base_fee))
    }

    async fn send_raw_transaction(&self, raw: &[u8]) -> Result<EvmBroadcastOutcome, EvmRpcError> {
        self.record("eth_sendRawTransaction");
        let behaviour = self
            .with(|s| s.send_behaviour.pop_front())
            .unwrap_or(SendBehaviour::Accept);
        let tx_hash = crate::evm::keccak256(raw);
        match behaviour {
            SendBehaviour::Accept => {
                self.with(|s| {
                    s.broadcasts.push(BroadcastRecord {
                        raw: raw.to_vec(),
                        tx_hash,
                        nonce: s.pending_nonce,
                    })
                });
                Ok(EvmBroadcastOutcome::Accepted {
                    tx_hash: EvmTxHash::from_bytes(tx_hash),
                })
            }
            SendBehaviour::AlreadyKnown => Ok(EvmBroadcastOutcome::AlreadyKnown),
            SendBehaviour::NonceTooLow => Ok(EvmBroadcastOutcome::NonceTooLow),
            SendBehaviour::ReplacementUnderpriced => {
                Ok(EvmBroadcastOutcome::ReplacementUnderpriced)
            }
            SendBehaviour::Reject { code, message } => {
                Ok(EvmBroadcastOutcome::Rejected { code, message })
            }
            SendBehaviour::Transport => Err(EvmRpcError::Transport(
                "the mock node dropped the connection mid-send".to_string(),
            )),
        }
    }

    async fn transaction_receipt(
        &self,
        tx_hash: EvmTxHash,
    ) -> Result<Option<EvmReceipt>, EvmRpcError> {
        self.record("eth_getTransactionReceipt");
        Ok(self.with(|s| s.receipts.get(tx_hash.as_bytes()).cloned()))
    }
}

impl EvmRpc for MockNode {
    async fn chain_id(&self) -> Result<EvmChainId, EvmRpcError> {
        self.record("eth_chainId");
        Ok(self.chain_id)
    }

    async fn block_number(&self) -> Result<u64, EvmRpcError> {
        self.record("eth_blockNumber");
        if let Some(message) = self.with(|s| s.head_failure.clone()) {
            return Err(EvmRpcError::Transport(message));
        }
        Ok(self.with(|s| s.head))
    }

    async fn block_by_number(&self, number: u64) -> Result<Option<EvmBlockRef>, EvmRpcError> {
        self.record("eth_getBlockByNumber");
        Ok(Some(EvmBlockRef {
            number,
            hash: EvmBlockHash::from_bytes(block_hash(number, 0)),
            parent_hash: EvmBlockHash::from_bytes(block_hash(number.saturating_sub(1), 0)),
            timestamp: 1_780_000_000 + number,
        }))
    }

    async fn logs(&self, _filter: &EvmLogFilter) -> Result<Vec<EvmRawLog>, EvmRpcError> {
        self.record("eth_getLogs");
        Ok(Vec::new())
    }
}
