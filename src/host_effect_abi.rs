//! Experimental compiler-owned host-effect ABI vocabulary.
//!
//! Source-level Nulang names such as `Storage.write` are language semantics.
//! Deployment/runtime systems must not independently decide what host operation
//! those names mean. This module is the first migration slice for issue #426:
//! it assigns versioned canonical host identities without changing the current
//! plain-WASM wire format yet.
//!
//! The plain-WASM `env.nulang_dispatch_args` lowering emits these canonical
//! identities for built-in host operations while preserving the existing
//! positional tagged-value argument ABI. Custom effects retain their legacy
//! source tag until they opt into an explicit versioned host contract.

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

/**
 * Deterministic compiler-owned request schema.
 *
 * `template_json` is valid JSON. Runtime arguments are represented by
 * marker objects such as `{"$arg":0}`, so conformance tooling can parse the
 * template without knowing Nulang source syntax.
 */
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HostRequestSchema {
    pub arity: u8,
    pub template_json: &'static str,
}

/// Minimum authorization provenance for host execution.
///
/// Resource-specific runtime grants may narrow this further, but the host must
/// never replace the compiler-checked effect requirement by reinterpreting a
/// source-level operation name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostAuthorityRequirement {
    CheckedEffectRow(&'static str),
}

/// Replay contract shared with RFC 0020 / Behavior Manifest v0alpha1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostReplayClass {
    Pure,
    LocalReplaySafe,
    /// Persist the observed result and replay that result rather than
    /// re-running the effect after completion.
    JournalResult,
    ExternalIdempotent,
    ExternalRequiresIdempotencyKey,
    /// No safe automatic redispatch is claimed after an ambiguous crash.
    ExternalNonreplayable,
}

impl HostReplayClass {
    /// Exact v0alpha1 Behavior Manifest spelling.
    pub const fn manifest_class(self) -> &'static str {
        match self {
            Self::Pure => "pure",
            Self::LocalReplaySafe => "local-replay-safe",
            Self::JournalResult => "journal-result",
            Self::ExternalIdempotent => "external-idempotent",
            Self::ExternalRequiresIdempotencyKey => "external-requires-idempotency-key",
            Self::ExternalNonreplayable => "external-nonreplayable",
        }
    }
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
    pub request: HostRequestSchema,
    pub response: HostResponseProjection,
    pub authority: HostAuthorityRequirement,
    pub replay: HostReplayClass,
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
/// Request schemas use valid JSON templates with {"$arg":N} markers. This
/// mirrors the current Cloud compatibility bridge exactly while moving semantic
/// ownership into the compiler before the wire format changes.
pub const HOST_OPERATIONS: &[HostOperationDescriptor] = &[
    HostOperationDescriptor {
        source_effect: "Inference",
        source_operation: "ask",
        effect_id: "nulang:inference/inference",
        operation_id: "chat",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"operation":"chat","messages":[{"role":"user","content":{"$arg":0}}]}"#,
        },
        response: HostResponseProjection::Field("content"),
        authority: HostAuthorityRequirement::CheckedEffectRow("Inference"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Storage",
        source_operation: "write",
        effect_id: "nulang:storage/string",
        operation_id: "Write",
        request: HostRequestSchema {
            arity: 2,
            template_json: r#"{"operation":"Write","key":{"$arg":0},"value":{"$arg":1}}"#,
        },
        response: HostResponseProjection::Discard,
        authority: HostAuthorityRequirement::CheckedEffectRow("Storage"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Storage",
        source_operation: "read",
        effect_id: "nulang:storage/string",
        operation_id: "Read",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"operation":"Read","key":{"$arg":0}}"#,
        },
        response: HostResponseProjection::Field("value"),
        authority: HostAuthorityRequirement::CheckedEffectRow("Storage"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Storage",
        source_operation: "delete",
        effect_id: "nulang:storage/string",
        operation_id: "Delete",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"operation":"Delete","key":{"$arg":0}}"#,
        },
        response: HostResponseProjection::Discard,
        authority: HostAuthorityRequirement::CheckedEffectRow("Storage"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Queue",
        source_operation: "push",
        effect_id: "nulang:queue/string",
        operation_id: "Send",
        request: HostRequestSchema {
            arity: 2,
            template_json: r#"{"operation":"Send","queue_name":{"$arg":0},"message":{"$arg":1}}"#,
        },
        response: HostResponseProjection::Discard,
        authority: HostAuthorityRequirement::CheckedEffectRow("Queue"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Queue",
        source_operation: "pop",
        effect_id: "nulang:queue/string",
        operation_id: "Receive",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"operation":"Receive","queue_name":{"$arg":0}}"#,
        },
        response: HostResponseProjection::Field("message"),
        authority: HostAuthorityRequirement::CheckedEffectRow("Queue"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Http",
        source_operation: "get",
        effect_id: "nulang:http/string",
        operation_id: "GET",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"url":{"$arg":0},"method":"GET","headers":{},"body":""}"#,
        },
        response: HostResponseProjection::Field("body"),
        authority: HostAuthorityRequirement::CheckedEffectRow("Http"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Timer",
        source_operation: "sleep",
        effect_id: "nulang:timer/timer",
        operation_id: "sleep",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"ms":{"$arg":0}}"#,
        },
        response: HostResponseProjection::Discard,
        authority: HostAuthorityRequirement::CheckedEffectRow("Timer"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Comms",
        source_operation: "send",
        effect_id: "nulang:comms/comms",
        operation_id: "send",
        request: HostRequestSchema {
            arity: 8,
            template_json: r#"{"operation":"send","provider":{"$arg":0},"channel":{"$arg":1},"from":{"$arg":2},"to":{"$arg":3},"body":{"$arg":4},"media_urls":{"$arg":5},"idempotency_key":{"$arg":6},"metadata":{"$arg":7}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Comms"),
        replay: HostReplayClass::ExternalRequiresIdempotencyKey,
    },
    HostOperationDescriptor {
        source_effect: "Comms",
        source_operation: "call",
        effect_id: "nulang:comms/comms",
        operation_id: "call",
        request: HostRequestSchema {
            arity: 4,
            template_json: r#"{"operation":"call","provider":{"$arg":0},"from":{"$arg":1},"to":{"$arg":2},"metadata":{"$arg":3}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Comms"),
        replay: HostReplayClass::ExternalNonreplayable,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "create",
        effect_id: "nulang:agent/agent",
        operation_id: "create",
        request: HostRequestSchema {
            arity: 3,
            template_json: r#"{"operation":"create","name":{"$arg":0},"system_prompt":{"$arg":1},"max_turns":{"$arg":2}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "send",
        effect_id: "nulang:agent/agent",
        operation_id: "send",
        request: HostRequestSchema {
            arity: 2,
            template_json: r#"{"operation":"send","session_id":{"$arg":0},"text":{"$arg":1}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "state",
        effect_id: "nulang:agent/agent",
        operation_id: "state",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"operation":"state","session_id":{"$arg":0}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "list",
        effect_id: "nulang:agent/agent",
        operation_id: "list",
        request: HostRequestSchema {
            arity: 0,
            template_json: r#"{"operation":"list"}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "delete",
        effect_id: "nulang:agent/agent",
        operation_id: "delete",
        request: HostRequestSchema {
            arity: 1,
            template_json: r#"{"operation":"delete","session_id":{"$arg":0}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "remember",
        effect_id: "nulang:agent/agent",
        operation_id: "remember",
        request: HostRequestSchema {
            arity: 3,
            template_json: r#"{"operation":"remember","session_id":{"$arg":0},"text":{"$arg":1},"metadata":{"$arg":2}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
    },
    HostOperationDescriptor {
        source_effect: "Agent",
        source_operation: "recall",
        effect_id: "nulang:agent/agent",
        operation_id: "recall",
        request: HostRequestSchema {
            arity: 3,
            template_json: r#"{"operation":"recall","session_id":{"$arg":0},"query":{"$arg":1},"top_k":{"$arg":2}}"#,
        },
        response: HostResponseProjection::Passthrough,
        authority: HostAuthorityRequirement::CheckedEffectRow("Agent"),
        replay: HostReplayClass::JournalResult,
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
        operation.source_effect == source_effect && operation.source_operation == source_operation
    })
}

/// Resolve a canonical host identity without consulting source spellings.
pub fn lookup_host_operation_by_identity(
    effect_id: &str,
    operation_id: &str,
) -> Option<&'static HostOperationDescriptor> {
    HOST_OPERATIONS.iter().find(|operation| {
        operation.effect_id == effect_id && operation.operation_id == operation_id
    })
}

/// Build the compiler-owned external ABI descriptor consumed by conformance
/// tooling and, eventually, Cloud admission/runtime adapters.
///
/// The descriptor intentionally omits source operation spellings such as
/// `Storage.write`. Consumers receive only canonical host identity plus the
/// compiler-produced request/response/authority/replay contract.
pub fn host_effect_abi_descriptor() -> Result<serde_json::Value, serde_json::Error> {
    let mut operations = Vec::with_capacity(HOST_OPERATIONS.len());

    for operation in HOST_OPERATIONS {
        let template: serde_json::Value = serde_json::from_str(operation.request.template_json)?;
        let response = match operation.response {
            HostResponseProjection::Passthrough => serde_json::json!({
                "kind": "passthrough"
            }),
            HostResponseProjection::Field(field) => serde_json::json!({
                "kind": "field",
                "field": field
            }),
            HostResponseProjection::Discard => serde_json::json!({
                "kind": "discard"
            }),
        };
        let authority = match operation.authority {
            HostAuthorityRequirement::CheckedEffectRow(effect) => serde_json::json!({
                "kind": "checked-effect-row",
                "effect": effect
            }),
        };

        operations.push(serde_json::json!({
            "canonical_id": operation.canonical_id(),
            "effect_id": operation.effect_id,
            "operation_id": operation.operation_id,
            "request": {
                "arity": operation.request.arity,
                "template": template
            },
            "response": response,
            "authority": authority,
            "replay_class": operation.replay.manifest_class()
        }));
    }

    Ok(serde_json::json!({
        "schema": HOST_EFFECT_ABI_SCHEMA,
        "operations": operations
    }))
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
    fn request_contracts_have_valid_json_and_declared_arg_bounds() {
        fn visit(value: &serde_json::Value, arity: u8) {
            match value {
                serde_json::Value::Object(fields)
                    if fields.len() == 1 && fields.contains_key("$arg") =>
                {
                    let index = fields["$arg"]
                        .as_u64()
                        .expect("$arg marker must contain a non-negative integer");
                    assert!(
                        index < arity as u64,
                        "request schema references arg {index} but arity is {arity}"
                    );
                }
                serde_json::Value::Object(fields) => {
                    for value in fields.values() {
                        visit(value, arity);
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        visit(value, arity);
                    }
                }
                _ => {}
            }
        }

        for operation in HOST_OPERATIONS {
            let template: serde_json::Value =
                serde_json::from_str(operation.request.template_json).unwrap();
            visit(&template, operation.request.arity);
            assert_eq!(
                operation.authority,
                HostAuthorityRequirement::CheckedEffectRow(operation.source_effect)
            );
        }
    }

    #[test]
    fn canonical_identity_lookup_does_not_need_source_names() {
        let operation =
            lookup_host_operation_by_identity("nulang:queue/string", "Receive").unwrap();
        assert_eq!(
            (operation.source_effect, operation.source_operation),
            ("Queue", "pop")
        );
    }

    #[test]
    fn storage_write_has_compiler_owned_runtime_identity() {
        let operation = lookup_host_operation("Storage", "write").unwrap();
        assert_eq!(operation.effect_id, "nulang:storage/string");
        assert_eq!(operation.operation_id, "Write");
        assert_eq!(operation.request.arity, 2);
        assert_eq!(
            operation.request.template_json,
            r#"{"operation":"Write","key":{"$arg":0},"value":{"$arg":1}}"#
        );
        assert_eq!(operation.response, HostResponseProjection::Discard);
        assert_eq!(operation.replay, HostReplayClass::JournalResult);
        assert_eq!(operation.replay.manifest_class(), "journal-result");
        assert_eq!(
            operation.canonical_id(),
            "nulang.host-effects/v0alpha1:nulang:storage/string#Write"
        );
    }

    #[test]
    fn replay_classes_match_behavior_manifest_v0alpha1_vocabulary() {
        assert_eq!(HostReplayClass::Pure.manifest_class(), "pure");
        assert_eq!(
            HostReplayClass::LocalReplaySafe.manifest_class(),
            "local-replay-safe"
        );
        assert_eq!(
            HostReplayClass::JournalResult.manifest_class(),
            "journal-result"
        );
        assert_eq!(
            HostReplayClass::ExternalIdempotent.manifest_class(),
            "external-idempotent"
        );
        assert_eq!(
            HostReplayClass::ExternalRequiresIdempotencyKey.manifest_class(),
            "external-requires-idempotency-key"
        );
        assert_eq!(
            HostReplayClass::ExternalNonreplayable.manifest_class(),
            "external-nonreplayable"
        );
    }

    #[test]
    fn checked_in_descriptor_fixture_matches_compiler_contract() {
        let actual: serde_json::Value =
            serde_json::from_str(include_str!("../spec/host-effects/v0alpha1.json")).unwrap();
        let expected = host_effect_abi_descriptor().unwrap();
        assert_eq!(actual, expected);

        let fixture = include_str!("../spec/host-effects/v0alpha1.json");
        assert!(!fixture.contains("Storage.write"));
        assert!(!fixture.contains("Queue.push"));
        assert!(!fixture.contains("Inference.ask"));
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
