//! Does the DEPLOYED Solana program support the instructions this client
//! sends? A startup/preflight probe against the live program bytes, so
//! client/program drift is detected and reported instead of discovered
//! by a failing simulation.
//!
//! # The incident this exists for
//!
//! `glc-admin refund-manual-review` simulated its `refund_withdraw`
//! transaction against production on 2026-09-13 and failed with Anchor
//! error 101, `InstructionFallbackNotFound`: the deployed program
//! (`6tmLSP2j…`, last deployed slot 442,649,805, 2026-08-29) predates
//! the 2026-09-02 withdrawal hardening that added `refund_withdraw` and
//! `treasury_withdraw` and retired `rebalance_withdraw`. The client had
//! been built from the newer source for eleven days. Nothing compared
//! the two; every readiness surface claimed a refund capability the
//! chain did not have.
//!
//! # How the probe works
//!
//! The program is a BPF upgradeable-loader program, so its account holds
//! only a pointer to its `ProgramData` account, whose data is a 45-byte
//! header (`slot`, `upgrade_authority`) followed by the ELF. Anchor
//! dispatches on the 8-byte discriminator `sha256("global:<name>")[..8]`
//! — the same derivation [`super::instructions::discriminator`] uses to
//! BUILD every instruction — and those eight bytes are compiled into the
//! program as the immediate of an `lddw` (a 64-bit load split across two
//! 8-byte slots: `[0x18, reg, off, imm_lo] [0x00, 0, 0, 0, imm_hi]`).
//! [`elf_contains_discriminator`] looks for exactly that encoding, and
//! for the eight bytes contiguous (a compiler may equally place them in
//! `.rodata`). A discriminator that appears in neither form is not
//! dispatched by the program, and the instruction is unsupported.
//!
//! This is a necessary check, not a sufficient one: it proves the
//! program KNOWS the instruction, not that its account list or argument
//! layout match. Simulation still runs before every broadcast. What the
//! probe adds is that the answer is available BEFORE any transaction is
//! prepared, continuously, on every readiness surface.
//!
//! # What it feeds
//!
//! - `GET /status → solana_refund_supported` and `/chains`'
//!   per-route `refund_supported`;
//! - the `/health` invariant `solana_refund_instruction_supported` and
//!   the `glc_solana_refund_supported` gauge;
//! - the admin API's readiness context (a critical banner in the UI);
//! - `refund-manual-review` / the admin API's refund endpoints, which
//!   REFUSE before preparing anything when the probe says no
//!   ([`super::refund::build_refund_plan`]);
//! - `glc-admin solana-program-compat`, the on-demand report.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;

use super::accounts::PROGRAM_ID;
use super::instructions::discriminator;
use super::rpc::{SolanaRpc, SolanaRpcError};

/// The instruction every refund path sends. Its absence is what
/// `solana_refund_supported = false` means.
pub const REFUND_INSTRUCTION: &str = "refund_withdraw";

/// Every instruction this service (daemon + glc-admin) can build against
/// the program. Probed together so the compat report shows the whole
/// drift, not just the refund half.
pub const CLIENT_INSTRUCTIONS: &[&str] = &[
    "release_from_reserve",
    "deposit_to_reserve",
    "record_goldcoin_completion",
    "set_paused",
    "set_limit",
    "reset_rolling_volume_window",
    "refund_withdraw",
    "treasury_withdraw",
    "initialize_rebalance_policy",
    "rebalance_withdraw",
];

/// `UpgradeableLoaderState::Program` — `[u32 tag = 2][programdata: 32]`.
const PROGRAM_ACCOUNT_LEN: usize = 4 + 32;
const PROGRAM_TAG: u32 = 2;
/// `UpgradeableLoaderState::ProgramData` — `[u32 tag = 3][slot: u64]
/// [Option<Pubkey>: 1 + 32]`, then the ELF.
const PROGRAMDATA_HEADER_LEN: usize = 4 + 8 + 1 + 32;
const PROGRAMDATA_TAG: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum ProgramCompatError {
    #[error("reading the program account: {0}")]
    Rpc(#[from] SolanaRpcError),
    #[error("program {0} does not exist on this cluster")]
    ProgramMissing(Pubkey),
    #[error(
        "program {0} is not an upgradeable-loader program (account data {1} bytes, tag {2:?})"
    )]
    NotUpgradeable(Pubkey, usize, Option<u32>),
    #[error("ProgramData {0} does not exist")]
    ProgramDataMissing(Pubkey),
    #[error("ProgramData {0} is malformed ({1} bytes, tag {2:?})")]
    ProgramDataMalformed(Pubkey, usize, Option<u32>),
}

/// What the deployed program supports, as read from its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramCompat {
    pub program_id: Pubkey,
    pub programdata_address: Pubkey,
    pub last_deployed_slot: u64,
    pub upgrade_authority: Option<Pubkey>,
    /// Length of the ELF (the ProgramData minus its header).
    pub program_len: usize,
    /// SHA-256 of the ELF bytes — comparable to `sha256sum` of a
    /// `solana program dump` or of a reproducible `anchor build`.
    pub program_sha256: [u8; 32],
    /// `instruction name -> dispatched by the deployed program`.
    pub instructions: BTreeMap<&'static str, bool>,
}

impl ProgramCompat {
    pub fn supports(&self, instruction: &str) -> bool {
        self.instructions.get(instruction).copied().unwrap_or(false)
    }

    /// The one question every refund surface asks.
    pub fn refund_supported(&self) -> bool {
        self.supports(REFUND_INSTRUCTION)
    }

    /// Client instructions the deployed program does not dispatch.
    pub fn missing(&self) -> Vec<&'static str> {
        self.instructions
            .iter()
            .filter(|(_, present)| !**present)
            .map(|(name, _)| *name)
            .collect()
    }

    pub fn program_sha256_hex(&self) -> String {
        self.program_sha256
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

/// Whether `elf` dispatches `disc` — see the module docs for the two
/// encodings accepted.
pub fn elf_contains_discriminator(elf: &[u8], disc: [u8; 8]) -> bool {
    if elf.windows(8).any(|w| w == disc) {
        return true;
    }
    // lddw: imm_lo at i, four zero bytes, imm_hi at i+8; the slot's
    // opcode byte sits four bytes before imm_lo.
    let (lo, hi) = disc.split_at(4);
    elf.windows(12).enumerate().any(|(i, w)| {
        &w[0..4] == lo && w[4..8] == [0, 0, 0, 0] && &w[8..12] == hi && i >= 4 && elf[i - 4] == 0x18
    })
}

/// Reads the deployed program and answers for every client instruction.
pub async fn probe<R: SolanaRpc>(rpc: &R) -> Result<ProgramCompat, ProgramCompatError> {
    probe_program(rpc, PROGRAM_ID).await
}

pub async fn probe_program<R: SolanaRpc>(
    rpc: &R,
    program_id: Pubkey,
) -> Result<ProgramCompat, ProgramCompatError> {
    let program = rpc
        .get_account(&program_id)
        .await?
        .ok_or(ProgramCompatError::ProgramMissing(program_id))?;
    let tag = program
        .data
        .get(0..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if program.data.len() != PROGRAM_ACCOUNT_LEN || tag != Some(PROGRAM_TAG) {
        return Err(ProgramCompatError::NotUpgradeable(
            program_id,
            program.data.len(),
            tag,
        ));
    }
    let programdata_address = Pubkey::new_from_array(
        <[u8; 32]>::try_from(&program.data[4..36]).expect("32 bytes checked above"),
    );
    let programdata = rpc
        .get_account(&programdata_address)
        .await?
        .ok_or(ProgramCompatError::ProgramDataMissing(programdata_address))?;
    let tag = programdata
        .data
        .get(0..4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if programdata.data.len() < PROGRAMDATA_HEADER_LEN || tag != Some(PROGRAMDATA_TAG) {
        return Err(ProgramCompatError::ProgramDataMalformed(
            programdata_address,
            programdata.data.len(),
            tag,
        ));
    }
    let d = &programdata.data;
    let last_deployed_slot = u64::from_le_bytes(<[u8; 8]>::try_from(&d[4..12]).expect("8 bytes"));
    let upgrade_authority = match d[12] {
        1 => Some(Pubkey::new_from_array(
            <[u8; 32]>::try_from(&d[13..45]).expect("32 bytes"),
        )),
        _ => None,
    };
    let elf = &d[PROGRAMDATA_HEADER_LEN..];
    Ok(ProgramCompat {
        program_id,
        programdata_address,
        last_deployed_slot,
        upgrade_authority,
        program_len: elf.len(),
        program_sha256: Sha256::digest(elf).into(),
        instructions: CLIENT_INSTRUCTIONS
            .iter()
            .map(|name| (*name, elf_contains_discriminator(elf, discriminator(name))))
            .collect(),
    })
}

/// The shared, periodically refreshed answer the daemon's surfaces read.
#[derive(Debug, Default)]
pub struct ProgramCompatCache {
    state: Mutex<ProgramCompatSnapshot>,
}

/// One consistent reading of the cache.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramCompatSnapshot {
    /// The last successful probe, kept across later failures.
    pub compat: Option<ProgramCompat>,
    pub checked_at: Option<i64>,
    /// The last probe that failed, and when. Cleared by a success.
    pub last_error: Option<(String, i64)>,
}

impl ProgramCompatSnapshot {
    /// `Some(true|false)` once a probe has completed; `None` = unknown.
    pub fn refund_supported(&self) -> Option<bool> {
        self.compat.as_ref().map(ProgramCompat::refund_supported)
    }
}

impl ProgramCompatCache {
    pub fn new() -> Arc<ProgramCompatCache> {
        Arc::new(ProgramCompatCache::default())
    }

    pub fn snapshot(&self) -> ProgramCompatSnapshot {
        self.state.lock().expect("program compat lock").clone()
    }

    pub fn record(&self, compat: ProgramCompat, now: i64) {
        let mut s = self.state.lock().expect("program compat lock");
        s.compat = Some(compat);
        s.checked_at = Some(now);
        s.last_error = None;
    }

    pub fn record_failure(&self, error: &str, now: i64) {
        let mut s = self.state.lock().expect("program compat lock");
        s.last_error = Some((error.to_string(), now));
    }
}

/// The daemon's probe loop: once at startup, then every `interval`.
/// Read-only; logs a `error!` every time the deployed program lacks an
/// instruction the client sends, not just the first.
pub async fn run_probe_loop<R: SolanaRpc>(
    rpc: &R,
    cache: &ProgramCompatCache,
    interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    now: impl Fn() -> i64,
) -> u64 {
    let mut runs = 0u64;
    loop {
        if *shutdown.borrow() {
            return runs;
        }
        let at = now();
        match probe(rpc).await {
            Ok(compat) => {
                let missing = compat.missing();
                if missing.is_empty() {
                    tracing::info!(
                        target: "solana_program_compat",
                        slot = compat.last_deployed_slot,
                        sha256 = %compat.program_sha256_hex(),
                        "deployed Solana program dispatches every instruction this client sends"
                    );
                } else {
                    tracing::error!(
                        target: "solana_program_compat",
                        slot = compat.last_deployed_slot,
                        sha256 = %compat.program_sha256_hex(),
                        missing = ?missing,
                        refund_supported = compat.refund_supported(),
                        "deployed Solana program does NOT dispatch instruction(s) this client \
                         sends — client/program drift. Refunds are refused until the program is \
                         upgraded (docs/30-reserve-policy-deployment-runbook.md)."
                    );
                }
                cache.record(compat, at);
            }
            Err(e) => {
                tracing::error!(
                    target: "solana_program_compat",
                    error = %e,
                    "could not probe the deployed Solana program — compatibility unknown"
                );
                cache.record_failure(&e.to_string(), at);
            }
        }
        runs += 1;
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    return runs;
                }
            }
            _ = tokio::time::sleep(interval) => {}
        }
    }
}

/// Test/fixture helper: the two loader accounts for a program whose ELF
/// is `elf`, as `(program_account_data, programdata_account_data)`.
pub fn loader_accounts_for_tests(
    programdata_address: Pubkey,
    slot: u64,
    upgrade_authority: Option<Pubkey>,
    elf: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let mut program = PROGRAM_TAG.to_le_bytes().to_vec();
    program.extend_from_slice(programdata_address.as_ref());
    let mut programdata = PROGRAMDATA_TAG.to_le_bytes().to_vec();
    programdata.extend_from_slice(&slot.to_le_bytes());
    match upgrade_authority {
        Some(a) => {
            programdata.push(1);
            programdata.extend_from_slice(a.as_ref());
        }
        None => programdata.extend_from_slice(&[0u8; 33]),
    }
    programdata.extend_from_slice(elf);
    (program, programdata)
}

/// Test/fixture helper: a fake ELF that dispatches exactly `names`, in
/// the `lddw` encoding.
pub fn fake_elf_dispatching(names: &[&str]) -> Vec<u8> {
    let mut elf = b"\x7fELF-fake-".to_vec();
    for name in names {
        let disc = discriminator(name);
        // [0x18 dst off16 imm_lo] [0 0 0 0 imm_hi]
        elf.extend_from_slice(&[0x18, 0x01, 0, 0]);
        elf.extend_from_slice(&disc[0..4]);
        elf.extend_from_slice(&[0, 0, 0, 0]);
        elf.extend_from_slice(&disc[4..8]);
        // a following compare, so the pattern is not at the tail
        elf.extend_from_slice(&[0x5d, 0x12, 0, 0, 0, 0, 0, 0]);
    }
    elf
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::account::Account;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;

    struct Rpc {
        accounts: StdMutex<HashMap<Pubkey, Vec<u8>>>,
    }
    impl SolanaRpc for Rpc {
        async fn get_account(&self, pubkey: &Pubkey) -> Result<Option<Account>, SolanaRpcError> {
            Ok(self
                .accounts
                .lock()
                .unwrap()
                .get(pubkey)
                .map(|data| Account {
                    lamports: 1,
                    data: data.clone(),
                    owner: Pubkey::default(),
                    executable: true,
                    rent_epoch: 0,
                }))
        }
        async fn get_multiple_accounts(
            &self,
            _: &[Pubkey],
        ) -> Result<Vec<Option<Account>>, SolanaRpcError> {
            unimplemented!()
        }
        async fn get_slot(&self) -> Result<u64, SolanaRpcError> {
            Ok(1)
        }
        async fn get_latest_blockhash(&self) -> Result<solana_sdk::hash::Hash, SolanaRpcError> {
            unimplemented!()
        }
        async fn send_transaction(
            &self,
            _: &solana_sdk::transaction::Transaction,
        ) -> Result<solana_sdk::signature::Signature, SolanaRpcError> {
            unimplemented!()
        }
        async fn simulate_transaction(
            &self,
            _: &solana_sdk::transaction::Transaction,
        ) -> Result<super::super::rpc::SimulationOutcome, SolanaRpcError> {
            unimplemented!()
        }
        async fn get_signature_status(
            &self,
            _: &solana_sdk::signature::Signature,
        ) -> Result<Option<Result<(), String>>, SolanaRpcError> {
            unimplemented!()
        }
        async fn is_blockhash_valid(
            &self,
            _: &solana_sdk::hash::Hash,
        ) -> Result<bool, SolanaRpcError> {
            unimplemented!()
        }
    }

    fn rpc_with(elf: &[u8], authority: Option<Pubkey>) -> (Rpc, Pubkey) {
        let programdata = Pubkey::new_unique();
        let (p, pd) = loader_accounts_for_tests(programdata, 442_649_805, authority, elf);
        let mut m = HashMap::new();
        m.insert(PROGRAM_ID, p);
        m.insert(programdata, pd);
        (
            Rpc {
                accounts: StdMutex::new(m),
            },
            programdata,
        )
    }

    #[test]
    fn the_discriminator_is_anchors_global_sighash() {
        // Independently derived: sha256("global:refund_withdraw")[..8].
        let want: [u8; 8] = Sha256::digest(b"global:refund_withdraw")[..8]
            .try_into()
            .unwrap();
        assert_eq!(discriminator("refund_withdraw"), want);
        assert_eq!(
            discriminator("refund_withdraw"),
            [0x3d, 0x18, 0xb2, 0xd1, 0x69, 0x50, 0xa5, 0xc5]
        );
    }

    #[test]
    fn both_encodings_are_found_and_a_near_miss_is_not() {
        let disc = discriminator("refund_withdraw");
        assert!(!elf_contains_discriminator(b"nothing here", disc));
        let elf = fake_elf_dispatching(&["refund_withdraw"]);
        assert!(elf_contains_discriminator(&elf, disc));
        assert!(!elf_contains_discriminator(
            &elf,
            discriminator("treasury_withdraw")
        ));
        // Contiguous (rodata-style) form.
        let mut rodata = b"xx".to_vec();
        rodata.extend_from_slice(&disc);
        assert!(elf_contains_discriminator(&rodata, disc));
        // The split form WITHOUT an lddw opcode in front is not a dispatch.
        let mut bare = vec![0u8; 4];
        bare.extend_from_slice(&disc[0..4]);
        bare.extend_from_slice(&[0, 0, 0, 0]);
        bare.extend_from_slice(&disc[4..8]);
        assert!(!elf_contains_discriminator(&bare, disc));
    }

    #[tokio::test]
    async fn a_program_without_refund_withdraw_reports_it_missing() {
        let elf = fake_elf_dispatching(&[
            "release_from_reserve",
            "deposit_to_reserve",
            "record_goldcoin_completion",
            "set_paused",
            "set_limit",
            "reset_rolling_volume_window",
            "rebalance_withdraw",
        ]);
        let authority = Pubkey::new_unique();
        let (rpc, programdata) = rpc_with(&elf, Some(authority));
        let compat = probe(&rpc).await.unwrap();
        assert_eq!(compat.program_id, PROGRAM_ID);
        assert_eq!(compat.programdata_address, programdata);
        assert_eq!(compat.last_deployed_slot, 442_649_805);
        assert_eq!(compat.upgrade_authority, Some(authority));
        assert_eq!(compat.program_len, elf.len());
        assert_eq!(
            compat.program_sha256,
            <[u8; 32]>::from(Sha256::digest(&elf))
        );
        assert!(!compat.refund_supported());
        assert!(compat.supports("release_from_reserve"));
        assert_eq!(
            compat.missing(),
            vec![
                "initialize_rebalance_policy",
                "refund_withdraw",
                "treasury_withdraw"
            ]
        );
    }

    #[tokio::test]
    async fn a_program_with_every_client_instruction_is_fully_supported() {
        let elf = fake_elf_dispatching(CLIENT_INSTRUCTIONS);
        let (rpc, _) = rpc_with(&elf, None);
        let compat = probe(&rpc).await.unwrap();
        assert!(compat.refund_supported());
        assert!(compat.missing().is_empty());
        assert_eq!(compat.upgrade_authority, None);
    }

    #[tokio::test]
    async fn a_missing_or_malformed_program_is_an_error_never_unsupported() {
        let rpc = Rpc {
            accounts: StdMutex::new(HashMap::new()),
        };
        assert!(matches!(
            probe(&rpc).await.unwrap_err(),
            ProgramCompatError::ProgramMissing(_)
        ));
        // A non-loader account at the program id.
        rpc.accounts
            .lock()
            .unwrap()
            .insert(PROGRAM_ID, vec![1, 2, 3]);
        assert!(matches!(
            probe(&rpc).await.unwrap_err(),
            ProgramCompatError::NotUpgradeable(..)
        ));
        // A loader program whose ProgramData is missing.
        let (p, _) = loader_accounts_for_tests(Pubkey::new_unique(), 1, None, b"x");
        rpc.accounts.lock().unwrap().insert(PROGRAM_ID, p);
        assert!(matches!(
            probe(&rpc).await.unwrap_err(),
            ProgramCompatError::ProgramDataMissing(_)
        ));
    }

    #[test]
    fn the_cache_keeps_the_last_answer_across_a_failure() {
        let cache = ProgramCompatCache::new();
        assert_eq!(cache.snapshot().refund_supported(), None);
        cache.record_failure("rpc down", 10);
        assert_eq!(
            cache.snapshot().last_error,
            Some(("rpc down".to_string(), 10))
        );
        let elf = fake_elf_dispatching(CLIENT_INSTRUCTIONS);
        let compat = ProgramCompat {
            program_id: PROGRAM_ID,
            programdata_address: Pubkey::new_unique(),
            last_deployed_slot: 5,
            upgrade_authority: None,
            program_len: elf.len(),
            program_sha256: [0; 32],
            instructions: CLIENT_INSTRUCTIONS
                .iter()
                .map(|n| (*n, elf_contains_discriminator(&elf, discriminator(n))))
                .collect(),
        };
        cache.record(compat, 20);
        let s = cache.snapshot();
        assert_eq!(s.refund_supported(), Some(true));
        assert_eq!(s.checked_at, Some(20));
        assert_eq!(s.last_error, None);
        cache.record_failure("rpc down again", 30);
        let s = cache.snapshot();
        assert_eq!(s.refund_supported(), Some(true), "the last answer stands");
        assert_eq!(s.last_error, Some(("rpc down again".to_string(), 30)));
    }

    /// The REAL built program, when `anchor build` has run: every client
    /// instruction must be dispatched by what this repository compiles.
    #[test]
    fn the_built_program_dispatches_every_client_instruction() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/deploy/glc_reserve_bridge.so"
        );
        let Ok(elf) = std::fs::read(path) else {
            eprintln!("skipped: {path} not built");
            return;
        };
        for name in CLIENT_INSTRUCTIONS {
            assert!(
                elf_contains_discriminator(&elf, discriminator(name)),
                "{name} is not dispatched by the built program"
            );
        }
    }
}
