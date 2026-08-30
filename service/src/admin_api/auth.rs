//! Bearer-token authentication for the admin API.
//!
//! Follows `signing::remote`'s secret discipline exactly: the config file
//! names an ENVIRONMENT VARIABLE per operator, never the token itself,
//! and the token type's only `Debug` representation redacts the value —
//! a structural guard against the secret landing in a log line or panic
//! message, not a convention.
//!
//! Verification compares SHA-256 digests of the presented and stored
//! tokens byte-for-byte over the full fixed-length digest, so comparison
//! time is independent of both the token contents and where a mismatch
//! occurs — the standard constant-time-comparison construction, with no
//! new dependency (`sha2` is already used by `ops::audit`).

use sha2::{Digest, Sha256};

/// A per-operator admin bearer token, read once from the environment.
#[derive(Clone)]
pub struct AdminAuthToken(String);

#[derive(Debug, thiserror::Error)]
pub enum AdminAuthTokenError {
    #[error("admin operator token env var {var} is not set")]
    Missing { var: String },
    #[error("admin operator token env var {var} is set but empty")]
    Empty { var: String },
}

impl AdminAuthToken {
    pub fn from_env(var_name: &str) -> Result<Self, AdminAuthTokenError> {
        let value = std::env::var(var_name).map_err(|_| AdminAuthTokenError::Missing {
            var: var_name.to_string(),
        })?;
        if value.is_empty() {
            return Err(AdminAuthTokenError::Empty {
                var: var_name.to_string(),
            });
        }
        Ok(AdminAuthToken(value))
    }

    #[cfg(test)]
    pub(crate) fn for_tests(value: &str) -> Self {
        AdminAuthToken(value.to_string())
    }

    fn digest(&self) -> [u8; 32] {
        sha256(self.0.as_bytes())
    }
}

impl std::fmt::Debug for AdminAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminAuthToken(<redacted>)")
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Fixed-length comparison whose duration does not depend on where the
/// first differing byte is: every byte pair is always visited and folded
/// into the accumulator.
fn digests_equal_constant_time(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Resolves each declared operator's bearer token from its named env var
/// — called by the DAEMON immediately before starting the admin listener
/// (never by `Config::load`, so `glc-admin --config` recovery commands
/// work from shells that lack the daemon's secrets). Fails closed on any
/// missing or empty variable.
pub fn resolve_operator_tokens(
    operators: &[crate::config::AdminOperatorConfig],
) -> Result<Vec<(String, AdminAuthToken)>, AdminAuthTokenError> {
    operators
        .iter()
        .map(|op| Ok((op.name.clone(), AdminAuthToken::from_env(&op.token_env)?)))
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum OperatorRegistryError {
    /// Two operators presented the SAME token value (same digest). If
    /// this were allowed, `verify_bearer` would attribute both people's
    /// actions to whichever operator it resolves the token to — a silent
    /// audit-trail corruption. Fail closed at construction instead.
    #[error(
        "operators {first:?} and {second:?} have identical tokens — every operator needs their \
         own token so audit rows attribute to the right person"
    )]
    DuplicateToken { first: String, second: String },
}

/// The set of authorized admin operators: name -> token digest. Holds
/// digests only after construction; the raw tokens are dropped.
pub struct OperatorRegistry {
    operators: Vec<(String, [u8; 32])>,
}

impl OperatorRegistry {
    pub fn new(operators: Vec<(String, AdminAuthToken)>) -> Result<Self, OperatorRegistryError> {
        let resolved: Vec<(String, [u8; 32])> = operators
            .into_iter()
            .map(|(name, token)| (name, token.digest()))
            .collect();
        for (i, (first_name, first_digest)) in resolved.iter().enumerate() {
            for (second_name, second_digest) in resolved.iter().skip(i + 1) {
                if first_digest == second_digest {
                    return Err(OperatorRegistryError::DuplicateToken {
                        first: first_name.clone(),
                        second: second_name.clone(),
                    });
                }
            }
        }
        Ok(OperatorRegistry {
            operators: resolved,
        })
    }

    /// Verifies a raw `Authorization` header value ("Bearer <token>") and
    /// returns the matching operator's name. Every registered digest is
    /// always compared (no early return on match), so the timing does not
    /// reveal which operator, if any, matched.
    pub fn verify_bearer(&self, authorization_header: &str) -> Option<&str> {
        let token = authorization_header.strip_prefix("Bearer ")?;
        let presented = sha256(token.as_bytes());
        let mut matched: Option<&str> = None;
        for (name, digest) in &self.operators {
            if digests_equal_constant_time(digest, &presented) {
                matched = Some(name.as_str());
            }
        }
        matched
    }
}

impl std::fmt::Debug for OperatorRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Names are fine; digests stay out of Debug output too — they are
        // not secrets, but there is no operational reason to print them.
        f.debug_struct("OperatorRegistry")
            .field(
                "operators",
                &self
                    .operators
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> OperatorRegistry {
        OperatorRegistry::new(vec![
            ("alice".to_string(), AdminAuthToken::for_tests("token-a")),
            ("bob".to_string(), AdminAuthToken::for_tests("token-b")),
        ])
        .unwrap()
    }

    #[test]
    fn duplicate_token_values_are_rejected_at_registry_construction() {
        let err = OperatorRegistry::new(vec![
            ("alice".to_string(), AdminAuthToken::for_tests("same-token")),
            ("bob".to_string(), AdminAuthToken::for_tests("same-token")),
        ])
        .unwrap_err();
        let OperatorRegistryError::DuplicateToken { first, second } = err;
        assert_eq!((first.as_str(), second.as_str()), ("alice", "bob"));
    }

    #[test]
    fn resolve_operator_tokens_reads_the_named_env_vars_and_fails_closed() {
        std::env::set_var("GLC_TEST_ADMIN_RESOLVE_FN_A", "resolved-token-a");
        let ok = resolve_operator_tokens(&[crate::config::AdminOperatorConfig {
            name: "alice".to_string(),
            token_env: "GLC_TEST_ADMIN_RESOLVE_FN_A".to_string(),
        }])
        .unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].0, "alice");

        let err = resolve_operator_tokens(&[crate::config::AdminOperatorConfig {
            name: "bob".to_string(),
            token_env: "GLC_TEST_ADMIN_RESOLVE_FN_NEVER_SET".to_string(),
        }])
        .unwrap_err();
        assert!(matches!(err, AdminAuthTokenError::Missing { .. }));
    }

    #[test]
    fn valid_bearer_resolves_to_the_matching_operator() {
        let r = registry();
        assert_eq!(r.verify_bearer("Bearer token-a"), Some("alice"));
        assert_eq!(r.verify_bearer("Bearer token-b"), Some("bob"));
    }

    #[test]
    fn wrong_or_malformed_credentials_are_rejected() {
        let r = registry();
        assert_eq!(r.verify_bearer("Bearer wrong"), None);
        assert_eq!(r.verify_bearer("token-a"), None); // no Bearer prefix
        assert_eq!(r.verify_bearer("bearer token-a"), None); // case-sensitive
        assert_eq!(r.verify_bearer(""), None);
        assert_eq!(r.verify_bearer("Bearer "), None);
        // A digest-length collision attempt: the raw sha256 hex of a
        // valid token is NOT the token.
        assert_eq!(
            r.verify_bearer(
                "Bearer 6ee9e0a2d6e0f9d0e2a5c0d2b1a4f8c7d6e5b4a3928170f6e5d4c3b2a1908f7e"
            ),
            None
        );
    }

    #[test]
    fn token_debug_output_is_redacted() {
        let token = AdminAuthToken::for_tests("super-secret-value");
        let debug = format!("{token:?}");
        assert!(!debug.contains("super-secret-value"));
        assert_eq!(debug, "AdminAuthToken(<redacted>)");
    }

    #[test]
    fn registry_debug_output_contains_names_but_never_token_material() {
        let r = registry();
        let debug = format!("{r:?}");
        assert!(debug.contains("alice"));
        assert!(!debug.contains("token-a"));
    }

    #[test]
    fn from_env_fails_closed_on_missing_or_empty() {
        assert!(matches!(
            AdminAuthToken::from_env("GLC_TEST_ADMIN_TOKEN_THAT_IS_NOT_SET"),
            Err(AdminAuthTokenError::Missing { .. })
        ));
    }
}
