//! Explicit protocol compatibility for rolling actor upgrades.
//!
//! Durable/distributed actors may coexist across code deployments. Compatibility
//! therefore cannot be inferred from process version or from a protocol version
//! number alone. Each activation advertises one exact revision it emits and the
//! exact revisions it can receive. Every revision carries a schema fingerprint
//! so accidental drift under the same version number fails closed.
//!
//! No SemVer behavior is implicit. A new activation that wants to communicate
//! with an older peer must explicitly accept the older revision and, when the
//! old peer cannot consume the new revision, continue emitting the old revision
//! until the rollout reaches the protocol cutover phase.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolFingerprint(pub [u8; 32]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolRevision {
    pub version: ProtocolVersion,
    pub fingerprint: ProtocolFingerprint,
}

impl ProtocolRevision {
    pub const fn new(version: u32, fingerprint: [u8; 32]) -> Self {
        Self {
            version: ProtocolVersion(version),
            fingerprint: ProtocolFingerprint(fingerprint),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolSupport {
    pub protocol: String,
    pub emits: ProtocolRevision,
    pub accepts: BTreeSet<ProtocolRevision>,
}

impl ProtocolSupport {
    pub fn new(
        protocol: impl Into<String>,
        emits: ProtocolRevision,
        accepts: impl IntoIterator<Item = ProtocolRevision>,
    ) -> Result<Self, ProtocolCompatibilityError> {
        let protocol = protocol.into();
        if protocol.is_empty() {
            return Err(ProtocolCompatibilityError::EmptyProtocolName);
        }
        let accepts = accepts.into_iter().collect::<BTreeSet<_>>();
        if !accepts.contains(&emits) {
            return Err(ProtocolCompatibilityError::DoesNotAcceptOwnEmittedRevision {
                protocol,
                emits,
            });
        }
        Ok(Self { protocol, emits, accepts })
    }

    pub fn accepts_revision(&self, revision: ProtocolRevision) -> bool {
        self.accepts.contains(&revision)
    }

    /// Change the emitted revision after rollout coordination proves peers can
    /// receive it. No receive capability is added implicitly.
    pub fn with_emitted_revision(
        mut self,
        emits: ProtocolRevision,
    ) -> Result<Self, ProtocolCompatibilityError> {
        if !self.accepts.contains(&emits) {
            return Err(ProtocolCompatibilityError::DoesNotAcceptOwnEmittedRevision {
                protocol: self.protocol,
                emits,
            });
        }
        self.emits = emits;
        Ok(self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationProtocols {
    protocols: BTreeMap<String, ProtocolSupport>,
}

impl ActivationProtocols {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        support: ProtocolSupport,
    ) -> Result<(), ProtocolCompatibilityError> {
        let name = support.protocol.clone();
        if self.protocols.contains_key(&name) {
            return Err(ProtocolCompatibilityError::DuplicateProtocol { protocol: name });
        }
        self.protocols.insert(name, support);
        Ok(())
    }

    pub fn get(&self, protocol: &str) -> Option<&ProtocolSupport> {
        self.protocols.get(protocol)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ProtocolSupport)> {
        self.protocols
            .iter()
            .map(|(name, support)| (name.as_str(), support))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageCompatibility {
    Compatible,
    SenderMissingProtocol { protocol: String },
    ReceiverMissingProtocol { protocol: String },
    ReceiverRejectsRevision {
        protocol: String,
        emitted: ProtocolRevision,
    },
}

/// One-way compatibility: only the receiver must accept the exact revision
/// currently emitted by the sender.
pub fn check_message_compatibility(
    sender: &ActivationProtocols,
    receiver: &ActivationProtocols,
    protocol: &str,
) -> MessageCompatibility {
    let Some(sender_support) = sender.get(protocol) else {
        return MessageCompatibility::SenderMissingProtocol {
            protocol: protocol.to_string(),
        };
    };
    let Some(receiver_support) = receiver.get(protocol) else {
        return MessageCompatibility::ReceiverMissingProtocol {
            protocol: protocol.to_string(),
        };
    };
    if receiver_support.accepts_revision(sender_support.emits) {
        MessageCompatibility::Compatible
    } else {
        MessageCompatibility::ReceiverRejectsRevision {
            protocol: protocol.to_string(),
            emitted: sender_support.emits,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoexistenceCheck {
    pub old_to_new: MessageCompatibility,
    pub new_to_old: MessageCompatibility,
}

impl CoexistenceCheck {
    pub fn compatible(&self) -> bool {
        self.old_to_new == MessageCompatibility::Compatible
            && self.new_to_old == MessageCompatibility::Compatible
    }
}

/// Conservative coexistence gate for actors that may initiate messages in both
/// directions during a mixed-version deployment.
pub fn check_bidirectional_coexistence(
    old: &ActivationProtocols,
    new: &ActivationProtocols,
    protocol: &str,
) -> CoexistenceCheck {
    CoexistenceCheck {
        old_to_new: check_message_compatibility(old, new, protocol),
        new_to_old: check_message_compatibility(new, old, protocol),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolCompatibilityError {
    EmptyProtocolName,
    DoesNotAcceptOwnEmittedRevision {
        protocol: String,
        emits: ProtocolRevision,
    },
    DuplicateProtocol { protocol: String },
}

impl fmt::Display for ProtocolCompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyProtocolName => write!(f, "actor protocol name cannot be empty"),
            Self::DoesNotAcceptOwnEmittedRevision { protocol, emits } => write!(
                f,
                "protocol '{protocol}' emits revision {} but does not accept that exact revision",
                emits.version.0
            ),
            Self::DuplicateProtocol { protocol } => {
                write!(f, "duplicate actor protocol '{protocol}'")
            }
        }
    }
}

impl std::error::Error for ProtocolCompatibilityError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(version: u32, fingerprint_byte: u8) -> ProtocolRevision {
        ProtocolRevision::new(version, [fingerprint_byte; 32])
    }

    fn activation(support: ProtocolSupport) -> ActivationProtocols {
        let mut protocols = ActivationProtocols::new();
        protocols.insert(support).unwrap();
        protocols
    }

    #[test]
    fn exact_revision_is_compatible() {
        let v1 = r(1, 1);
        let a = activation(ProtocolSupport::new("Counter", v1, [v1]).unwrap());
        let b = activation(ProtocolSupport::new("Counter", v1, [v1]).unwrap());
        assert_eq!(
            check_message_compatibility(&a, &b, "Counter"),
            MessageCompatibility::Compatible
        );
    }

    #[test]
    fn same_version_number_with_different_schema_fingerprint_fails_closed() {
        let old_v1 = r(1, 1);
        let drifted_v1 = r(1, 2);
        let sender = activation(ProtocolSupport::new("Counter", drifted_v1, [drifted_v1]).unwrap());
        let receiver = activation(ProtocolSupport::new("Counter", old_v1, [old_v1]).unwrap());
        assert_eq!(
            check_message_compatibility(&sender, &receiver, "Counter"),
            MessageCompatibility::ReceiverRejectsRevision {
                protocol: "Counter".into(),
                emitted: drifted_v1,
            }
        );
    }

    #[test]
    fn phase_one_rollout_can_run_new_code_while_still_emitting_old_protocol() {
        let v1 = r(1, 1);
        let v2 = r(2, 2);
        let old = activation(ProtocolSupport::new("Counter", v1, [v1]).unwrap());
        let new = activation(ProtocolSupport::new("Counter", v1, [v1, v2]).unwrap());
        assert!(check_bidirectional_coexistence(&old, &new, "Counter").compatible());
    }

    #[test]
    fn switching_new_code_to_v2_before_old_peers_leave_is_rejected() {
        let v1 = r(1, 1);
        let v2 = r(2, 2);
        let old = activation(ProtocolSupport::new("Counter", v1, [v1]).unwrap());
        let new = activation(ProtocolSupport::new("Counter", v2, [v1, v2]).unwrap());
        let check = check_bidirectional_coexistence(&old, &new, "Counter");
        assert_eq!(check.old_to_new, MessageCompatibility::Compatible);
        assert_eq!(
            check.new_to_old,
            MessageCompatibility::ReceiverRejectsRevision {
                protocol: "Counter".into(),
                emitted: v2,
            }
        );
        assert!(!check.compatible());
    }

    #[test]
    fn phase_two_cutover_succeeds_after_all_peers_accept_v2() {
        let v1 = r(1, 1);
        let v2 = r(2, 2);
        let a = activation(ProtocolSupport::new("Counter", v2, [v1, v2]).unwrap());
        let b = activation(ProtocolSupport::new("Counter", v2, [v1, v2]).unwrap());
        assert!(check_bidirectional_coexistence(&a, &b, "Counter").compatible());
    }

    #[test]
    fn no_implicit_version_compatibility_exists() {
        let v10 = r(10, 10);
        let v11 = r(11, 11);
        let sender = activation(ProtocolSupport::new("Counter", v11, [v11]).unwrap());
        let receiver = activation(ProtocolSupport::new("Counter", v10, [v10]).unwrap());
        assert!(matches!(
            check_message_compatibility(&sender, &receiver, "Counter"),
            MessageCompatibility::ReceiverRejectsRevision { .. }
        ));
    }

    #[test]
    fn support_must_accept_its_own_emitted_revision() {
        let v1 = r(1, 1);
        let v2 = r(2, 2);
        assert!(matches!(
            ProtocolSupport::new("Counter", v2, [v1]),
            Err(ProtocolCompatibilityError::DoesNotAcceptOwnEmittedRevision { .. })
        ));
    }

    #[test]
    fn duplicate_protocol_registration_does_not_replace_existing_support() {
        let v1 = r(1, 1);
        let support = ProtocolSupport::new("Counter", v1, [v1]).unwrap();
        let mut protocols = ActivationProtocols::new();
        protocols.insert(support.clone()).unwrap();
        assert_eq!(
            protocols.insert(support),
            Err(ProtocolCompatibilityError::DuplicateProtocol {
                protocol: "Counter".into(),
            })
        );
        assert_eq!(protocols.get("Counter").unwrap().emits, v1);
    }
}
