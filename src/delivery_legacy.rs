//! Compatibility adapter for the runtime's existing string/integer delivery
//! failure notifications.
//!
//! `runtime::distributed::notify_delivery_failed` currently maps ad-hoc reason
//! strings to integer codes. New delivery paths should emit structured
//! `DeadLetterReason` values directly, but this adapter lets migration happen
//! incrementally without changing the meaning of old actor-visible codes.

use crate::delivery::DeadLetterReason;

/// Existing actor-visible delivery failure classification plus the new
/// structured reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDeliveryFailure {
    pub code: i64,
    pub reason: DeadLetterReason,
}

/// Convert a legacy runtime reason string into both its historical integer code
/// and the structured dead-letter vocabulary.
///
/// Historical codes are preserved exactly for the strings handled by today's
/// `delivery_failure_code` implementation:
///
/// - 0: unresolvable
/// - 1: target node left cluster
/// - 2: sender string payload cannot be resolved
/// - 3: receiver string interning failed
/// - 4: target actor missing
/// - 5: unknown/other
/// - 6: sender object ref cannot be resolved
/// - 7: receiver object interning failed
pub fn classify_legacy_delivery_failure(reason: &str) -> LegacyDeliveryFailure {
    let (code, structured) = match reason {
        "unresolvable" => (0, DeadLetterReason::Unresolvable),
        "target node left cluster" => (1, DeadLetterReason::NodeUnavailable),
        "string payload unresolvable" => (2, DeadLetterReason::PayloadEncoding),
        "string intern failed on receiver" => (3, DeadLetterReason::PayloadEncoding),
        "target actor not found" => (4, DeadLetterReason::TargetActorMissing),
        "object ref unresolvable" => (6, DeadLetterReason::PayloadEncoding),
        "object intern failed on receiver" => (7, DeadLetterReason::PayloadEncoding),
        // Newer retry/error strings already present in distributed.rs were not
        // assigned dedicated historical codes and therefore remain code 5.
        "target actor not found on retry" => (5, DeadLetterReason::TargetActorMissing),
        "spawn request rejected" => (5, DeadLetterReason::SpawnRejected),
        "spawn target node not in cluster" => (5, DeadLetterReason::NodeUnavailable),
        "string intern failed on retry" | "object intern failed on retry" => {
            (5, DeadLetterReason::PayloadEncoding)
        }
        "behavior content hash still mismatched after fetch" => {
            (5, DeadLetterReason::BehaviorUnavailable)
        }
        other if other.starts_with("bytecode fetch failed:") => {
            (5, DeadLetterReason::BehaviorUnavailable)
        }
        other if other.starts_with("capability denied:") => {
            (5, DeadLetterReason::CapabilityDenied)
        }
        other => (5, DeadLetterReason::Other(other.to_string())),
    };

    LegacyDeliveryFailure {
        code,
        reason: structured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_all_historical_codes() {
        let cases = [
            ("unresolvable", 0),
            ("target node left cluster", 1),
            ("string payload unresolvable", 2),
            ("string intern failed on receiver", 3),
            ("target actor not found", 4),
            ("object ref unresolvable", 6),
            ("object intern failed on receiver", 7),
            ("something old does not recognize", 5),
        ];

        for (reason, code) in cases {
            assert_eq!(classify_legacy_delivery_failure(reason).code, code);
        }
    }

    #[test]
    fn groups_encoding_failures_without_losing_legacy_code() {
        assert_eq!(
            classify_legacy_delivery_failure("string payload unresolvable"),
            LegacyDeliveryFailure {
                code: 2,
                reason: DeadLetterReason::PayloadEncoding,
            }
        );
        assert_eq!(
            classify_legacy_delivery_failure("object ref unresolvable"),
            LegacyDeliveryFailure {
                code: 6,
                reason: DeadLetterReason::PayloadEncoding,
            }
        );
    }

    #[test]
    fn classifies_newer_failure_strings_structurally() {
        assert_eq!(
            classify_legacy_delivery_failure("spawn request rejected").reason,
            DeadLetterReason::SpawnRejected
        );
        assert_eq!(
            classify_legacy_delivery_failure(
                "bytecode fetch failed: sender does not have the requested behavior"
            )
            .reason,
            DeadLetterReason::BehaviorUnavailable
        );
        assert_eq!(
            classify_legacy_delivery_failure("capability denied: Net::TcpOut(api.example:443)")
                .reason,
            DeadLetterReason::CapabilityDenied
        );
    }

    #[test]
    fn unknown_reason_is_retained_for_diagnostics() {
        assert_eq!(
            classify_legacy_delivery_failure("custom failure"),
            LegacyDeliveryFailure {
                code: 5,
                reason: DeadLetterReason::Other("custom failure".into()),
            }
        );
    }
}
