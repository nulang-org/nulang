//! Experimental compiler-owned host-effect ABI vocabulary.
//!
//! Source-level Nulang names such as `Storage.write` are language semantics.
//! Deployment/runtime systems must not independently decide what host operation
//! those names mean. This module is the first migration slice for issue #426:
//! it assigns versioned canonical host identities without changing the current
//! plain-WASM wire format yet.
//!
//! The current `env.nulang_dispatch_args` lowering still emits the legacy
//! source name. A later slice will lower directly to these canonical identities
//! and deterministic request schemas.

/// Experimental host-effect ABI schema identity.
///
/// This is intentionally independent from the Nulang language version and from
/// the Behavior Manifest schema version.
pub const HOST_EFFECT_ABI_SCHEMA: &str = "nulang.host-effects/v0alpha1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostResponseProjection {
    /// Return the host response unchanged.
    Passthrough,
    /// Extract one named field from an object response.
    Field(&'static str),
    /// The source operation has no result value.
    Discard,
}

/// Compiler-owned identity for one source operation that crosses the host ABI.
///
/// `effect_id` and `operation_id` are runtime contract identifiers, not
/// source spellings. `source_effect` / `source_operation` exist only so the
/// compiler can lower checked source semantics into that contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostOperationDescriptor {
    pub source_effect: &'static str,
    pub source_operation: &'static str,
    pub effect_id: &'static str,
    pub operation_id: &'static str,
    pub response: HostResponseProjection,
}

impl HostOperationDescriptor {
    /// Stable versioned identity suitable for manifests, fixtures and
    /// conformance tooling. The exact v0alpha1 string format is experimental.
    pub fn canonical_id(&self) -> String {
        format!(
            "{}:{}#{}",
            HOST_EFFECT_ABI_SCHEMA, self.effect_id, self.operation_id
        )
    }
}

/// Operations currently supported by the legacy Nulang Cloud source-name
/// bridge. Keeping this table compiler-owned prevents Cloud from becoming an
/// independent source-semantics registry while the wire migration proceeds.
///
/// Request schemas/envelope construction remain follow-up work; the table only
/// establishes identity and response projection in this first slice.
pub const HOST_OPERATIONS: &[HostOperationDescriptor] = &[
    HostOperationDescriptor {
        source_effect: "Inference",
        source_operation: "ask",
        effect_id: "nulang:inference/inference",
        operation_id: "chat",
        response: HostResponseProjection::Field("content"),
    },
    HostOperationDescriptor {
        source_effect: "Storage",
        source_operation: "write",
        effect_id: "nulang:storage/string",
        operation_id: "Write",
        response: HostResponseProjection::Discard,
    },
    HostOperationDescriptor {
        source_effect: "Storage",
        source_operation: "read",
        effect_id: "nulang:storage/string",
        operation_id: "Read",
        response: HostResponseProjection::Field("value"),
    },
    HostOperationDescriptor {
        source_effect: "Storage",
        source_operation: "delete",
        effect_id: "nulang:storage/string",
        operation_id: "Delete",
        response: HostResponseProjection::Discard,
    },
    HostOperationDescriptor {
        source_effect: "Queue",
        source_operation: "push",
        effect_id: "nulang:queue/string",
        operation_id: "Send",
        response: HostResponseProjection::Discard,
    },
    HostOperationDescriptor {
        source_effect: "Queue",
        source_operation: "pop",
        effect_id: "nulang:queue/string",
        operation_id: "Receive",
        response: HostResponseProjection::Field("message"),
    },
    HostOperationDescriptor {
        source_effect: "Http",
        source_operation: "get",
        effect_id: "nulang:http/string",
        operation_id: "GET",
        response: HostResponseProjection::Field("body"),
    },
    HostOperationDescriptor {
        source_effect: "Timer",
        source_operation: "sleep",
        effect_id: "nulang:timer/timer",
        operation_id: "sleep",
        response: HostResponseProjection::Discard,
    },
    HostOperationDescriptor {
        source_effect: "Comms",
        source_operation: "send",
        effect_id: "nulang:comms/comms",
        operation_id: "send",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Comms",
        source_operation: "call",
        effect_id: "nulang:comms/comms",
        operation_id: "call",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "create",
        effect_id: "nulang:agent/agent",
        operation_id: "create",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "send",
        effect_id: "nulang:agent/agent",
        operation_id: "send",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "state",
        effect_id: "nulang:agent/agent",
        operation_id: "state",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "list",
        effect_id: "nulang:agent/agent",
        operation_id: "list",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "delete",
        effect_id: "nulang:agent/agent",
        operation_id: "delete",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "remember",
        effect_id: "nulang:agent/agent",
        operation_id: "remember",
        response: HostResponseProjection::Passthrough,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "recall",
        effect_id: "nulang:agent/agent",
        operation_id: "recall",
        response: HostResponseProjection::Passthrough,
    },
];

/// Resolve one checked source effect operation to the compiler-owned host ABI.
///
/// Unknown operations return `None`: custom effects are an explicit extension
/// path and must not be silently classified as built-in host authority.
pub fn lookup_host_operation(
    source_effect: &str,
    source_operation: &str,
) -> Option<&'static HostOperationDescriptor> {
    HOST_OPERATIONS.iter().find(|operation| {
        operation.source_effect == source_effect
            && operation.source_operation == source_operation
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn canonical_operation_ids_are_unique() {
        let mut ids = HashSet::new();
        for operation in HOST_OPERATIONS {
            assert!(
                ids.insert(operation.canonical_id()),
                "duplicate canonical host operation id: {}",
                operation.canonical_id()
            );
        }
    }

    #[test]
    fn source_operation_keys_are_unique() {
        let mut keys = HashSet::new();
        for operation in HOST_OPERATIONS {
            assert!(
                keys.insert((operation.source_effect, operation.source_operation)),
                "duplicate source host-operation mapping: {}.{}",
                operation.source_effect,
                operation.source_operation
            );
        }
    }

    #[test]
    fn storage_write_has_compiler_owned_runtime_identity() {
        let operation = lookup_host_operation("Storage", "write").unwrap();
        assert_eq!(operation.effect_id, "nulang:storage/string");
        assert_eq!(operation.operation_id, "Write");
        assert_eq!(operation.response, HostResponseProjection::Discard);
        assert_eq!(
            operation.canonical_id(),
            "nulang.host-effects/v0alpha1:nulang:storage/string#Write"
        );
    }

    #[test]
    fn custom_effect_is_not_silently_promoted_to_builtin_authority() {
        assert!(lookup_host_operation("CustomerBilling", "charge").is_none());
    }

    #[test]
    fn migrated_cloud_legacy_surface_is_complete() {
        let expected = [
            ("Inference", "ask"),
            ("Storage", "write"),
            ("Storage", "read"),
            ("Storage", "delete"),
            ("Queue", "push"),
            ("Queue", "pop"),
            ("Http", "get"),
            ("Timer", "sleep"),
            ("Comms", "send"),
            ("Comms", "call"),
            ("Agent", "create"),
            ("Agent", "send"),
            ("Agent", "state"),
            ("Agent", "list"),
            ("Agent", "delete"),
            ("Agent", "remember"),
            ("Agent", "recall"),
        ];

        assert_eq!(HOST_OPERATIONS.len(), expected.len());
        for (effect, operation) in expected {
            assert!(
                lookup_host_operation(effect, operation).is_some(),
                "missing compiler-owned host operation for {effect}.{operation}"
            );
        }
    }
}
