//! Runtime-owned opaque secret handles.
//!
//! This broker intentionally stores **no secret bytes**. A Nulang
//! `Secret[T]` runtime value is currently represented by a checked integer
//! handle whose broker entry contains only the owning actor id and the logical
//! secret name. Provider-specific operations may later resolve the name to
//! actual secret material entirely inside the host boundary.
//!
//! Handle ids are opaque, not cryptographic capabilities. Every lookup is
//! scoped to the owning actor, so guessing another actor's handle does not
//! grant access. Compiler-level `Secret[T]` typing prevents ordinary source
//! code from manufacturing a secret value from an integer.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use crate::value_layout::INT48_MAX;

pub const MAX_SECRET_NAME_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SecretHandle {
    owner_actor: u64,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretBrokerError {
    EmptyName,
    NameTooLong { actual: usize },
    ContainsNul,
    HandleSpaceExhausted,
}

impl fmt::Display for SecretBrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyName => f.write_str("secret name must not be empty"),
            Self::NameTooLong { actual } => write!(
                f,
                "secret name exceeds {MAX_SECRET_NAME_BYTES} bytes: {actual}"
            ),
            Self::ContainsNul => f.write_str("secret name must not contain NUL"),
            Self::HandleSpaceExhausted => {
                f.write_str("secret handle space exhausted")
            }
        }
    }
}

impl Error for SecretBrokerError {}

#[derive(Debug)]
pub struct SecretBroker {
    next_handle: u64,
    handles: HashMap<u64, SecretHandle>,
}

impl Default for SecretBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretBroker {
    pub fn new() -> Self {
        Self {
            next_handle: 1,
            handles: HashMap::new(),
        }
    }

    /// Issue an actor-scoped opaque handle for a logical secret name.
    ///
    /// This does not read from an environment variable, Vault, KMS, or any
    /// other provider, so no secret material enters the VM as a side effect of
    /// obtaining the handle.
    pub fn issue(
        &mut self,
        owner_actor: u64,
        name: &str,
    ) -> Result<u64, SecretBrokerError> {
        if name.is_empty() {
            return Err(SecretBrokerError::EmptyName);
        }
        if name.len() > MAX_SECRET_NAME_BYTES {
            return Err(SecretBrokerError::NameTooLong { actual: name.len() });
        }
        if name.as_bytes().contains(&0) {
            return Err(SecretBrokerError::ContainsNul);
        }
        if self.next_handle > INT48_MAX as u64 {
            return Err(SecretBrokerError::HandleSpaceExhausted);
        }

        let handle = self.next_handle;
        self.next_handle += 1;
        self.handles.insert(
            handle,
            SecretHandle {
                owner_actor,
                name: name.to_owned(),
            },
        );
        Ok(handle)
    }

    /// Resolve a handle to its logical name only when the owner matches.
    pub fn resolve_name(&self, owner_actor: u64, handle: u64) -> Option<&str> {
        self.handles
            .get(&handle)
            .filter(|entry| entry.owner_actor == owner_actor)
            .map(|entry| entry.name.as_str())
    }

    /// Revoke one handle. Cross-actor revocation fails closed.
    pub fn revoke(&mut self, owner_actor: u64, handle: u64) -> bool {
        let owned = self
            .handles
            .get(&handle)
            .is_some_and(|entry| entry.owner_actor == owner_actor);
        if owned {
            self.handles.remove(&handle);
        }
        owned
    }

    /// Drop every handle owned by an actor.
    pub fn revoke_owner(&mut self, owner_actor: u64) -> usize {
        let before = self.handles.len();
        self.handles
            .retain(|_, entry| entry.owner_actor != owner_actor);
        before - self.handles.len()
    }

    pub fn len(&self) -> usize {
        self.handles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuing_handle_stores_only_name_and_owner_metadata() {
        let mut broker = SecretBroker::new();
        let handle = broker.issue(7, "payments/stripe").unwrap();

        assert_eq!(broker.resolve_name(7, handle), Some("payments/stripe"));
        assert_eq!(broker.len(), 1);
    }

    #[test]
    fn another_actor_cannot_resolve_or_revoke_handle() {
        let mut broker = SecretBroker::new();
        let handle = broker.issue(7, "TOKEN").unwrap();

        assert_eq!(broker.resolve_name(8, handle), None);
        assert!(!broker.revoke(8, handle));
        assert_eq!(broker.resolve_name(7, handle), Some("TOKEN"));
    }

    #[test]
    fn revoke_owner_invalidates_all_actor_handles() {
        let mut broker = SecretBroker::new();
        let a = broker.issue(7, "A").unwrap();
        let b = broker.issue(7, "B").unwrap();
        let c = broker.issue(8, "C").unwrap();

        assert_eq!(broker.revoke_owner(7), 2);
        assert_eq!(broker.resolve_name(7, a), None);
        assert_eq!(broker.resolve_name(7, b), None);
        assert_eq!(broker.resolve_name(8, c), Some("C"));
    }

    #[test]
    fn rejects_invalid_names() {
        let mut broker = SecretBroker::new();
        assert_eq!(broker.issue(1, ""), Err(SecretBrokerError::EmptyName));
        assert_eq!(
            broker.issue(1, "bad\0name"),
            Err(SecretBrokerError::ContainsNul)
        );
    }
}
