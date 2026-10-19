//! Experimental read-only reflection over the compiler's HIR actor declarations.
//!
//! Produces deterministic, namespace-qualified metadata for tooling and future
//! typed derivation. No source-level reflection syntax, runtime introspection,
//! schema-format changes, or compile-time evaluator is introduced here.
//! HIR can substitute placeholder types and empty effects for unannotated
//! behaviors. Their names, parameter names and explicit capabilities are exposed,
//! but not falsely advertised as fully verified type/effect contracts.

use std::collections::BTreeSet;

use crate::ast::StateModel;
use crate::hir;
use crate::types::{Capability, Type};

/// One immutable snapshot of an actor's compiler-visible public structure.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorReflection {
    pub qualified_name: String,
    pub schema_version: u32,
    pub persistent: bool,
    pub fields: Vec<StateFieldReflection>,
    pub behaviors: Vec<BehaviorReflection>,
    pub events: Vec<EventReflection>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateFieldReflection {
    pub name: String,
    pub model: StateModel,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BehaviorReflection {
    pub name: String,
    /// Names and declaration order only: HIR may use placeholder parameter types.
    pub parameter_names: Vec<String>,
    pub capability: Capability,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventReflection {
    pub name: String,
    /// Event payload order is semantically meaningful and preserved.
    pub params: Vec<(String, Type)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReflectionError {
    DuplicateActor(String),
    DuplicateMember {
        actor: String,
        kind: &'static str,
        name: String,
    },
}

fn ensure_unique<'a>(
    names: impl IntoIterator<Item = &'a str>,
    actor: &str,
    kind: &'static str,
) -> Result<(), ReflectionError> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(ReflectionError::DuplicateMember {
                actor: actor.to_string(),
                kind,
                name: name.to_string(),
            });
        }
    }
    Ok(())
}

/// Reflect named actors in a previously validated HIR module.
///
/// Actors, fields, behaviors and events are sorted by fully qualified name or
/// member name for reproducibility. Parameter ordering is never changed.
/// Duplicate qualified actor names and duplicate members fail closed.
/// This API is not an admission or compatibility proof: consumers must still
/// validate type/effect semantics and use the canonical semantic identity
/// machinery for deployable artifacts.
pub fn reflect_actors(module: &hir::Module) -> Result<Vec<ActorReflection>, ReflectionError> {
    let mut prefix = Vec::new();
    if !module.name.is_empty() {
        prefix.push(module.name.clone());
    }
    let mut actors = Vec::new();
    collect_actors(&module.decls, &mut prefix, &mut actors)?;
    actors.sort_by(|a, b| a.qualified_name.cmp(&b.qualified_name));
    for pair in actors.windows(2) {
        if pair[0].qualified_name == pair[1].qualified_name {
            return Err(ReflectionError::DuplicateActor(pair[0].qualified_name.clone()));
        }
    }
    Ok(actors)
}

fn collect_actors(
    decls: &[hir::Decl],
    prefix: &mut Vec<String>,
    out: &mut Vec<ActorReflection>,
) -> Result<(), ReflectionError> {
    for decl in decls {
        match decl {
            hir::Decl::Module { name, decls, .. } => {
                prefix.push(name.clone());
                let result = collect_actors(decls, prefix, out);
                prefix.pop();
                result?;
            }
            hir::Decl::Actor(actor) => {
                let qualified_name = if prefix.is_empty() {
                    actor.name.clone()
                } else {
                    format!("{}::{}", prefix.join("::"), actor.name)
                };

                ensure_unique(
                    actor.state_fields.iter().map(|(name, ..)| name.as_str()),
                    &qualified_name,
                    "state field",
                )?;
                ensure_unique(
                    actor.behaviors.iter().map(|behavior| behavior.name.as_str()),
                    &qualified_name,
                    "behavior",
                )?;
                ensure_unique(
                    actor.events.iter().map(|event| event.name.as_str()),
                    &qualified_name,
                    "event",
                )?;

                let mut fields: Vec<_> = actor
                    .state_fields
                    .iter()
                    .map(|(name, model, ty, _default)| StateFieldReflection {
                        name: name.clone(),
                        model: *model,
                        ty: ty.clone(),
                    })
                    .collect();
                fields.sort_by(|a, b| a.name.cmp(&b.name));

                let mut behaviors: Vec<_> = actor
                    .behaviors
                    .iter()
                    .map(|behavior| BehaviorReflection {
                        name: behavior.name.clone(),
                        parameter_names: behavior
                            .params
                            .iter()
                            .map(|(name, _)| name.clone())
                            .collect(),
                        capability: behavior.cap,
                    })
                    .collect();
                behaviors.sort_by(|a, b| a.name.cmp(&b.name));

                let mut events: Vec<_> = actor
                    .events
                    .iter()
                    .map(|event| EventReflection {
                        name: event.name.clone(),
                        params: event.params.clone(),
                    })
                    .collect();
                events.sort_by(|a, b| a.name.cmp(&b.name));

                out.push(ActorReflection {
                    qualified_name,
                    schema_version: actor.version,
                    persistent: actor.persistent,
                    fields,
                    behaviors,
                    events,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EventDecl, Literal};
    use crate::types::{EffectRow, PrimitiveType, Span};

    fn int_ty() -> Type {
        Type::Primitive(PrimitiveType::Int)
    }

    fn sample_actor(name: &str) -> hir::ActorDef {
        let span = Span::default();
        hir::ActorDef {
            name: name.to_string(),
            type_params: Vec::new(),
            persistent: true,
            state_fields: vec![
                (
                    "z".into(),
                    StateModel::Local,
                    int_ty(),
                    hir::Operand::Literal(Literal::Int(0), int_ty()),
                ),
                (
                    "a".into(),
                    StateModel::Durable,
                    int_ty(),
                    hir::Operand::Literal(Literal::Int(0), int_ty()),
                ),
            ],
            behaviors: vec![
                hir::BehaviorDef {
                    name: "write".into(),
                    params: vec![("first".into(), int_ty()), ("second".into(), int_ty())],
                    ret: Type::unit(),
                    effect: EffectRow::empty(),
                    cap: Capability::Ref,
                    body: hir::Body::default(),
                    compensate: None,
                    parallel_branches: None,
                    span,
                },
                hir::BehaviorDef {
                    name: "read".into(),
                    params: Vec::new(),
                    ret: Type::unit(),
                    effect: EffectRow::empty(),
                    cap: Capability::Box,
                    body: hir::Body::default(),
                    compensate: None,
                    parallel_branches: None,
                    span,
                },
            ],
            init: Vec::new(),
            events: vec![
                EventDecl {
                    name: "Written".into(),
                    params: vec![("value".into(), int_ty())],
                    span,
                },
                EventDecl {
                    name: "Created".into(),
                    params: Vec::new(),
                    span,
                },
            ],
            apply_handlers: Vec::new(),
            version: 2,
            migrations: Vec::new(),
            is_organization: false,
            is_workflow: false,
            is_agent: false,
            virtual_: false,
            tools: Vec::new(),
            semantic_memory_dimensions: None,
            procedural_memory_namespace: None,
            fallback_config: String::new(),
            retry_config: String::new(),
            span,
        }
    }

    #[test]
    fn reflection_qualifies_nested_actors_and_sorts_members_without_reordering_params() {
        let module = hir::Module {
            name: "demo".into(),
            decls: vec![
                hir::Decl::Module {
                    name: "Inventory".into(),
                    exports: Vec::new(),
                    decls: vec![hir::Decl::Actor(sample_actor("Counter"))],
                    span: Span::default(),
                },
                hir::Decl::Module {
                    name: "Billing".into(),
                    exports: Vec::new(),
                    decls: vec![hir::Decl::Actor(sample_actor("Counter"))],
                    span: Span::default(),
                },
            ],
        };
        let actors = reflect_actors(&module).unwrap();
        assert_eq!(actors.len(), 2);
        assert_eq!(actors[0].qualified_name, "demo::Billing::Counter");
        assert_eq!(actors[1].qualified_name, "demo::Inventory::Counter");
        assert_eq!(actors[0].schema_version, 2);
        assert!(actors[0].persistent);
        assert_eq!(
            actors[0]
                .fields
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
        assert_eq!(actors[0].fields[0].model, StateModel::Durable);
        assert_eq!(
            actors[0]
                .events
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            ["Created", "Written"]
        );
        assert_eq!(
            actors[0]
                .behaviors
                .iter()
                .map(|x| x.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "write"]
        );
        assert_eq!(
            actors[0].behaviors[1]
                .parameter_names
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(actors[0].behaviors[0].capability, Capability::Box);
    }

    #[test]
    fn reflection_is_independent_of_declaration_order() {
        let first = sample_actor("Counter");
        let mut permuted = first.clone();
        permuted.state_fields.reverse();
        permuted.events.reverse();
        permuted.behaviors.reverse();
        let left = hir::Module {
            name: "demo".into(),
            decls: vec![hir::Decl::Actor(first)],
        };
        let right = hir::Module {
            name: "demo".into(),
            decls: vec![hir::Decl::Actor(permuted)],
        };
        assert_eq!(reflect_actors(&left).unwrap(), reflect_actors(&right).unwrap());
    }

    #[test]
    fn reflection_fails_closed_on_ambiguous_actor_or_member_names() {
        let actor = sample_actor("Counter");
        let duplicated = hir::Module {
            name: "demo".into(),
            decls: vec![
                hir::Decl::Actor(actor.clone()),
                hir::Decl::Actor(actor.clone()),
            ],
        };
        assert_eq!(
            reflect_actors(&duplicated),
            Err(ReflectionError::DuplicateActor("demo::Counter".into()))
        );

        let mut duplicate_field = actor.clone();
        duplicate_field
            .state_fields
            .push(duplicate_field.state_fields[0].clone());
        assert_eq!(
            reflect_actors(&hir::Module {
                name: "demo".into(),
                decls: vec![hir::Decl::Actor(duplicate_field)],
            }),
            Err(ReflectionError::DuplicateMember {
                actor: "demo::Counter".into(),
                kind: "state field",
                name: "z".into(),
            })
        );

        let mut duplicate_behavior = actor.clone();
        duplicate_behavior
            .behaviors
            .push(duplicate_behavior.behaviors[0].clone());
        assert_eq!(
            reflect_actors(&hir::Module {
                name: "demo".into(),
                decls: vec![hir::Decl::Actor(duplicate_behavior)],
            }),
            Err(ReflectionError::DuplicateMember {
                actor: "demo::Counter".into(),
                kind: "behavior",
                name: "write".into(),
            })
        );

        let mut duplicate_event = actor;
        duplicate_event.events.push(duplicate_event.events[0].clone());
        assert_eq!(
            reflect_actors(&hir::Module {
                name: "demo".into(),
                decls: vec![hir::Decl::Actor(duplicate_event)],
            }),
            Err(ReflectionError::DuplicateMember {
                actor: "demo::Counter".into(),
                kind: "event",
                name: "Written".into(),
            })
        );
    }
}
