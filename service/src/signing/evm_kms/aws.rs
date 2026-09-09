//! The only AWS-SDK-aware code in this crate.
//!
//! # `MessageType = DIGEST`, and why it is not negotiable
//!
//! [`crate::signing::evm_policy::EvmAuthDecision::digest`] is ALREADY the
//! final EIP-712 typed-data hash — `keccak256(0x19 || 0x01 ||
//! domainSeparator || structHash)`, the exact 32 bytes
//! `GlcRobinhoodBridge._authorize` recovers against. Sending it with
//! `MessageType::Raw` would make KMS hash it a second time (with SHA-256,
//! at that) and sign `sha256(keccak256(...))`, which recovers to the
//! right key over the wrong message: every such signature would be
//! rejected on-chain, and none of them would be attributable to a bug in
//! the policy that approved it.
//!
//! [`aws_sdk_kms::types::SigningAlgorithmSpec::EcdsaSha256`] is the
//! algorithm because it is the only one KMS supports for an
//! `ECC_SECG_P256K1` key. With `MessageType::Digest` the SHA-256 in that
//! name describes the digest KMS believes it was given, not a hash it
//! applies — the 32 bytes are signed as the scalar, which is exactly what
//! `ecrecover` expects.
//!
//! # No credential ever passes through this file
//!
//! [`aws_config`]'s default provider chain resolves the host's identity
//! (environment, shared profile, container role, IMDS/instance role).
//! This module names no credential, reads no credential variable, and has
//! no field that could hold one. The deployment's IAM policy is what
//! grants `kms:Sign` on the one key; this process merely asks.
//!
//! # The TLS stack is pinned deliberately
//!
//! The connector is constructed explicitly with rustls over `ring` rather
//! than taken from the SDK's `default-https-client` feature, which would
//! enable `rustls/aws_lc_rs`. See `service/Cargo.toml`: `rustls` is a
//! shared crate here, and Cargo feature unification would have made
//! aws-lc the process-wide default crypto provider for the EXISTING
//! daemon's RPC connections too.

use aws_sdk_kms::error::ProvideErrorMetadata;
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::types::{KeySpec, KeyUsageType, MessageType, SigningAlgorithmSpec};
use aws_smithy_http_client::{tls, Builder as HttpClientBuilder};

use super::kms::{BoxFut, KmsDigestSigner, KmsError};

/// The KMS key spec an EVM authorization signer must be.
pub const REQUIRED_KEY_SPEC: KeySpec = KeySpec::EccSecgP256K1;
/// The KMS key usage it must have.
pub const REQUIRED_KEY_USAGE: KeyUsageType = KeyUsageType::SignVerify;

/// A signing backend that is one AWS KMS key.
///
/// Holds a client and a key id. It cannot hold, fetch or export key
/// material: the private key never leaves KMS, and the SDK offers this
/// process no API that would return it.
#[derive(Debug, Clone)]
pub struct AwsKmsDigestSigner {
    client: aws_sdk_kms::Client,
    /// Key id, ARN, or alias — public, and safe to log.
    key_id: String,
}

impl AwsKmsDigestSigner {
    /// Builds a client from the host's own AWS configuration.
    ///
    /// `region` pins the region explicitly when the deployment prefers
    /// that over `AWS_REGION`/profile resolution; `None` uses the
    /// standard chain.
    pub async fn connect(key_id: &str, region: Option<&str>) -> AwsKmsDigestSigner {
        let http_client = HttpClientBuilder::new()
            .tls_provider(tls::Provider::Rustls(
                tls::rustls_provider::CryptoMode::Ring,
            ))
            .build_https();

        let mut loader =
            aws_config::defaults(aws_config::BehaviorVersion::latest()).http_client(http_client);
        if let Some(region) = region {
            loader = loader.region(aws_config::Region::new(region.to_string()));
        }
        let shared = loader.load().await;
        AwsKmsDigestSigner {
            client: aws_sdk_kms::Client::new(&shared),
            key_id: key_id.to_string(),
        }
    }

    /// `kms:DescribeKey`, checking the two properties that make a key
    /// usable here at all.
    ///
    /// Returns `Ok(None)` when the key's shape could not be READ — most
    /// commonly because the deployment's IAM policy grants `kms:Sign`
    /// without `kms:DescribeKey`, which is a legitimate least-privilege
    /// posture. That is not treated as a failure, because it is not the
    /// authoritative check: [`super::kms::prove_key_controls_address`]
    /// is, and it runs regardless. A key of the wrong spec or usage
    /// cannot produce a signature that recovers to the configured
    /// address, so the proof catches it either way — this call just
    /// turns that into a precise message instead of a puzzling one.
    ///
    /// A key whose shape IS readable and is wrong returns `Err`.
    pub async fn describe_key_shape(&self) -> Result<Option<(KeySpec, KeyUsageType)>, KmsError> {
        let described = match self.client.describe_key().key_id(&self.key_id).send().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    key_id = %self.key_id,
                    detail = %describe_sdk_error(&e),
                    "could not read KMS key metadata (kms:DescribeKey may not be granted); \
                     relying on the startup identity proof alone"
                );
                return Ok(None);
            }
        };
        let Some(metadata) = described.key_metadata() else {
            return Ok(None);
        };
        let spec = metadata.key_spec().cloned();
        let usage = metadata.key_usage().cloned();
        let (Some(spec), Some(usage)) = (spec, usage) else {
            return Ok(None);
        };
        if spec != REQUIRED_KEY_SPEC {
            return Err(KmsError::WrongKeyShape(format!(
                "key {} has spec {spec}, but an EVM authorization signer requires \
                 {REQUIRED_KEY_SPEC}",
                self.key_id
            )));
        }
        if usage != REQUIRED_KEY_USAGE {
            return Err(KmsError::WrongKeyShape(format!(
                "key {} has usage {usage}, but signing requires {REQUIRED_KEY_USAGE}",
                self.key_id
            )));
        }
        Ok(Some((spec, usage)))
    }
}

impl KmsDigestSigner for AwsKmsDigestSigner {
    fn key_label(&self) -> &str {
        &self.key_id
    }

    fn sign_digest<'a>(&'a self, digest: &'a [u8; 32]) -> BoxFut<'a, Result<Vec<u8>, KmsError>> {
        Box::pin(async move {
            let response = self
                .client
                .sign()
                .key_id(&self.key_id)
                // The 32 bytes, verbatim. Nothing is prefixed, padded or
                // re-hashed on the way in.
                .message(Blob::new(digest.to_vec()))
                .message_type(MessageType::Digest)
                .signing_algorithm(SigningAlgorithmSpec::EcdsaSha256)
                .send()
                .await
                .map_err(|e| classify_sdk_error(&e))?;

            let signature = response
                .signature()
                .ok_or(KmsError::EmptySignature)?
                .as_ref()
                .to_vec();
            if signature.is_empty() {
                return Err(KmsError::EmptySignature);
            }
            Ok(signature)
        })
    }
}

/// Maps an SDK error onto the retriable/not-retriable split
/// [`KmsError`] models.
///
/// Deliberately coarse. A signer that tried to enumerate every KMS error
/// code would drift out of date against the service; what the HTTP layer
/// actually needs to know is whether the bridge should come back next
/// tick, and the SDK already answers that with its own transport/timeout
/// classification.
fn classify_sdk_error<E, R>(err: &aws_sdk_kms::error::SdkError<E, R>) -> KmsError
where
    E: std::fmt::Debug + ProvideErrorMetadata,
    R: std::fmt::Debug,
{
    use aws_sdk_kms::error::SdkError;
    let detail = describe_sdk_error(err);
    match err {
        // Reached nobody, or reached them too slowly: retry next tick.
        SdkError::DispatchFailure(_) | SdkError::TimeoutError(_) => KmsError::Unavailable(detail),
        // KMS answered. Whether that answer was AccessDenied, a disabled
        // key or throttling, the request as sent will keep producing it
        // until an operator changes something.
        _ => KmsError::Refused(detail),
    }
}

/// Renders an SDK error as the service's own error code and message,
/// falling back to the SDK's `Debug` shape for transport-level failures
/// that have neither.
///
/// AWS SDK errors never carry the secret access key or session token —
/// they carry request ids, error codes and, at most, the key ARN, all of
/// which are the things an operator needs.
fn describe_sdk_error<E, R>(err: &aws_sdk_kms::error::SdkError<E, R>) -> String
where
    E: std::fmt::Debug + ProvideErrorMetadata,
    R: std::fmt::Debug,
{
    let rendered = match (err.code(), err.message()) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_string(),
        _ => format!("{err:?}"),
    };
    cap(&rendered)
}

/// Caps a rendering so a `Debug` blob (which can run to several kilobytes
/// of nested source chains) cannot dominate a log line or a response
/// body. About volume, not confidentiality.
fn cap(rendered: &str) -> String {
    const MAX: usize = 400;
    let cleaned = rendered.replace('\n', " ");
    if cleaned.chars().count() <= MAX {
        return cleaned;
    }
    let truncated: String = cleaned.chars().take(MAX).collect();
    format!("{truncated}… (truncated)")
}
