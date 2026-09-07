//! Proofs that no configured endpoint, and no credential embedded in
//! one, can survive into text bound for an operator-facing surface.
//!
//! The tests are written against the STRING that reqwest and the JSON-RPC
//! layer actually produce, not against a synthetic sample, because the
//! whole point of this module is that it works on text this crate did not
//! write.

use super::*;

/// The shape `reqwest::Error`'s `Display` produces for a connection
/// failure — the exact string that reached
/// `RobinhoodHealthSnapshot::last_rpc_error` before this module existed.
const REQWEST_STYLE: &str =
    "error sending request for url (https://apikey:s3cr3tpassw0rd@rpc.robinhood.example:8545/v2/AbCdEf0123456789)";

const ENDPOINT: &str =
    "https://apikey:s3cr3tpassw0rd@rpc.robinhood.example:8545/v2/AbCdEf0123456789";

/// Every fragment of the endpoint that must never appear in output.
const SECRETS: &[&str] = &[
    ENDPOINT,
    "apikey:s3cr3tpassw0rd",
    "s3cr3tpassw0rd",
    "rpc.robinhood.example",
    "AbCdEf0123456789",
];

fn assert_clean(text: &str) {
    let lower = text.to_ascii_lowercase();
    for secret in SECRETS {
        assert!(
            !lower.contains(&secret.to_ascii_lowercase()),
            "redacted output still contains {secret:?}:\n  {text}"
        );
    }
    assert!(
        !lower.contains("://"),
        "redacted output still contains a scheme-qualified URL:\n  {text}"
    );
}

#[test]
fn a_credential_bearing_rpc_url_cannot_survive_redaction() {
    let redactor = Redactor::for_endpoint(ENDPOINT);
    let out = redactor.apply(REQWEST_STYLE);
    assert_clean(&out);
    // The diagnostic prose survives — this is what makes the surface
    // worth reading at all.
    assert!(out.contains("error sending request"), "{out}");
    assert!(out.contains(REDACTED_ENDPOINT), "{out}");
}

#[test]
fn the_full_indexer_error_wrapper_is_redacted_too() {
    // What `RobinhoodIndexerError::NodeUnavailable(Transport(..))`
    // renders as, wrapper prose and all.
    let full = format!(
        "Robinhood EVM endpoint unavailable: transport error contacting the Robinhood EVM RPC: {REQWEST_STYLE}"
    );
    let out = Redactor::for_endpoint(ENDPOINT).apply(&full);
    assert_clean(&out);
    assert!(out.contains("Robinhood EVM endpoint unavailable"), "{out}");
}

/// Pass 1 needs no configuration at all: a URL this deployment never
/// configured — a redirect target, a proxy, an upstream named by the node
/// — is removed on shape alone.
#[test]
fn an_unconfigured_url_is_still_removed() {
    let out = Redactor::none()
        .apply("upstream failed: https://someone:else@other.example/path?key=abc then gave up");
    assert!(!out.contains("://"), "{out}");
    assert!(!out.contains("other.example"), "{out}");
    assert!(!out.contains("someone:else"), "{out}");
    assert!(out.contains("upstream failed:"), "{out}");
    assert!(out.contains("then gave up"), "{out}");
}

/// Pass 2 catches what pass 1 cannot: a bare hostname, with no scheme, in
/// a DNS resolution error.
#[test]
fn a_bare_hostname_from_a_dns_error_is_removed() {
    let out = Redactor::for_endpoint(ENDPOINT)
        .apply("dns error: failed to lookup address information for rpc.robinhood.example");
    assert_clean(&out);
    assert!(
        out.contains("failed to lookup address information"),
        "{out}"
    );
}

/// And a password echoed on its own, with no URL around it.
#[test]
fn a_bare_password_is_removed() {
    let out = Redactor::for_endpoint(ENDPOINT).apply("auth rejected for token s3cr3tpassw0rd");
    assert_clean(&out);
    assert!(out.contains("auth rejected"), "{out}");
}

/// A provider that puts the API key in the path rather than in userinfo
/// is the common real-world shape, and the key is just as secret there.
#[test]
fn an_api_key_in_the_path_is_removed_even_on_its_own() {
    let redactor = Redactor::for_endpoint("https://rpc.example/v2/9f8e7d6c5b4a3210");
    let out = redactor.apply("provider says: project 9f8e7d6c5b4a3210 is over quota");
    assert!(!out.contains("9f8e7d6c5b4a3210"), "{out}");
    assert!(out.contains("is over quota"), "{out}");
}

/// DNS is case-insensitive and reqwest lowercases hosts, so a config
/// written with different casing is the same secret.
#[test]
fn matching_is_case_insensitive() {
    let redactor = Redactor::for_endpoint("https://RPC.Robinhood.Example:8545");
    let out = redactor.apply("connection refused by rpc.robinhood.example:8545");
    assert!(
        !out.to_ascii_lowercase().contains("robinhood.example"),
        "{out}"
    );
}

/// Pass 3, on a userinfo run that survived both earlier passes because it
/// belongs to no configured endpoint and carries no scheme.
#[test]
fn a_residual_userinfo_run_is_removed() {
    let out = Redactor::none().apply("proxy auth failed for admin:hunter2@internal.host");
    assert!(!out.contains("hunter2"), "{out}");
    assert!(!out.contains("internal.host"), "{out}");
    assert!(out.contains("proxy auth failed"), "{out}");
}

/// Over-redaction has a real operational cost, so the things an operator
/// triages on must survive: chain ids, block numbers, contract addresses,
/// transaction hashes and JSON-RPC codes are not secrets.
#[test]
fn diagnostic_detail_is_preserved() {
    let redactor = Redactor::for_endpoint(ENDPOINT);
    let message = "Robinhood EVM RPC method error (code -32005): query returned more than 10000 \
                   results for block 4663991 at contract 0xAbC0000000000000000000000000000000000001";
    let out = redactor.apply(message);
    assert_eq!(out, message, "nothing in this message is a secret");
}

#[test]
fn a_message_with_nothing_to_redact_is_returned_unchanged() {
    let redactor = Redactor::for_endpoint(ENDPOINT);
    assert_eq!(redactor.apply("head is 12345"), "head is 12345");
    assert_eq!(Redactor::none().apply("head is 12345"), "head is 12345");
}

/// Applying redaction twice must not corrupt output — the markers
/// themselves must not look like something to redact.
#[test]
fn redaction_is_idempotent() {
    let redactor = Redactor::for_endpoint(ENDPOINT);
    let once = redactor.apply(REQWEST_STYLE);
    let twice = redactor.apply(&once);
    assert_eq!(once, twice);
}

/// Original spacing survives, so a redacted message still reads as prose
/// rather than as a re-joined token list.
#[test]
fn whitespace_is_preserved_exactly() {
    let out = Redactor::none().apply("a  b\tc\nd");
    assert_eq!(out, "a  b\tc\nd");
}

/// A `://` that is not a URL must not swallow the rest of the line.
#[test]
fn a_bare_scheme_separator_is_not_treated_as_a_url() {
    let out = Redactor::none().apply("ratio 3://4 is meaningless");
    assert!(out.contains("is meaningless"), "{out}");
}

#[test]
fn an_empty_message_is_handled() {
    assert_eq!(Redactor::for_endpoint(ENDPOINT).apply(""), "");
    assert_eq!(Redactor::for_endpoint("").literal_count(), 0);
}

/// A malformed configured URL must still yield literals — an operator's
/// typo is exactly when errors get logged, and a parser that rejected the
/// input would leave the credential unprotected at that moment.
#[test]
fn a_malformed_endpoint_still_yields_literals() {
    let redactor = Redactor::for_endpoint("htp:/apikey:hunter2@host.example");
    assert!(redactor.literal_count() > 0);
    let out = redactor.apply("failed talking to host.example with hunter2");
    assert!(!out.contains("host.example"), "{out}");
    assert!(!out.contains("hunter2"), "{out}");
}

/// A one- or two-character userinfo component is deliberately NOT a
/// literal (see `MIN_LITERAL_SECRET_LEN`) — but it is still removed
/// whenever it appears inside the URL it belongs to, which is the only
/// place it realistically appears.
#[test]
fn a_degenerate_short_credential_is_still_stripped_inside_its_url() {
    let redactor = Redactor::for_endpoint("https://a:b@host.example/x");
    let out = redactor.apply("error sending request for url (https://a:b@host.example/x)");
    assert!(!out.contains("://"), "{out}");
    assert!(!out.contains("host.example"), "{out}");
}

/// Multiple URLs in one message are each removed.
#[test]
fn every_url_in_a_message_is_removed() {
    let out = Redactor::none()
        .apply("redirected from https://a.example/1 to https://b.example/2 and failed");
    assert!(!out.contains("a.example"), "{out}");
    assert!(!out.contains("b.example"), "{out}");
    assert_eq!(out.matches(REDACTED_ENDPOINT).count(), 2, "{out}");
    assert!(out.contains("and failed"), "{out}");
}

/// A URL at the very end of a message, with no trailing terminator.
#[test]
fn a_trailing_url_is_removed() {
    let out = Redactor::none().apply("could not reach https://rpc.example:8545/path");
    assert!(!out.contains("rpc.example"), "{out}");
    assert!(out.ends_with(REDACTED_ENDPOINT), "{out}");
}
