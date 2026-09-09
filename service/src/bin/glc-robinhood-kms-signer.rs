//! `glc-robinhood-kms-signer` — the custody domain's server half of the
//! Robinhood EIP-712 authorization protocol, backed by an AWS KMS
//! `ECC_SECG_P256K1` / `SIGN_VERIFY` key.
//!
//! This is the process `crate::signing::remote::RemoteEvmAuthSigner`
//! talks to. It runs in the custody domain, not on the bridge host, and
//! it is one of the deployment's three independent signers
//! (`alias/glc-robinhood-signer-a|b|c`) — WHICH one is entirely a matter
//! of the `GLC_RHN_SIGNER_KMS_KEY_ID` this instance is given.
//!
//! # It holds no key and signs nothing it did not derive
//!
//! The private key lives in KMS. This process holds a key id and an IAM
//! identity. It exposes exactly two endpoints, neither of which accepts
//! bytes or a digest to sign: the only value ever sent to KMS is the
//! digest `EvmSignerPolicy::evaluate` recomputed from a request's
//! structured fields and approved. See
//! `glc_reserve_bridge_service::signing::evm_kms` for the full design.
//!
//! # Startup fails closed, in this order
//!
//! 1. Configuration — every `GLC_RHN_SIGNER_*` value, with no defaults
//!    for anything that identifies the deployment.
//! 2. The KMS client, and the key's spec/usage where IAM lets this
//!    process read them.
//! 3. The identity proof: KMS signs one fixed domain-separated challenge
//!    and the recovered address must equal `GLC_RHN_SIGNER_EVM_ADDRESS`.
//!    Only after that does anything bind a socket, so this process never
//!    serves an identity it cannot actually sign as.
//!
//! Every one of those exits non-zero rather than degrading.
//!
//! # It never prints a secret
//!
//! The bearer token exists in this process only as a SHA-256 digest
//! (`config::BearerToken`), no AWS credential is read by this crate at
//! all, and no signature is logged. The startup banner is deliberately
//! all-public: bind address, signer address, key id/alias, chain id,
//! verifying contract, token, and the policy ceilings.

use std::process::ExitCode;
use std::sync::Arc;

use glc_reserve_bridge_service::signing::evm_kms::{
    aws::AwsKmsDigestSigner,
    config::{self, SignerConfig},
    kms::prove_key_controls_address,
    server::{self, SignerService},
};

const USAGE: &str = "glc-robinhood-kms-signer — Robinhood EIP-712 authorization signer (AWS KMS)

  glc-robinhood-kms-signer

Serves, on the configured private/loopback bind address:

  GET  /v2/evm-identity     -> {\"address\":\"0x…\"}
  POST /v2/sign-evm-auth    -> {\"signature_hex\":\"…\"}

Both require `Authorization: Bearer <GLC_RHN_SIGNER_BEARER_TOKEN>`. There
is no endpoint that signs arbitrary bytes or an arbitrary digest.

Configuration is entirely environmental — this custody domain provisions
every value itself, and nothing is defaulted:

  GLC_RHN_SIGNER_BIND                 host:port (loopback or private only)
  GLC_RHN_SIGNER_KMS_KEY_ID           key id, ARN or alias
  GLC_RHN_SIGNER_BEARER_TOKEN         shared secret the bridge presents
  GLC_RHN_SIGNER_EVM_ADDRESS          this signer's address (proven at startup)
  GLC_RHN_SIGNER_CHAIN_ID             EIP-155 chain id
  GLC_RHN_SIGNER_VERIFYING_CONTRACT   deployed GlcRobinhoodBridge
  GLC_RHN_SIGNER_TOKEN                custodied ERC-20
  GLC_RHN_SIGNER_MAX_AMOUNT_ATOMIC    per-authorization ceiling, 18-decimal atomic
  GLC_RHN_SIGNER_MAX_TTL_SECS         authorization lifetime ceiling

Optional:

  GLC_RHN_SIGNER_ALLOWED_ACTIONS      subset of payout,refund,settlement
  GLC_RHN_SIGNER_ALLOWED_ROUTES       subset of GlcToRhn,RhnToGlc
  GLC_RHN_SIGNER_SIGNER_EPOCH         expected contract signerEpoch (unset = do not check)
  GLC_RHN_SIGNER_AWS_REGION           pins the AWS region

TLS is terminated by the deployment's own reverse proxy; this process
speaks plaintext HTTP and refuses to bind a public address.";

#[tokio::main]
async fn main() -> ExitCode {
    // This binary takes no configuration on the command line — every
    // value is provisioned by the custody domain's own environment — so
    // anything other than a help flag is a mistake worth naming rather
    // than ignoring.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [] => {}
        [flag] if flag == "-h" || flag == "--help" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        unexpected => {
            eprintln!(
                "unexpected argument(s) {unexpected:?} — this binary is configured entirely \
                 through the environment\n\n{USAGE}"
            );
            return ExitCode::from(2);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            tracing::error!("{message}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<(), String> {
    // ---- 1. configuration ----
    let SignerConfig {
        bind,
        kms_key_id,
        aws_region,
        bearer_token,
        evm_address,
        policy,
        governance,
    } = config::from_env().map_err(|e| format!("configuration is not usable: {e}"))?;

    tracing::info!(
        signer = %evm_address.to_checksum_string(),
        key = %kms_key_id,
        chain_id = policy.chain_id.get(),
        verifying_contract = %policy.verifying_contract.to_checksum_string(),
        token = %policy.token.to_checksum_string(),
        routes = ?policy.allowed_routes.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
        actions = ?policy.allowed_actions,
        max_amount_atomic = %policy.max_amount_robinhood_atomic,
        max_ttl_secs = policy.max_authorization_ttl_secs,
        expected_signer_epoch = ?policy.expected_signer_epoch,
        "configuration loaded"
    );

    // Said out loud, every start, in both postures. An operator reading
    // a signer's log must be able to see whether this process will sign
    // a governance action without inspecting its environment.
    if governance.is_enabled() {
        tracing::warn!(
            governance_actions = ?governance.allowed_actions,
            "GOVERNANCE SIGNING IS ENABLED on this signer: it will authorize the listed \
             GlcRobinhoodBridge governance actions under quorum. Value-moving authorizations are \
             unaffected and still bounded by the policy above"
        );
    } else {
        tracing::info!(
            "governance signing is DISABLED (GLC_RHN_SIGNER_ALLOWED_GOVERNANCE_ACTIONS unset); \
             POST /v3/sign-evm-governance refuses every request"
        );
    }

    // ---- 2. the KMS client, and the key's shape where readable ----
    let kms = AwsKmsDigestSigner::connect(&kms_key_id, aws_region.as_deref()).await;
    match kms.describe_key_shape().await {
        Ok(Some((spec, usage))) => {
            tracing::info!(key = %kms_key_id, %spec, %usage, "KMS key shape verified");
        }
        Ok(None) => {
            tracing::info!(
                key = %kms_key_id,
                "KMS key shape not readable; the startup identity proof is the authoritative check"
            );
        }
        Err(e) => return Err(format!("the configured KMS key is not usable: {e}")),
    }

    // ---- 3. prove the key controls the configured address ----
    let kms: Arc<dyn glc_reserve_bridge_service::signing::evm_kms::KmsDigestSigner> = Arc::new(kms);
    prove_key_controls_address(kms.as_ref(), evm_address)
        .await
        .map_err(|e| {
            format!(
                "the configured KMS key does not demonstrably control {} — refusing to start: {e}",
                evm_address.to_checksum_string()
            )
        })?;
    tracing::info!(
        signer = %evm_address.to_checksum_string(),
        key = %kms_key_id,
        "KMS key proven to control the configured signer address"
    );

    // ---- serve ----
    let service = Arc::new(
        SignerService::new(evm_address, policy, bearer_token, kms).with_governance(governance),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    server::serve(bind, service, shutdown_rx)
        .await
        .map_err(|e| format!("could not serve on {bind}: {e}"))
}

/// Same graceful-shutdown discipline as `glc-bridge-daemon`: an in-flight
/// signing request always finishes, and a second signal is not needed.
async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        _ = ctrl_c => tracing::info!("SIGINT received"),
        _ = terminate => tracing::info!("SIGTERM received"),
    }
}
