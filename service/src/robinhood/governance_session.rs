//! Proposing, authorizing and installing one governance change: read the
//! chain, derive the proposal, gather a quorum, simulate, broadcast,
//! and prove afterwards that the chain says what the proposal said it
//! would.
//!
//! # Dry run is not a flag on the end of this, it is the default
//!
//! [`plan`] performs every read, every derivation and every local
//! validation, and produces a [`GovernancePlan`] holding the exact
//! before/after state and the digest a quorum would have to sign. It
//! contacts no signer, spends no nonce and sends nothing. [`execute`] is
//! a separate function that takes a plan; there is no argument to
//! `plan` that makes it write.
//!
//! # The order in [`execute`], and why each step is where it is
//!
//! 1. **Re-read the nonce and epoch.** A plan is a photograph. Any
//!    governance action anywhere in the world between planning and
//!    signing invalidates it, and the contract's strict-equality nonce
//!    check would reject the result after a quorum had already signed —
//!    so the staleness is caught here, before two custody domains are
//!    asked to look at anything.
//! 2. **Gather exactly two DISTINCT signatures.** The contract requires
//!    exactly `SIGNER_THRESHOLD` and refuses `first == second`. Gathering
//!    three and sending two would mean choosing which to drop; gathering
//!    two from one domain is the duplicate the contract rejects. Both are
//!    refused here rather than on chain.
//! 3. **Simulate.** `eth_estimateGas` from the submitter's own address
//!    executes the call against current state and reverts if the call
//!    would revert — the contract's `_validateLimits`, its pause and
//!    migration guards, and the quorum check itself all run. A revert
//!    here costs nothing; a revert after broadcast costs the nonce.
//! 4. **Broadcast**, and wait for a receipt with `status == 1`.
//! 5. **Verify.** Re-read the contract and require it to hold exactly
//!    what the plan said. A successful receipt proves a transaction
//!    executed, not that it meant what was intended.
//!
//! # What this can never do
//!
//! It cannot enable a route as a consequence of a limit or pause change:
//! each [`GovernancePayload`] carries exactly one action and
//! [`apply_to`] changes exactly the fields that action changes. It holds
//! no key — the submitter's key belongs to [`super::submitter::
//! Submitter`] and the authorization keys never leave their custody
//! domains. It does not restart the daemon and does not touch the config
//! file: a governance change alters the CONTRACT, and reconciling the
//! backend's stated policy to it is `scripts/chain-policy.sh`'s job,
//! deliberately a separate action by a separate tool.

use crate::evm::{EvmAddress, EvmChainId, EvmSignature, EvmU256};
use crate::robinhood::auth::BridgeDomain;
use crate::robinhood::calls::{BridgeLimits, BridgeReader, ContractReadError};
use crate::robinhood::governance::{
    GovernanceAuth, GovernanceError, GovernancePayload, ROUTE_GLC_TO_RHN, ROUTE_RHN_TO_GLC,
    ROUTE_RHN_TO_SOL, ROUTE_SOL_TO_RHN,
};
use crate::robinhood::rpc::{EvmBlockTag, EvmCall, EvmCallRpc, EvmRpcError, EvmSubmitRpc};
use crate::robinhood::submitter::{SubmitError, Submitter};
use crate::routes::Route;

/// Everything a governance decision depends on, read in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceStateSnapshot {
    pub limits: BridgeLimits,
    pub deposits_paused: bool,
    pub payouts_paused: bool,
    pub glc_to_rhn_enabled: bool,
    pub rhn_to_glc_enabled: bool,
    pub sol_to_rhn_enabled: bool,
    pub rhn_to_sol_enabled: bool,
    pub governance_nonce: EvmU256,
    pub signer_epoch: u64,
    /// A migrated contract refuses every governance action. Read so the
    /// refusal is stated here rather than discovered as a revert.
    pub migrated: bool,
    /// Whether a successor is committed and the routes permanently
    /// closed. `commitMigration` refuses a second commit; `finalizeMigration`
    /// requires one.
    pub migration_committed: bool,
    /// The committed successor (zero when none). A finalize is a
    /// proposal about THIS address and nothing else.
    pub migration_successor: EvmAddress,
    /// From when `finalizeMigration` is callable, zero when nothing is
    /// committed. Read, not computed: a delayed predecessor and an
    /// undelayed successor answer this differently.
    pub migration_finalizable_at: u64,
    /// The pending obligations `finalizeMigration` refuses to strand.
    pub outstanding_refundable_count: EvmU256,
    pub outstanding_refundable_principal: EvmU256,
}

impl GovernanceStateSnapshot {
    /// The state this snapshot would become if `payload` were installed.
    ///
    /// Exactly the fields the named action changes, and no others. This
    /// is what makes "never enable a route as a side effect of setLimits
    /// or setPaused" a property of the type rather than a promise.
    pub fn apply_to(&self, payload: &GovernancePayload) -> Result<Self, GovernanceError> {
        let mut after = self.clone();
        match payload {
            GovernancePayload::SetLimits(limits) => after.limits = *limits,
            GovernancePayload::SetPaused {
                deposits_paused,
                payouts_paused,
            } => {
                after.deposits_paused = *deposits_paused;
                after.payouts_paused = *payouts_paused;
            }
            GovernancePayload::SetRouteEnabled { route, enabled } => {
                // `governance_route_byte` refuses the two routes the
                // contract does not model, so an unsupported route cannot
                // reach the match below with a meaning.
                match crate::robinhood::governance::governance_route_byte(*route)? {
                    ROUTE_GLC_TO_RHN => after.glc_to_rhn_enabled = *enabled,
                    ROUTE_RHN_TO_GLC => after.rhn_to_glc_enabled = *enabled,
                    ROUTE_SOL_TO_RHN => after.sol_to_rhn_enabled = *enabled,
                    ROUTE_RHN_TO_SOL => after.rhn_to_sol_enabled = *enabled,
                    other => unreachable!("governance_route_byte returned {other:#04x}"),
                }
            }
            GovernancePayload::CommitMigration { successor } => {
                after.migration_committed = true;
                after.migration_successor = *successor;
            }
            GovernancePayload::FinalizeMigration { .. } => {
                after.migrated = true;
            }
        }
        Ok(after)
    }

    /// Whether this snapshot agrees with `expected` on everything a
    /// governance action can change. The nonce is excluded: it advances
    /// by one on every successful action, which is the proof the action
    /// landed rather than a disagreement.
    pub fn matches_expected_effect(&self, expected: &GovernanceStateSnapshot) -> Vec<String> {
        let mut differences = Vec::new();
        if self.limits != expected.limits {
            differences.push(format!(
                "limits: chain holds {:?}, the proposal said {:?}",
                self.limits, expected.limits
            ));
        }
        if self.deposits_paused != expected.deposits_paused {
            differences.push(format!(
                "depositsPaused: chain holds {}, the proposal said {}",
                self.deposits_paused, expected.deposits_paused
            ));
        }
        if self.payouts_paused != expected.payouts_paused {
            differences.push(format!(
                "payoutsPaused: chain holds {}, the proposal said {}",
                self.payouts_paused, expected.payouts_paused
            ));
        }
        if self.glc_to_rhn_enabled != expected.glc_to_rhn_enabled {
            differences.push(format!(
                "routeEnabled(GlcToRhn): chain holds {}, the proposal said {}",
                self.glc_to_rhn_enabled, expected.glc_to_rhn_enabled
            ));
        }
        if self.rhn_to_glc_enabled != expected.rhn_to_glc_enabled {
            differences.push(format!(
                "routeEnabled(RhnToGlc): chain holds {}, the proposal said {}",
                self.rhn_to_glc_enabled, expected.rhn_to_glc_enabled
            ));
        }
        if self.sol_to_rhn_enabled != expected.sol_to_rhn_enabled {
            differences.push(format!(
                "routeEnabled(SolToRhn): chain holds {}, the proposal said {}",
                self.sol_to_rhn_enabled, expected.sol_to_rhn_enabled
            ));
        }
        if self.rhn_to_sol_enabled != expected.rhn_to_sol_enabled {
            differences.push(format!(
                "routeEnabled(RhnToSol): chain holds {}, the proposal said {}",
                self.rhn_to_sol_enabled, expected.rhn_to_sol_enabled
            ));
        }
        if self.migration_committed != expected.migration_committed {
            differences.push(format!(
                "migrationCommitted: chain holds {}, the proposal said {}",
                self.migration_committed, expected.migration_committed
            ));
        }
        if self.migration_successor != expected.migration_successor {
            differences.push(format!(
                "migrationSuccessor: chain holds {}, the proposal said {}",
                self.migration_successor.to_checksum_string(),
                expected.migration_successor.to_checksum_string()
            ));
        }
        if self.migrated != expected.migrated {
            differences.push(format!(
                "migrated: chain holds {}, the proposal said {}",
                self.migrated, expected.migrated
            ));
        }
        differences
    }
}

/// Why a governance session could not proceed.
#[derive(Debug, thiserror::Error)]
pub enum GovernanceSessionError {
    #[error("reading the contract: {0}")]
    Read(#[from] ContractReadError),
    #[error(transparent)]
    Encoding(#[from] GovernanceError),
    #[error(
        "this deployment's configured EVM chain id is {configured}, but the endpoint reports \
         {actual} — refusing to build a governance authorization against a network this \
         deployment does not serve"
    )]
    WrongChainId { configured: u64, actual: u64 },
    #[error(
        "the contract at {contract} reports migrated = true. A committed migration closes this \
         bridge permanently and every governance action but a disable is refused on chain"
    )]
    AlreadyMigrated { contract: String },
    #[error(
        "commitMigration requires BOTH directions paused on chain first, and the contract \
         reports depositsPaused = {deposits_paused}, payoutsPaused = {payouts_paused}. Pause \
         both (robinhood-governance-pause, or a guardian's guardianPause(true, true)) and re-plan"
    )]
    MigrationRequiresPause {
        deposits_paused: bool,
        payouts_paused: bool,
    },
    #[error(
        "a migration to {successor} is already committed on chain. There is no second commit \
         and no cancel from here: a guardian may veto it, or it may be finalized"
    )]
    MigrationAlreadyCommitted { successor: String },
    #[error(
        "the successor {successor} is not usable: {detail}. `commitMigration` would revert \
         InvalidSuccessor, so nothing was signed"
    )]
    InvalidSuccessor { successor: String, detail: String },
    #[error(
        "no migration is committed on chain, so there is nothing to finalize. Commit a successor \
         first (robinhood-governance-commit-migration)"
    )]
    MigrationNotCommitted,
    #[error(
        "the chain holds {committed} as the committed successor, but this proposal names \
         {proposed}. A finalize is a proposal about the committed address and nothing else; \
         re-run naming the address the chain holds, or have a guardian veto it"
    )]
    SuccessorMismatch { committed: String, proposed: String },
    #[error(
        "the contract will not accept finalizeMigration before unix time {finalizable_at} (it is \
         {now}; {remaining_secs}s remain). This is the DEPLOYED contract's own migration delay, \
         enforced from its bytecode; nothing off chain shortens it. Re-plan after that time"
    )]
    MigrationNotReady {
        finalizable_at: u64,
        now: u64,
        remaining_secs: u64,
    },
    #[error(
        "finalizeMigration reverts while any obligation is still Pending, and the contract \
         reports {count} pending obligation(s) holding {principal} (18dp). Every one must be \
         settled, refunded or abandoned first — finalizing would strand its depositor's principal"
    )]
    OutstandingRefundsRemain { count: String, principal: String },
    #[error(
        "the governance nonce moved from {planned} to {actual} between planning and signing — \
         another governance action landed in between. Nothing was signed and nothing was sent; \
         re-plan against the current state and read the new before/after"
    )]
    StaleNonce { planned: String, actual: String },
    #[error(
        "the signer epoch moved from {planned} to {actual} between planning and signing — the \
         signer set was rotated. Every authorization built under the old epoch is void; re-plan"
    )]
    StaleSignerEpoch { planned: u64, actual: u64 },
    #[error(
        "the quorum needs {required} signatures from DISTINCT signers and only {gathered} \
         distinct signer(s) answered. The contract requires exactly {required} and refuses two \
         signatures from the same address"
    )]
    QuorumNotMet { required: usize, gathered: usize },
    #[error(
        "custody domain {identity} answered with a signature from {address}, which another \
         domain already provided. The contract refuses a duplicate signer, so this quorum could \
         never be valid"
    )]
    DuplicateSigner { identity: String, address: String },
    #[error("custody domain {identity} refused or could not be reached: {detail}")]
    SignerUnavailable { identity: String, detail: String },
    #[error(
        "the simulation reverted, so this call would fail on chain and consume the governance \
         nonce for nothing: {detail}"
    )]
    SimulationReverted { detail: String },
    #[error("submitting: {0}")]
    Submit(#[from] SubmitError),
    #[error("broadcasting: {0}")]
    Rpc(#[from] EvmRpcError),
    #[error(
        "the transaction {tx_hash} was mined with status 0 (reverted). The governance nonce was \
         NOT consumed by a reverted call, so the proposal may be re-planned and retried"
    )]
    TransactionReverted { tx_hash: String },
    #[error(
        "no receipt for {tx_hash} after {waited}s. The transaction may still be pending: do NOT \
         re-broadcast a second governance action until this one has resolved, or two proposals \
         will contend for one nonce"
    )]
    ReceiptTimeout { tx_hash: String, waited: u64 },
    #[error(
        "the transaction succeeded but the contract does NOT hold what the proposal said it \
         would:\n{}\nThis is the check that exists because a successful receipt proves a \
         transaction executed, not that it meant what was intended", .differences.join("\n")
    )]
    PostStateDisagrees { differences: Vec<String> },
}

/// One custody domain that can authorize a governance action.
///
/// A trait so the session can be driven by the real
/// [`crate::signing::remote::RemoteEvmAuthSigner`] in production and by a
/// local key in tests, without either knowing about the other.
pub trait GovernanceQuorumSigner {
    /// A stable label for messages — an address or an endpoint name.
    /// Never a token, and never a key.
    fn identity(&self) -> String;
    fn sign_governance<'a>(
        &'a self,
        auth: &'a GovernanceAuth,
        domain: BridgeDomain,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<EvmSignature, String>> + Send + 'a>,
    >;
}

/// A complete, validated proposal that has written and sent nothing.
#[derive(Debug, Clone)]
pub struct GovernancePlan {
    pub domain: BridgeDomain,
    pub auth: GovernanceAuth,
    pub digest: [u8; 32],
    pub before: GovernanceStateSnapshot,
    pub after: GovernanceStateSnapshot,
}

impl GovernancePlan {
    /// Whether this proposal would change anything at all.
    pub fn is_noop(&self) -> bool {
        self.before.matches_expected_effect(&self.after).is_empty()
    }
}

/// Reads every value a governance decision depends on.
pub async fn read_state<R: EvmCallRpc>(
    reader: &BridgeReader,
    rpc: &R,
    block: EvmBlockTag,
) -> Result<GovernanceStateSnapshot, GovernanceSessionError> {
    Ok(GovernanceStateSnapshot {
        limits: reader.limits(rpc, block).await?,
        deposits_paused: reader.deposits_paused(rpc, block).await?,
        payouts_paused: reader.payouts_paused(rpc, block).await?,
        glc_to_rhn_enabled: reader.route_enabled(rpc, ROUTE_GLC_TO_RHN, block).await?,
        rhn_to_glc_enabled: reader.route_enabled(rpc, ROUTE_RHN_TO_GLC, block).await?,
        sol_to_rhn_enabled: reader.route_enabled(rpc, ROUTE_SOL_TO_RHN, block).await?,
        rhn_to_sol_enabled: reader.route_enabled(rpc, ROUTE_RHN_TO_SOL, block).await?,
        governance_nonce: reader.governance_nonce(rpc, block).await?,
        signer_epoch: reader.signer_epoch(rpc, block).await?,
        migrated: reader.migrated(rpc, block).await?,
        migration_committed: reader.migration_committed(rpc, block).await?,
        migration_successor: reader.migration_successor(rpc, block).await?,
        migration_finalizable_at: reader.migration_finalizable_at(rpc, block).await?,
        outstanding_refundable_count: reader.outstanding_refundable_count(rpc, block).await?,
        outstanding_refundable_principal: reader
            .outstanding_refundable_principal(rpc, block)
            .await?,
    })
}

/// What a successor must answer before `commitMigration` will accept it,
/// read from the successor itself. The contract checks exactly these —
/// `code.length != 0`, `!= address(this)`, `token()` and
/// `bridgeProtocolId()` — and this re-states them so a bad successor is
/// refused before a quorum is asked, with the reason named.
///
/// This proves nothing about whether the successor is CORRECT: a
/// contract that merely answers these two views passes every on-chain
/// check and can swallow the reserve. Verifying its bytecode against the
/// repository's build is the human step this cannot replace.
pub async fn check_successor<R: EvmCallRpc>(
    rpc: &R,
    bridge: EvmAddress,
    successor: EvmAddress,
) -> Result<(), GovernanceSessionError> {
    let refuse = |detail: String| GovernanceSessionError::InvalidSuccessor {
        successor: successor.to_checksum_string(),
        detail,
    };
    if successor == EvmAddress::ZERO {
        return Err(refuse("it is the zero address".into()));
    }
    if successor == bridge {
        return Err(refuse("it is the bridge itself".into()));
    }
    let code = rpc
        .code_at(successor, EvmBlockTag::Latest)
        .await
        .map_err(|e| refuse(format!("reading its code: {e}")))?;
    if code.is_empty() {
        return Err(refuse(
            "no contract code at that address (an EOA, or not deployed)".into(),
        ));
    }
    let predecessor = BridgeReader::new(bridge);
    let candidate = BridgeReader::new(successor);
    let block = EvmBlockTag::Latest;
    let our_token = predecessor.token(rpc, block).await?;
    let its_token = candidate
        .token(rpc, block)
        .await
        .map_err(|e| refuse(format!("it does not answer token(): {e}")))?;
    if its_token != our_token {
        return Err(refuse(format!(
            "it custodies {} but this bridge custodies {}",
            its_token.to_checksum_string(),
            our_token.to_checksum_string()
        )));
    }
    let ours = predecessor.bridge_protocol_id(rpc, block).await?;
    let its = candidate
        .bridge_protocol_id(rpc, block)
        .await
        .map_err(|e| refuse(format!("it does not answer bridgeProtocolId(): {e}")))?;
    if its != ours {
        return Err(refuse(
            "its bridgeProtocolId() is not this protocol family".into(),
        ));
    }
    Ok(())
}

/// Builds a proposal against an already-read snapshot.
///
/// Writes nothing, contacts no signer and spends no nonce. Every refusal
/// available here happens before a custody domain is asked to look at
/// anything.
pub fn plan(
    before: GovernanceStateSnapshot,
    domain: BridgeDomain,
    configured_chain_id: EvmChainId,
    payload: GovernancePayload,
    expiry: u64,
    now: u64,
) -> Result<GovernancePlan, GovernanceSessionError> {
    if domain.chain_id != configured_chain_id {
        return Err(GovernanceSessionError::WrongChainId {
            configured: configured_chain_id.get(),
            actual: domain.chain_id.get(),
        });
    }
    if before.migrated {
        return Err(GovernanceSessionError::AlreadyMigrated {
            contract: domain.verifying_contract.to_checksum_string(),
        });
    }
    // The migration actions' own on-chain gates, re-stated so a plan that
    // the contract would revert is refused here with the reason, before a
    // custody domain is asked to look at it. The contract remains the
    // enforcer; this can only refuse earlier, never authorize more.
    match &payload {
        GovernancePayload::CommitMigration { .. } => {
            if !before.deposits_paused || !before.payouts_paused {
                return Err(GovernanceSessionError::MigrationRequiresPause {
                    deposits_paused: before.deposits_paused,
                    payouts_paused: before.payouts_paused,
                });
            }
            if before.migration_committed {
                return Err(GovernanceSessionError::MigrationAlreadyCommitted {
                    successor: before.migration_successor.to_checksum_string(),
                });
            }
        }
        GovernancePayload::FinalizeMigration { successor } => {
            if !before.migration_committed {
                return Err(GovernanceSessionError::MigrationNotCommitted);
            }
            if *successor != before.migration_successor {
                return Err(GovernanceSessionError::SuccessorMismatch {
                    committed: before.migration_successor.to_checksum_string(),
                    proposed: successor.to_checksum_string(),
                });
            }
            if now < before.migration_finalizable_at {
                return Err(GovernanceSessionError::MigrationNotReady {
                    finalizable_at: before.migration_finalizable_at,
                    now,
                    remaining_secs: before.migration_finalizable_at - now,
                });
            }
            if !before.outstanding_refundable_count.is_zero()
                || !before.outstanding_refundable_principal.is_zero()
            {
                return Err(GovernanceSessionError::OutstandingRefundsRemain {
                    count: decimal(before.outstanding_refundable_count),
                    principal: decimal(before.outstanding_refundable_principal),
                });
            }
        }
        _ => {}
    }
    let after = before.apply_to(&payload)?;
    let auth = GovernanceAuth {
        payload,
        signer_epoch: before.signer_epoch,
        nonce: before.governance_nonce,
        expiry,
    };
    let digest = auth.digest(domain)?;
    Ok(GovernancePlan {
        domain,
        auth,
        digest,
        before,
        after,
    })
}

/// A `uint256` for a message: decimal when it fits, the hex word when it
/// does not — a count or a principal above `u128::MAX` is not a figure
/// worth rounding for.
fn decimal(value: EvmU256) -> String {
    match value.try_to_u128() {
        Ok(v) => v.to_string(),
        Err(_) => value.to_word_hex(),
    }
}

/// What [`execute`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernanceExecution {
    pub tx_hash: String,
    pub gas_used: u64,
    pub signers: Vec<String>,
    /// The state read back AFTER the receipt, which agreed with the plan.
    pub verified: GovernanceStateSnapshot,
}

/// How long to wait for a receipt, and how often to look.
#[derive(Debug, Clone, Copy)]
pub struct ReceiptWait {
    pub timeout_secs: u64,
    pub poll_interval_secs: u64,
}

impl Default for ReceiptWait {
    fn default() -> ReceiptWait {
        ReceiptWait {
            timeout_secs: 300,
            poll_interval_secs: 3,
        }
    }
}

/// Gathers a quorum, simulates, broadcasts, and verifies the result.
///
/// Every step's failure leaves the chain as it was, except a broadcast
/// that lands — and a landed transaction is verified against the plan
/// before this returns success.
#[allow(clippy::too_many_arguments)]
pub async fn execute<R, S>(
    plan: &GovernancePlan,
    reader: &BridgeReader,
    rpc: &R,
    submitter: &Submitter,
    signers: &[&S],
    required_signatures: usize,
    wait: ReceiptWait,
    sleep: impl Fn(u64) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
) -> Result<GovernanceExecution, GovernanceSessionError>
where
    R: EvmCallRpc + EvmSubmitRpc,
    S: GovernanceQuorumSigner + ?Sized,
{
    // ---- 1. the plan is still current ----
    let now_nonce = reader.governance_nonce(rpc, EvmBlockTag::Latest).await?;
    if now_nonce != plan.auth.nonce {
        return Err(GovernanceSessionError::StaleNonce {
            planned: plan.auth.nonce.to_word_hex(),
            actual: now_nonce.to_word_hex(),
        });
    }
    let now_epoch = reader.signer_epoch(rpc, EvmBlockTag::Latest).await?;
    if now_epoch != plan.auth.signer_epoch {
        return Err(GovernanceSessionError::StaleSignerEpoch {
            planned: plan.auth.signer_epoch,
            actual: now_epoch,
        });
    }

    // ---- 2. exactly `required_signatures` DISTINCT signers ----
    let mut signatures: Vec<Vec<u8>> = Vec::new();
    let mut addresses: Vec<EvmAddress> = Vec::new();
    let mut identities: Vec<String> = Vec::new();
    let mut refusals: Vec<String> = Vec::new();
    for signer in signers {
        if signatures.len() == required_signatures {
            break;
        }
        let signature = match signer.sign_governance(&plan.auth, plan.domain).await {
            Ok(signature) => signature,
            Err(detail) => {
                // One domain being unavailable is not fatal while others
                // remain; the quorum check below is the authority.
                refusals.push(format!("{}: {detail}", signer.identity()));
                continue;
            }
        };
        let address = crate::evm::secp::recover_address(&plan.digest, &signature).map_err(|e| {
            GovernanceSessionError::SignerUnavailable {
                identity: signer.identity(),
                detail: format!("the signature does not recover: {e}"),
            }
        })?;
        if addresses.contains(&address) {
            return Err(GovernanceSessionError::DuplicateSigner {
                identity: signer.identity(),
                address: address.to_checksum_string(),
            });
        }
        addresses.push(address);
        identities.push(format!(
            "{} ({})",
            signer.identity(),
            address.to_checksum_string()
        ));
        signatures.push(signature.to_bytes().to_vec());
    }
    if signatures.len() != required_signatures {
        if !refusals.is_empty() {
            return Err(GovernanceSessionError::SignerUnavailable {
                identity: format!("{} of {}", refusals.len(), signers.len()),
                detail: refusals.join("; "),
            });
        }
        return Err(GovernanceSessionError::QuorumNotMet {
            required: required_signatures,
            gathered: signatures.len(),
        });
    }

    let call = EvmCall {
        to: plan.domain.verifying_contract,
        data: plan.auth.calldata(&signatures)?,
    };

    // ---- 3. simulate ----
    let gas_limit = match submitter.estimate_gas(rpc, &call).await {
        Ok(limit) => limit,
        Err(SubmitError::EstimateFailed { detail }) => {
            return Err(GovernanceSessionError::SimulationReverted { detail })
        }
        Err(other) => return Err(other.into()),
    };

    // ---- 4. broadcast ----
    submitter.check_funding(rpc).await?;
    // The submitter's PENDING nonce, read straight from the node rather
    // than allocated from the ledger. A governance action is not a bridge
    // obligation: it has no ledger row, no request id and nothing to
    // resume, so `super::nonce`'s ledger allocator — which exists so a
    // crashed settlement can find its own transaction again — would be
    // recording a row nothing ever reads. `"pending"` is the right tag
    // here for the reason the trait documents: it counts this
    // deployment's own in-flight transactions.
    let nonce = rpc
        .pending_nonce(submitter.address())
        .await
        .map_err(|source| SubmitError::Rpc {
            doing: "reading the submitter's pending nonce",
            source,
        })?;
    let fees = submitter.read_fees(rpc, 0).await?;
    let signed = submitter.sign(nonce, gas_limit, fees, &call);
    let tx_hash = crate::evm::hex::encode_lower(&signed.hash.to_bytes());
    rpc.send_raw_transaction(&signed.raw).await?;

    // ---- 5. receipt ----
    let mut waited = 0;
    let receipt = loop {
        if let Some(receipt) = rpc.transaction_receipt(signed.hash).await? {
            break receipt;
        }
        if waited >= wait.timeout_secs {
            return Err(GovernanceSessionError::ReceiptTimeout {
                tx_hash: tx_hash.clone(),
                waited,
            });
        }
        sleep(wait.poll_interval_secs).await;
        waited += wait.poll_interval_secs;
    };
    if !receipt.success {
        return Err(GovernanceSessionError::TransactionReverted { tx_hash });
    }

    // ---- 6. and prove it meant what it said ----
    let verified = read_state(reader, rpc, EvmBlockTag::Latest).await?;
    let differences = verified.matches_expected_effect(&plan.after);
    if !differences.is_empty() {
        return Err(GovernanceSessionError::PostStateDisagrees { differences });
    }

    Ok(GovernanceExecution {
        tx_hash,
        gas_used: receipt.gas_used,
        signers: identities,
        verified,
    })
}

/// The route pair, for reporting. Not derived from the contract's byte
/// values at call sites, so a new route cannot silently change which
/// field a report reads.
pub const REPORTED_ROUTES: [(Route, u8); 2] = [
    (Route::GlcToRhn, ROUTE_GLC_TO_RHN),
    (Route::RhnToGlc, ROUTE_RHN_TO_GLC),
];

#[cfg(test)]
mod tests;
