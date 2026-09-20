//! Canonical actor-behavior identity helpers.
//!
//! Actor dispatch must never use "first suffix match" semantics. A statically
//! known actor resolves only an exact `Actor.behavior` identity. When the actor
//! identity is genuinely dynamic, a short behavior name may resolve only when
//! it is globally unique; ambiguity is an error rather than an arbitrary choice.
//!
//! This module is intentionally independent of HIR, MIR, bytecode, and the
//! runtime so every layer can share the same fail-closed rule.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BehaviorIdentityError {
    Unknown {
        actor: Option<String>,
        behavior: String,
    },
    Ambiguous {
        behavior: String,
        candidates: Vec<String>,
    },
}

impl fmt::Display for BehaviorIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BehaviorIdentityError::Unknown {
                actor: Some(actor),
                behavior,
            } => write!(f, "actor '{actor}' has no behavior '{behavior}'"),
            BehaviorIdentityError::Unknown {
                actor: None,
                behavior,
            } => write!(f, "no actor declares behavior '{behavior}'"),
            BehaviorIdentityError::Ambiguous {
                behavior,
                candidates,
            } => write!(
                f,
                "behavior '{behavior}' is ambiguous across actor schemas: {}",
                candidates.join(", ")
            ),
        }
    }
}

impl std::error::Error for BehaviorIdentityError {}

/// Resolve a source-level behavior name to its backend-local behavior-table
/// index without ever selecting an arbitrary suffix match.
///
/// When `actor_name` is known, only the exact `Actor.behavior` identity is
/// accepted. When it is unknown, compatibility fallback is permitted only if
/// exactly one actor schema declares the requested short name.
pub fn resolve_behavior_index(
    actor_name: Option<&str>,
    behavior: &str,
    behavior_names: &[String],
) -> Result<usize, BehaviorIdentityError> {
    if let Some(actor_name) = actor_name.filter(|name| !name.is_empty()) {
        let expected = format!("{actor_name}.{behavior}");
        return behavior_names
            .iter()
            .position(|name| name == &expected)
            .ok_or_else(|| BehaviorIdentityError::Unknown {
                actor: Some(actor_name.to_string()),
                behavior: behavior.to_string(),
            });
    }

    resolve_unique_short_behavior(behavior, behavior_names)
}

/// Transitional resolver for compiler layers that still carry only a lexical
/// receiver-name hint rather than a proven actor schema identity.
///
/// The hint is allowed to win only when it happens to name an exact actor
/// schema (`Second.hit`). Otherwise we deliberately discard the untrusted hint
/// and fall back to globally-unique short-name resolution. This preserves old
/// dynamic behavior where it is unambiguous while eliminating the dangerous
/// "first suffix match" rule.
///
/// Once typed HIR carries a proven actor schema, callers should use
/// [`resolve_behavior_index`] directly with `Some(actor_schema)` instead.
pub fn resolve_behavior_index_from_hint(
    actor_name_hint: &str,
    behavior: &str,
    behavior_names: &[String],
) -> Result<usize, BehaviorIdentityError> {
    if !actor_name_hint.is_empty() {
        let exact = format!("{actor_name_hint}.{behavior}");
        if let Some(idx) = behavior_names.iter().position(|name| name == &exact) {
            return Ok(idx);
        }
    }

    resolve_unique_short_behavior(behavior, behavior_names)
}

fn resolve_unique_short_behavior(
    behavior: &str,
    behavior_names: &[String],
) -> Result<usize, BehaviorIdentityError> {
    let mut matches = behavior_names
        .iter()
        .enumerate()
        .filter_map(|(idx, full_name)| {
            (short_behavior_name(full_name) == behavior).then_some((idx, full_name))
        });

    let Some((idx, first_name)) = matches.next() else {
        return Err(BehaviorIdentityError::Unknown {
            actor: None,
            behavior: behavior.to_string(),
        });
    };

    let mut candidates = vec![first_name.clone()];
    for (_, name) in matches {
        candidates.push(name.clone());
    }

    if candidates.len() == 1 {
        Ok(idx)
    } else {
        candidates.sort();
        Err(BehaviorIdentityError::Ambiguous {
            behavior: behavior.to_string(),
            candidates,
        })
    }
}

/// Verify that a backend-local behavior-table index belongs to the target
/// actor schema. This is the runtime defense-in-depth counterpart to nominal
/// compiler resolution: stale, malformed, migrated, or remote artifacts must
/// not execute another actor schema's code merely because the numeric index is
/// in range.
pub fn behavior_index_belongs_to_actor(
    actor_name: &str,
    behavior_idx: usize,
    actor_behavior_indices: &[usize],
    behavior_names: &[String],
) -> bool {
    if !actor_behavior_indices.contains(&behavior_idx) {
        return false;
    }

    behavior_names.get(behavior_idx).is_some_and(|name| {
        name.strip_prefix(actor_name)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|behavior| !behavior.is_empty())
    })
}

fn short_behavior_name(full_name: &str) -> &str {
    full_name
        .rsplit_once('.')
        .map_or(full_name, |(_, behavior)| behavior)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec![
            "First.hit".to_string(),
            "First.only_first".to_string(),
            "Second.hit".to_string(),
            "Second.only_second".to_string(),
        ]
    }

    #[test]
    fn nominal_lookup_selects_only_the_receiver_schema() {
        let names = names();
        assert_eq!(resolve_behavior_index(Some("First"), "hit", &names), Ok(0));
        assert_eq!(resolve_behavior_index(Some("Second"), "hit", &names), Ok(2));
    }

    #[test]
    fn nominal_lookup_never_falls_back_to_another_actor() {
        let names = names();
        let error = resolve_behavior_index(Some("Second"), "only_first", &names).unwrap_err();
        assert_eq!(
            error,
            BehaviorIdentityError::Unknown {
                actor: Some("Second".to_string()),
                behavior: "only_first".to_string(),
            }
        );
    }

    #[test]
    fn dynamic_lookup_allows_only_a_globally_unique_short_name() {
        let names = names();
        assert_eq!(resolve_behavior_index(None, "only_second", &names), Ok(3));

        let error = resolve_behavior_index(None, "hit", &names).unwrap_err();
        assert_eq!(
            error,
            BehaviorIdentityError::Ambiguous {
                behavior: "hit".to_string(),
                candidates: vec!["First.hit".to_string(), "Second.hit".to_string()],
            }
        );
    }

    #[test]
    fn lexical_hint_falls_back_only_when_short_name_is_unique() {
        let names = names();
        assert_eq!(
            resolve_behavior_index_from_hint("target", "only_second", &names),
            Ok(3)
        );

        let error = resolve_behavior_index_from_hint("target", "hit", &names).unwrap_err();
        assert_eq!(
            error,
            BehaviorIdentityError::Ambiguous {
                behavior: "hit".to_string(),
                candidates: vec!["First.hit".to_string(), "Second.hit".to_string()],
            }
        );
    }

    #[test]
    fn lexical_hint_uses_exact_actor_name_when_available() {
        let names = names();
        assert_eq!(
            resolve_behavior_index_from_hint("Second", "hit", &names),
            Ok(2)
        );
    }

    #[test]
    fn unknown_dynamic_behavior_fails_closed() {
        let error = resolve_behavior_index(None, "missing", &names()).unwrap_err();
        assert_eq!(
            error,
            BehaviorIdentityError::Unknown {
                actor: None,
                behavior: "missing".to_string(),
            }
        );
    }

    #[test]
    fn runtime_ownership_requires_index_and_nominal_owner() {
        let names = names();
        let first_indices = [0, 1];
        let second_indices = [2, 3];

        assert!(behavior_index_belongs_to_actor(
            "First",
            0,
            &first_indices,
            &names
        ));
        assert!(behavior_index_belongs_to_actor(
            "Second",
            2,
            &second_indices,
            &names
        ));
        assert!(!behavior_index_belongs_to_actor(
            "Second",
            0,
            &second_indices,
            &names
        ));
        assert!(!behavior_index_belongs_to_actor(
            "First",
            2,
            &first_indices,
            &names
        ));
    }

    #[test]
    fn runtime_ownership_handles_qualified_actor_names() {
        let names = vec![
            "billing.Counter.hit".to_string(),
            "other.Counter.hit".to_string(),
        ];

        assert!(behavior_index_belongs_to_actor(
            "billing.Counter",
            0,
            &[0],
            &names
        ));
        assert!(!behavior_index_belongs_to_actor(
            "billing.Counter",
            1,
            &[0],
            &names
        ));
    }
}
