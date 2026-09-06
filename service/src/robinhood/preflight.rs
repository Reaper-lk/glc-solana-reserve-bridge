//! The startup verification gate: everything that must be true about the
//! deployed contracts before any Robinhood route may be enabled.
//!
//! # What this proves, and what it emphatically does not
//!
//! It proves that the addresses in the config file are the contracts this
//! code was written against, on the network this deployment expects:
//!
//! - there is contract CODE at the bridge address at all;
//! - `eth_chainId` is the configured chain id;
//! - the bridge's `bridgeProtocolId()` is this protocol family;
//! - the bridge's `TOKEN()` is the configured expected token;
//! - the token's `decimals()` is exactly 18;
//! - the bridge's `signers()` is the configured authorized set;
//! - the bridge's `domainSeparator()` equals the one this service
//!   computes, so every authorization it mints will be checked against
//!   the domain it was built for;
//! - each route's `routeChains()` pair, recorded for use in every
//!   authorization;
//! - the configured transaction envelope matches the chain's own fee
//!   market.
//!
//! It does **not** prove anything about the token beyond its decimals. In
//! particular it says nothing about whether the token has a mint
//! authority, a blocklist, a transfer hook, a fee on transfer, a pause,
//! or an upgradeable proxy behind it. Those are properties of the token's
//! CODE and its governance, not of any value this service can read, and
//! establishing them is a separate mainnet token review that this
//! function neither performs nor substitutes for. Claiming otherwise
//! because a `decimals()` call succeeded would be the worst possible
//! outcome of writing this module.
//!
//! # Why every check is a REFUSAL rather than a warning
//!
//! Each of these is a value the whole settlement path depends on being
//! true, and none of them can be "mostly" right. A wrong token address
//! means paying out a different asset; a wrong decimals means every
//! amount is off by ten orders of magnitude; a wrong domain separator
//! means every signature is worthless; a wrong signer set means every
//! quorum is rejected on-chain after gas was spent.
//!
//! # When it runs
//!
//! At startup, before the orchestrator's first tick, and it is the gate
//! [`crate::chains::robinhood::RobinhoodAdapter`] consults: an adapter
//! constructed from an UNVERIFIED configuration reports
//! [`crate::chains::Capability::Unavailable`], so a deployment that
//! skipped or failed preflight cannot open a route no matter what its
//! config file or its `bridge_routes` table say.

use crate::amount_conversion::robinhood::{ensure_robinhood_decimals, ROBINHOOD_DECIMALS};
use crate::evm::{EvmAddress, EvmChainId, TxEnvelope};
use crate::routes::Route;

use super::auth::ProtocolChainPair;
use super::calls::{self, BridgeReader, ContractReadError, TokenReader};
use super::config::RobinhoodIndexerConfig;
use super::rpc::{EvmBlockTag, EvmCallRpc, EvmRpc, EvmRpcError, EvmSubmitRpc};
use super::settlement_config::RobinhoodSettlementConfig;

#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    #[error("Robinhood RPC while {doing}: {source}")]
    Rpc {
        doing: &'static str,
        #[source]
        source: EvmRpcError,
    },
    #[error(transparent)]
    Read(#[from] ContractReadError),
    #[error(
        "the endpoint reports chain id {actual}, but this deployment is configured for {expected} \
         — refusing to settle on a different network than the one it was configured for"
    )]
    WrongChain { expected: u64, actual: u64 },
    #[error(
        "there is NO CONTRACT CODE at {address} on this chain — the configured bridge address is \
         either wrong, or names a contract that has not been deployed on this network"
    )]
    NoContractCode { address: String },
    #[error(
        "there is NO CONTRACT CODE at the configured token address {address} — an `eth_call` to \
         an address with no code returns empty data rather than failing, so this is checked \
         directly"
    )]
    NoTokenCode { address: String },
    #[error(
        "the contract at {address} reports bridgeProtocolId() = {actual}, not this protocol \
         family's {expected} — it is not a GLC reserve bridge"
    )]
    WrongProtocol {
        address: String,
        expected: String,
        actual: String,
    },
    #[error(
        "the bridge custodies token {actual}, but robinhood.indexer.expected_token says {expected} \
         — paying out against this contract would move a DIFFERENT asset than the one configured"
    )]
    WrongToken { expected: String, actual: String },
    #[error(
        "the reserve token reports {actual} decimals, but this bridge's amount model is built for \
         exactly {expected}: {detail}"
    )]
    WrongDecimals {
        expected: u32,
        actual: u8,
        detail: String,
    },
    #[error(
        "the bridge's signer set is {actual:?}, but robinhood.settlement.authorized_signers says \
         {expected:?} — every quorum this service assembled would be refused on-chain"
    )]
    WrongSignerSet {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    #[error(
        "the bridge's own domainSeparator() is {actual} but this service computes {expected} — \
         every authorization it minted would be signed over the wrong domain and rejected. The \
         cross-language golden fixture proves the FORMULA; this proves the DEPLOYMENT."
    )]
    DomainSeparatorMismatch { expected: String, actual: String },
    #[error(
        "the configured transaction envelope is {envelope}, but the chain's latest block \
         {evidence}. This is the one chain property this repository had no evidence for, so it \
         is configured AND verified — a configured value nothing checks is still a guess."
    )]
    EnvelopeMismatch {
        envelope: &'static str,
        evidence: &'static str,
    },
    #[error(
        "the bridge has MIGRATED to a successor contract: every value-moving call against this \
         address reverts permanently. Point this deployment at the successor."
    )]
    AlreadyMigrated,
    #[error(
        "route {route} resolves to protocol chain pair ({source_chain}, {dest}) on the \
         contract, but the two legs must be distinct — refusing to bind a self-referential route"
    )]
    DegenerateRouteChains {
        route: &'static str,
        // Named `source_chain` rather than `source` because `thiserror`
        // treats a field literally named `source` as the error's CAUSE.
        source_chain: u64,
        dest: u64,
    },
}

/// What preflight established, and which every later step reads rather
/// than re-deriving.
///
/// Constructing one is [`verify`]'s job and there is no other
/// constructor, so possessing a `VerifiedDeployment` IS the evidence that
/// every check above passed against this exact deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDeployment {
    pub chain_id: EvmChainId,
    pub bridge_contract: EvmAddress,
    /// The token the bridge actually custodies, read from the contract —
    /// not the configured expectation, which was only used to check it.
    pub token: EvmAddress,
    pub token_decimals: u8,
    pub signers: [EvmAddress; 3],
    /// The contract's own EIP-712 domain separator, confirmed equal to
    /// this service's computed one.
    pub domain_separator: [u8; 32],
    /// The `(protocolSourceChainId, protocolDestChainId)` pair for each
    /// EXECUTABLE route, read from the contract's immutables.
    ///
    /// Read once here rather than before every authorization: they are
    /// immutable at the contract's construction, so a per-operation read
    /// would be a round trip that can only ever return the same answer.
    /// Carrying them means an authorization is built from a value that
    /// was proven against the deployment, not from configuration.
    pub glc_to_rhn_chains: ProtocolChainPair,
    pub rhn_to_glc_chains: ProtocolChainPair,
    pub tx_envelope: TxEnvelope,
    /// Whether the chain's latest header carries a `baseFeePerGas` — the
    /// evidence the envelope was checked against, carried so it can be
    /// logged rather than re-derived.
    pub chain_has_base_fee: bool,
}

impl VerifiedDeployment {
    /// The chain pair for one executable route.
    ///
    /// Returns `None` for the two Solana<->Robinhood routes and for the
    /// two Solana<->Goldcoin ones: neither group is verified here,
    /// because neither is executable, and handing back a pair for one
    /// would suggest otherwise.
    pub fn chains_for(&self, route: Route) -> Option<ProtocolChainPair> {
        match route {
            Route::GlcToRhn => Some(self.glc_to_rhn_chains),
            Route::RhnToGlc => Some(self.rhn_to_glc_chains),
            Route::GlcToSol | Route::SolToGlc | Route::SolToRhn | Route::RhnToSol => None,
        }
    }

    /// The EIP-712 domain, from the verified deployment identity.
    pub fn domain(&self) -> super::auth::BridgeDomain {
        super::auth::BridgeDomain::new(self.chain_id, self.bridge_contract)
    }
}

/// Runs every check and returns the verified deployment, or the first
/// refusal.
///
/// Every read is pinned to `Latest` rather than a specific block: this is
/// a startup check about the deployment as it exists now, and a pinned
/// historical block would answer a question nobody asked.
pub async fn verify<R>(
    rpc: &R,
    indexer: &RobinhoodIndexerConfig,
    settlement: &RobinhoodSettlementConfig,
) -> Result<VerifiedDeployment, PreflightError>
where
    R: EvmRpc + EvmCallRpc + EvmSubmitRpc,
{
    let block = EvmBlockTag::Latest;

    // ---- the network ----
    let chain_id = rpc.chain_id().await.map_err(|source| PreflightError::Rpc {
        doing: "reading the chain id",
        source,
    })?;
    if chain_id != settlement.chain_id {
        return Err(PreflightError::WrongChain {
            expected: settlement.chain_id.get(),
            actual: chain_id.get(),
        });
    }

    // ---- there is something deployed at all ----
    //
    // FIRST, because an `eth_call` to an address with no code does not
    // fail — it returns empty data, which every subsequent decoder
    // reports as a malformed return. Checking code presence directly is
    // what turns "the configured address is wrong" into its own message
    // rather than a confusing ABI error.
    let bridge_code = rpc
        .code_at(settlement.bridge_contract, block)
        .await
        .map_err(|source| PreflightError::Rpc {
            doing: "reading the bridge contract's code",
            source,
        })?;
    if bridge_code.is_empty() {
        return Err(PreflightError::NoContractCode {
            address: settlement.bridge_contract.to_checksum_string(),
        });
    }

    let reader = BridgeReader::new(settlement.bridge_contract);

    // ---- it is a bridge of THIS protocol family ----
    let protocol_id = reader.bridge_protocol_id(rpc, block).await?;
    let expected_protocol = calls::bridge_protocol_id();
    if protocol_id != expected_protocol {
        return Err(PreflightError::WrongProtocol {
            address: settlement.bridge_contract.to_checksum_string(),
            expected: hex32(&expected_protocol),
            actual: hex32(&protocol_id),
        });
    }

    // ---- it has not already handed its reserve to a successor ----
    if reader.migrated(rpc, block).await? {
        return Err(PreflightError::AlreadyMigrated);
    }

    // ---- the token ----
    let token = reader.token(rpc, block).await?;
    if token != indexer.expected_token {
        return Err(PreflightError::WrongToken {
            expected: indexer.expected_token.to_checksum_string(),
            actual: token.to_checksum_string(),
        });
    }
    let token_code = rpc
        .code_at(token, block)
        .await
        .map_err(|source| PreflightError::Rpc {
            doing: "reading the token contract's code",
            source,
        })?;
    if token_code.is_empty() {
        return Err(PreflightError::NoTokenCode {
            address: token.to_checksum_string(),
        });
    }
    let decimals = TokenReader::new(token).decimals(rpc, block).await?;
    ensure_robinhood_decimals(decimals).map_err(|e| PreflightError::WrongDecimals {
        expected: ROBINHOOD_DECIMALS,
        actual: decimals,
        detail: e.to_string(),
    })?;

    // ---- the signer set ----
    let signers = reader.signers(rpc, block).await?;
    // Compared as SETS, not as ordered lists: the contract stores an
    // array but `_isSigner` is a mapping, and a rotation may legitimately
    // reorder. Requiring an order would make a correct configuration fail.
    let mut on_chain: Vec<EvmAddress> = signers.to_vec();
    let mut configured: Vec<EvmAddress> = settlement.authorized_signers.to_vec();
    on_chain.sort_by_key(|a| a.to_bytes());
    configured.sort_by_key(|a| a.to_bytes());
    if on_chain != configured {
        return Err(PreflightError::WrongSignerSet {
            expected: configured.iter().map(|a| a.to_checksum_string()).collect(),
            actual: on_chain.iter().map(|a| a.to_checksum_string()).collect(),
        });
    }

    // ---- the EIP-712 domain ----
    //
    // The cross-language golden fixture proves this service's FORMULA
    // matches the contract's. This proves the formula, applied to THIS
    // deployment's address and chain id, produces the separator the
    // deployed contract actually uses.
    let on_chain_domain = reader.domain_separator(rpc, block).await?;
    let computed_domain = settlement.domain().separator();
    if on_chain_domain != computed_domain {
        return Err(PreflightError::DomainSeparatorMismatch {
            expected: hex32(&computed_domain),
            actual: hex32(&on_chain_domain),
        });
    }

    // ---- the route topology ----
    let mut pairs = Vec::with_capacity(2);
    for route in [Route::GlcToRhn, Route::RhnToGlc] {
        let route_byte = route
            .contract_route_id()
            .expect("both executable Robinhood routes have a contract discriminator");
        let chains = reader.route_chains(rpc, route_byte, block).await?;
        if chains.source == chains.dest {
            return Err(PreflightError::DegenerateRouteChains {
                route: route.as_str(),
                source_chain: chains.source,
                dest: chains.dest,
            });
        }
        pairs.push(chains);
    }

    // ---- the fee market ----
    //
    // The one chain property this repository had no evidence for. It is
    // configured, and it is verified here against the chain's own header:
    // a `baseFeePerGas` proves London is active, and its absence proves
    // it is not.
    let base_fee = rpc
        .latest_base_fee()
        .await
        .map_err(|source| PreflightError::Rpc {
            doing: "reading the chain's base fee to verify the transaction envelope",
            source,
        })?;
    let chain_has_base_fee = base_fee.is_some();
    match (settlement.tx_envelope, chain_has_base_fee) {
        (TxEnvelope::Eip1559, false) => {
            return Err(PreflightError::EnvelopeMismatch {
                envelope: "eip1559",
                evidence: "carries NO baseFeePerGas, so this chain has no EIP-1559 fee market \
                           and would not recognise a type-0x02 transaction",
            })
        }
        (TxEnvelope::Legacy, true) => {
            return Err(PreflightError::EnvelopeMismatch {
                envelope: "legacy",
                evidence: "DOES carry a baseFeePerGas, so this chain has an EIP-1559 fee market \
                           and a legacy transaction's gasPrice is not the price it will be \
                           charged",
            })
        }
        _ => {}
    }

    Ok(VerifiedDeployment {
        chain_id,
        bridge_contract: settlement.bridge_contract,
        token,
        token_decimals: decimals,
        signers,
        domain_separator: on_chain_domain,
        glc_to_rhn_chains: pairs[0],
        rhn_to_glc_chains: pairs[1],
        tx_envelope: settlement.tx_envelope,
        chain_has_base_fee,
    })
}

fn hex32(bytes: &[u8; 32]) -> String {
    format!(
        "0x{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

#[cfg(test)]
mod tests;
