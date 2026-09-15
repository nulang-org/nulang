//! Typed external-authority capabilities.
//!
//! Nulang already has *reference capabilities* (`iso`, `trn`, `ref`, ...)
//! governing aliasing and mutation. This module is deliberately separate:
//! authority grants govern which external resources an actor/computation may
//! access. Keeping the two concepts distinct avoids overloading the word
//! "capability" in compiler and runtime code.
//!
//! The current spawn pipeline still carries canonical authority tokens as
//! strings. `AuthorityGrant` is the migration target and provides a strict
//! parser/formatter so existing tokens can cross that boundary without making
//! security-sensitive runtime decisions on ad-hoc string matching.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::str::FromStr;

/// A structured external authority granted to an actor or computation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AuthorityGrant {
    /// Permission to open an outbound TCP connection to one host and port.
    NetTcpOut { host: String, port: u16 },
    /// Permission to read a filesystem path/pattern.
    FsRead { path: String },
    /// Permission to write a filesystem path/pattern.
    FsWrite { path: String },
    /// Permission to read one environment variable.
    EnvRead { name: String },
    /// Permission to read one named secret.
    SecretRead { name: String },
    /// A namespaced extension authority. This keeps extension points typed
    /// without silently treating an arbitrary opaque string as permission.
    Other {
        namespace: String,
        operation: String,
        argument: Option<String>,
    },
}

impl AuthorityGrant {
    /// Build and validate a grant from the source-level structural pieces.
    ///
    /// Parser code should use this rather than constructing an unchecked
    /// canonical token by hand. The canonical string representation remains
    /// an encoding boundary for bytecode/persistence compatibility, not the
    /// semantic representation of authority.
    pub fn from_parts(
        namespace: &str,
        operation: &str,
        argument: Option<&str>,
    ) -> Result<Self, AuthorityParseError> {
        let token = match argument {
            Some(argument) => format!("{namespace}::{operation}({argument})"),
            None => format!("{namespace}::{operation}"),
        };
        token.parse()
    }
}

/// Deterministically ordered authority set. Empty means no external authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorityManifest {
    grants: BTreeSet<AuthorityGrant>,
}

impl AuthorityManifest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_grants(grants: impl IntoIterator<Item = AuthorityGrant>) -> Self {
        Self {
            grants: grants.into_iter().collect(),
        }
    }

    /// Compatibility bridge for existing bytecode/persistence/runtime token
    /// sets. Invalid persisted authority fails closed instead of being kept as
    /// an opaque string that a host function could accidentally authorize.
    pub fn from_tokens<'a>(
        tokens: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, AuthorityParseError> {
        let mut grants = BTreeSet::new();
        for token in tokens {
            grants.insert(token.parse()?);
        }
        Ok(Self { grants })
    }

    /// Convenience bridge for the runtime's current `BTreeSet<String>` actor
    /// field while it migrates to `AuthorityManifest` directly.
    pub fn from_token_set(tokens: &BTreeSet<String>) -> Result<Self, AuthorityParseError> {
        Self::from_tokens(tokens.iter().map(String::as_str))
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    pub fn len(&self) -> usize {
        self.grants.len()
    }

    pub fn contains(&self, grant: &AuthorityGrant) -> bool {
        self.grants.contains(grant)
    }

    /// Generic typed authorization check. Host boundaries should construct
    /// the exact grant they require and call this method; category-specific
    /// helpers are ergonomic wrappers around the same exact-match rule.
    pub fn allows(&self, grant: &AuthorityGrant) -> bool {
        self.contains(grant)
    }

    pub fn allows_tcp_out(&self, host: &str, port: u16) -> bool {
        self.allows(&AuthorityGrant::NetTcpOut {
            host: host.to_string(),
            port,
        })
    }

    /// Return true when every authority in this manifest is already present
    /// in `parent`. This is the primitive monotonic-delegation invariant:
    /// delegation may remove authority but cannot manufacture new authority.
    ///
    /// This intentionally uses exact grants only. Resource-specific semantic
    /// attenuation (for example proving `/uploads/public/**` is narrower than
    /// `/uploads/**`) belongs in explicit per-resource policies rather than an
    /// unsafe generic string-prefix rule.
    pub fn is_subset_of(&self, parent: &AuthorityManifest) -> bool {
        self.grants.is_subset(&parent.grants)
    }

    /// Intersect this manifest with an allow-list. The result can only have
    /// equal or less authority than `self`, making it safe for child actors,
    /// sandboxed handlers, and delegated execution contexts.
    pub fn restrict_to(&self, allowed: &AuthorityManifest) -> AuthorityManifest {
        AuthorityManifest {
            grants: self.grants.intersection(&allowed.grants).cloned().collect(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &AuthorityGrant> {
        self.grants.iter()
    }

    /// Stable tokens for bytecode metadata, persistence, hashing, and the
    /// existing runtime bridge while those layers migrate to typed grants.
    ///
    /// Serialization order is lexical by canonical token, not enum variant
    /// declaration order. This prevents an internal enum reordering from
    /// changing stable metadata or content identities.
    pub fn canonical_tokens(&self) -> Vec<String> {
        let mut tokens: Vec<_> = self.grants.iter().map(ToString::to_string).collect();
        tokens.sort_unstable();
        tokens
    }

    /// Canonical compatibility representation for the runtime's current actor
    /// field. New semantic code should prefer `AuthorityManifest` directly.
    pub fn canonical_token_set(&self) -> BTreeSet<String> {
        self.canonical_tokens().into_iter().collect()
    }
}

impl fmt::Display for AuthorityGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthorityGrant::NetTcpOut { host, port } => {
                write!(f, "Net::TcpOut({host}:{port})")
            }
            AuthorityGrant::FsRead { path } => write!(f, "Fs::Read({path})"),
            AuthorityGrant::FsWrite { path } => write!(f, "Fs::Write({path})"),
            AuthorityGrant::EnvRead { name } => write!(f, "Env::Read({name})"),
            AuthorityGrant::SecretRead { name } => write!(f, "Secret::Read({name})"),
            AuthorityGrant::Other {
                namespace,
                operation,
                argument,
            } => match argument {
                Some(argument) => write!(f, "{namespace}::{operation}({argument})"),
                None => write!(f, "{namespace}::{operation}"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityParseError {
    token: String,
    reason: &'static str,
}

impl AuthorityParseError {
    fn new(token: &str, reason: &'static str) -> Self {
        Self {
            token: token.to_string(),
            reason,
        }
    }
}

impl fmt::Display for AuthorityParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid authority token '{}': {}",
            self.token, self.reason
        )
    }
}

impl Error for AuthorityParseError {}

impl FromStr for AuthorityGrant {
    type Err = AuthorityParseError;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthorityParseError::new(token, "token is empty"));
        }

        let (head, argument) = split_token(token)?;
        let (namespace, operation) = head
            .split_once("::")
            .ok_or_else(|| AuthorityParseError::new(token, "expected Namespace::Operation"))?;
        if namespace.is_empty() || operation.is_empty() {
            return Err(AuthorityParseError::new(
                token,
                "namespace and operation must be non-empty",
            ));
        }

        match (namespace, operation) {
            ("Net", "TcpOut") => {
                let endpoint = require_argument(token, argument)?;
                let (host, port) = endpoint
                    .rsplit_once(':')
                    .ok_or_else(|| AuthorityParseError::new(token, "TcpOut expects host:port"))?;
                if host.is_empty() {
                    return Err(AuthorityParseError::new(token, "host must be non-empty"));
                }
                let port = port
                    .parse::<u16>()
                    .map_err(|_| AuthorityParseError::new(token, "port must be a valid u16"))?;
                if port == 0 {
                    return Err(AuthorityParseError::new(
                        token,
                        "port must be in the range 1..=65535",
                    ));
                }
                Ok(AuthorityGrant::NetTcpOut {
                    host: host.to_string(),
                    port,
                })
            }
            ("Fs", "Read") => Ok(AuthorityGrant::FsRead {
                path: require_nonempty_argument(token, argument)?.to_string(),
            }),
            ("Fs", "Write") => Ok(AuthorityGrant::FsWrite {
                path: require_nonempty_argument(token, argument)?.to_string(),
            }),
            ("Env", "Read") => Ok(AuthorityGrant::EnvRead {
                name: require_nonempty_argument(token, argument)?.to_string(),
            }),
            ("Secret", "Read") => Ok(AuthorityGrant::SecretRead {
                name: require_nonempty_argument(token, argument)?.to_string(),
            }),
            _ => Ok(AuthorityGrant::Other {
                namespace: namespace.to_string(),
                operation: operation.to_string(),
                argument: argument.map(str::to_string),
            }),
        }
    }
}

/// Split `Namespace::Operation(argument)` into its head and optional argument.
/// Nested parentheses are intentionally not accepted: authority tokens are
/// metadata, not an expression language.
fn split_token(token: &str) -> Result<(&str, Option<&str>), AuthorityParseError> {
    let Some(open) = token.find('(') else {
        if token.contains(')') {
            return Err(AuthorityParseError::new(token, "unmatched ')'"));
        }
        return Ok((token, None));
    };

    if !token.ends_with(')') {
        return Err(AuthorityParseError::new(token, "unclosed '('"));
    }
    let head = &token[..open];
    let argument = &token[open + 1..token.len() - 1];
    if argument.contains('(') || argument.contains(')') {
        return Err(AuthorityParseError::new(
            token,
            "nested parentheses are not allowed",
        ));
    }
    Ok((head, Some(argument)))
}

fn require_argument<'a>(
    token: &str,
    argument: Option<&'a str>,
) -> Result<&'a str, AuthorityParseError> {
    argument.ok_or_else(|| AuthorityParseError::new(token, "operation requires an argument"))
}

fn require_nonempty_argument<'a>(
    token: &str,
    argument: Option<&'a str>,
) -> Result<&'a str, AuthorityParseError> {
    let argument = require_argument(token, argument)?;
    if argument.is_empty() {
        return Err(AuthorityParseError::new(
            token,
            "argument must be non-empty",
        ));
    }
    Ok(argument)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcp_out_round_trips_canonically() {
        let grant: AuthorityGrant = "Net::TcpOut(api.stripe.com:443)".parse().unwrap();
        assert_eq!(
            grant,
            AuthorityGrant::NetTcpOut {
                host: "api.stripe.com".into(),
                port: 443,
            }
        );
        assert_eq!(grant.to_string(), "Net::TcpOut(api.stripe.com:443)");
    }

    #[test]
    fn source_parts_share_the_same_validation_path() {
        let grant =
            AuthorityGrant::from_parts("Net", "TcpOut", Some("api.stripe.com:443")).unwrap();
        assert_eq!(grant.to_string(), "Net::TcpOut(api.stripe.com:443)");
        assert!(AuthorityGrant::from_parts("Net", "TcpOut", Some("api.stripe.com:0")).is_err());
        assert!(AuthorityGrant::from_parts("Fs", "Read", Some("")).is_err());
    }

    #[test]
    fn manifest_is_deny_by_default() {
        let manifest = AuthorityManifest::new();
        assert!(!manifest.allows_tcp_out("api.stripe.com", 443));
    }

    #[test]
    fn manifest_allows_only_exact_tcp_grant() {
        let manifest = AuthorityManifest::from_tokens(["Net::TcpOut(api.stripe.com:443)"]).unwrap();
        assert!(manifest.allows_tcp_out("api.stripe.com", 443));
        assert!(!manifest.allows_tcp_out("api.stripe.com", 80));
        assert!(!manifest.allows_tcp_out("example.com", 443));
    }

    #[test]
    fn current_runtime_token_set_converts_fail_closed() {
        let tokens = BTreeSet::from(["Net::TcpOut(api.stripe.com:443)".to_string()]);
        let manifest = AuthorityManifest::from_token_set(&tokens).unwrap();
        assert!(manifest.allows_tcp_out("api.stripe.com", 443));
        assert_eq!(manifest.canonical_token_set(), tokens);

        let malformed = BTreeSet::from(["Net::TcpOut(api.stripe.com)".to_string()]);
        assert!(AuthorityManifest::from_token_set(&malformed).is_err());
    }

    #[test]
    fn attenuation_can_only_remove_exact_authority() {
        let parent = AuthorityManifest::from_tokens([
            "Fs::Read(/uploads/**)",
            "Net::TcpOut(api.stripe.com:443)",
            "Secret::Read(STRIPE_KEY)",
        ])
        .unwrap();
        let requested = AuthorityManifest::from_tokens([
            "Net::TcpOut(api.stripe.com:443)",
            "Secret::Read(OTHER_KEY)",
        ])
        .unwrap();

        let child = parent.restrict_to(&requested);
        assert_eq!(
            child.canonical_tokens(),
            vec!["Net::TcpOut(api.stripe.com:443)"]
        );
        assert!(child.is_subset_of(&parent));
        assert!(!requested.is_subset_of(&parent));
    }

    #[test]
    fn canonical_tokens_are_deterministic() {
        let manifest = AuthorityManifest::from_tokens([
            "Secret::Read(STRIPE_KEY)",
            "Fs::Read(/uploads/**)",
            "Net::TcpOut(api.stripe.com:443)",
        ])
        .unwrap();
        assert_eq!(
            manifest.canonical_tokens(),
            vec![
                "Fs::Read(/uploads/**)",
                "Net::TcpOut(api.stripe.com:443)",
                "Secret::Read(STRIPE_KEY)",
            ]
        );
    }

    #[test]
    fn malformed_known_grants_are_rejected() {
        assert!("Net::TcpOut(api.stripe.com)"
            .parse::<AuthorityGrant>()
            .is_err());
        assert!("Net::TcpOut(:443)".parse::<AuthorityGrant>().is_err());
        assert!("Net::TcpOut(api.stripe.com:0)"
            .parse::<AuthorityGrant>()
            .is_err());
        assert!("Fs::Read()".parse::<AuthorityGrant>().is_err());
    }

    #[test]
    fn duplicate_grants_collapse_in_manifest() {
        let manifest = AuthorityManifest::from_tokens([
            "Net::TcpOut(api.stripe.com:443)",
            "Net::TcpOut(api.stripe.com:443)",
        ])
        .unwrap();
        assert_eq!(manifest.len(), 1);
    }

    #[test]
    fn extension_grants_remain_structured() {
        let grant: AuthorityGrant = "Gpu::Use(cuda:0)".parse().unwrap();
        assert_eq!(
            grant,
            AuthorityGrant::Other {
                namespace: "Gpu".into(),
                operation: "Use".into(),
                argument: Some("cuda:0".into()),
            }
        );
        assert_eq!(grant.to_string(), "Gpu::Use(cuda:0)");
    }
}
