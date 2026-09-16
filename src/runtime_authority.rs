//! Runtime authority evaluation over Nulang's existing canonical grant strings.
//!
//! This module does not introduce a second token format. `Actor.capabilities`
//! and `CodeModule::spawn_capability_grants` already store canonical strings
//! such as `Net::TcpOut(api.example.com:443)`. We parse those strings into a
//! structured view only to validate them and enforce exact authority checks.
//!
//! Security defaults are intentionally strict:
//! - malformed grants fail closed;
//! - empty manifests grant no authority;
//! - resources are exact-match, not prefix/wildcard matched;
//! - attenuation can only select a subset of existing grants.

use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthorityToken {
    raw: String,
    namespace: String,
    action: String,
    resource: Option<String>,
}

impl AuthorityToken {
    pub fn parse(raw: impl Into<String>) -> Result<Self, AuthorityTokenError> {
        let raw = raw.into();
        if raw.is_empty() || raw.trim() != raw {
            return Err(AuthorityTokenError::Malformed(raw));
        }

        let (namespace, operation) = raw
            .split_once("::")
            .ok_or_else(|| AuthorityTokenError::Malformed(raw.clone()))?;
        if !valid_identifier(namespace) || operation.is_empty() || operation.contains("::") {
            return Err(AuthorityTokenError::Malformed(raw));
        }

        let (action, resource) = if let Some(open) = operation.find('(') {
            if !operation.ends_with(')') || open == 0 {
                return Err(AuthorityTokenError::Malformed(raw));
            }
            let action = &operation[..open];
            let resource = &operation[open + 1..operation.len() - 1];
            if !valid_identifier(action)
                || resource.is_empty()
                || resource.trim() != resource
                || resource.contains('(')
                || resource.contains(')')
            {
                return Err(AuthorityTokenError::Malformed(raw));
            }
            (action.to_string(), Some(resource.to_string()))
        } else {
            if !valid_identifier(operation) || operation.contains(')') {
                return Err(AuthorityTokenError::Malformed(raw));
            }
            (operation.to_string(), None)
        };

        Ok(Self {
            raw,
            namespace: namespace.to_string(),
            action,
            resource,
        })
    }

    pub fn net_tcp_out(destination: impl Into<String>) -> Result<Self, AuthorityTokenError> {
        let destination = destination.into();
        Self::parse(format!("Net::TcpOut({destination})"))
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn resource(&self) -> Option<&str> {
        self.resource.as_deref()
    }
}

fn valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityTokenError {
    Malformed(String),
}

impl fmt::Display for AuthorityTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(raw) => write!(f, "malformed runtime authority token '{raw}'"),
        }
    }
}

impl std::error::Error for AuthorityTokenError {}

/// Validated exact-match authority manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthoritySet {
    grants: BTreeSet<AuthorityToken>,
}

impl AuthoritySet {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Validate an actor's canonical string manifest.
    ///
    /// Any malformed entry rejects the entire manifest rather than silently
    /// dropping a restriction or accidentally broadening authority.
    pub fn from_manifest<'a, I>(manifest: I) -> Result<Self, AuthorityTokenError>
    where
        I: IntoIterator<Item = &'a String>,
    {
        let mut grants = BTreeSet::new();
        for raw in manifest {
            grants.insert(AuthorityToken::parse(raw.clone())?);
        }
        Ok(Self { grants })
    }

    pub fn from_tokens<I>(tokens: I) -> Self
    where
        I: IntoIterator<Item = AuthorityToken>,
    {
        Self {
            grants: tokens.into_iter().collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.grants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Exact authority check. No wildcard, prefix, DNS suffix, or port-range
    /// semantics are inferred from strings.
    pub fn allows(&self, required: &AuthorityToken) -> bool {
        self.grants.contains(required)
    }

    pub fn require(&self, required: &AuthorityToken) -> Result<(), CapabilityDenied> {
        if self.allows(required) {
            Ok(())
        } else {
            Err(CapabilityDenied {
                required: required.clone(),
            })
        }
    }

    /// Attenuate authority to an explicit subset.
    ///
    /// Attempting to introduce authority absent from the parent fails closed.
    pub fn attenuate<I>(&self, requested: I) -> Result<Self, CapabilityDenied>
    where
        I: IntoIterator<Item = AuthorityToken>,
    {
        let mut grants = BTreeSet::new();
        for token in requested {
            self.require(&token)?;
            grants.insert(token);
        }
        Ok(Self { grants })
    }

    pub fn iter(&self) -> impl Iterator<Item = &AuthorityToken> {
        self.grants.iter()
    }

    /// Convert back to the canonical strings already used by Actor/bytecode.
    pub fn into_manifest(self) -> BTreeSet<String> {
        self.grants.into_iter().map(|token| token.raw).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityDenied {
    pub required: AuthorityToken,
}

impl fmt::Display for CapabilityDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "capability denied: {}", self.required.as_str())
    }
}

impl std::error::Error for CapabilityDenied {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(dest: &str) -> AuthorityToken {
        AuthorityToken::net_tcp_out(dest).unwrap()
    }

    #[test]
    fn parses_existing_canonical_spawn_grant_shape() {
        let token = tcp("api.stripe.com:443");
        assert_eq!(token.namespace(), "Net");
        assert_eq!(token.action(), "TcpOut");
        assert_eq!(token.resource(), Some("api.stripe.com:443"));
        assert_eq!(token.as_str(), "Net::TcpOut(api.stripe.com:443)");
    }

    #[test]
    fn exact_resource_is_allowed_but_neighbor_is_denied() {
        let grants = AuthoritySet::from_tokens([tcp("api.example.com:443")]);
        assert!(grants.require(&tcp("api.example.com:443")).is_ok());
        assert!(grants.require(&tcp("evil.example.com:443")).is_err());
        assert!(grants.require(&tcp("api.example.com:80")).is_err());
    }

    #[test]
    fn empty_manifest_is_default_deny() {
        let grants = AuthoritySet::empty();
        let err = grants.require(&tcp("api.example.com:443")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "capability denied: Net::TcpOut(api.example.com:443)"
        );
    }

    #[test]
    fn malformed_manifest_fails_closed() {
        let manifest = BTreeSet::from([
            "Net::TcpOut(api.example.com:443)".to_string(),
            "not-a-capability".to_string(),
        ]);
        assert!(AuthoritySet::from_manifest(&manifest).is_err());
    }

    #[test]
    fn attenuation_can_only_remove_authority() {
        let parent = AuthoritySet::from_tokens([
            tcp("api.example.com:443"),
            tcp("storage.example.com:443"),
        ]);
        let child = parent
            .attenuate([tcp("api.example.com:443")])
            .expect("subset attenuation must succeed");
        assert_eq!(child.len(), 1);
        assert!(child.allows(&tcp("api.example.com:443")));
        assert!(!child.allows(&tcp("storage.example.com:443")));

        assert!(parent.attenuate([tcp("new.example.com:443")]).is_err());
    }

    #[test]
    fn round_trip_preserves_existing_manifest_format() {
        let manifest = BTreeSet::from([
            "Clock::Read".to_string(),
            "Net::TcpOut(api.example.com:443)".to_string(),
        ]);
        let validated = AuthoritySet::from_manifest(&manifest).unwrap();
        assert_eq!(validated.into_manifest(), manifest);
    }

    #[test]
    fn malformed_names_whitespace_and_nested_resource_syntax_are_rejected() {
        for malformed in [
            " Net::TcpOut(api:443)",
            "Net ::TcpOut(api:443)",
            "Net::TcpOut()",
            "Net::TcpOut(a(b))",
            "Net::Tcp Out(api:443)",
            "Net::Tcp::Out(api:443)",
            "Net-Admin::Read",
            "9Net::Read",
        ] {
            assert!(AuthorityToken::parse(malformed).is_err(), "{malformed}");
        }
    }
}
