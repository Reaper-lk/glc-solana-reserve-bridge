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
