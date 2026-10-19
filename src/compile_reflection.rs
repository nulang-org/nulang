
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{EventDecl, Literal};
    use crate::types::{PrimitiveType, Span};

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
                ("z".into(), StateModel::Local, int_ty(),
                    hir::Operand::Literal(Literal::Int(0), int_ty())),
                ("a".into(), StateModel::Durable, int_ty(),
                    hir::Operand::Literal(Literal::Int(0), int_ty())),
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
                EventDecl { name: "Written".into(), params: vec![("value".into(), int_ty())], span },
                EventDecl { name: "Created".into(), params: Vec::new(), span },
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
        assert_eq!(actors[0].fields.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["a", "z"]);
        assert_eq!(actors[0].fields[0].model, StateModel::Durable);
        assert_eq!(actors[0].events.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["Created", "Written"]);
        assert_eq!(actors[0].behaviors.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["read", "write"]);
        assert_eq!(actors[0].behaviors[1].params.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        assert_eq!(actors[0].behaviors[0].capability, Capability::Box);
    }

    #[test]
    fn reflection_is_independent_of_declaration_order() {
        let first = sample_actor("Counter");
        let mut permuted = first.clone();
        permuted.state_fields.reverse();
        permuted.events.reverse();
        permuted.behaviors.reverse();
        let left = hir::Module { name: "demo".into(), decls: vec![hir::Decl::Actor(first)] };
        let right = hir::Module { name: "demo".into(), decls: vec![hir::Decl::Actor(permuted)] };
        assert_eq!(reflect_actors(&left).unwrap(), reflect_actors(&right).unwrap());
    }

    #[test]
    fn reflection_fails_closed_on_ambiguous_actor_or_member_names() {
        let actor = sample_actor("Counter");
        let duplicated = hir::Module {
            name: "demo".into(),
            decls: vec![hir::Decl::Actor(actor.clone()), hir::Decl::Actor(actor.clone())],
        };
        assert_eq!(reflect_actors(&duplicated), Err(ReflectionError::DuplicateActor("demo::Counter".into())));

        let mut duplicate_field = actor.clone();
        duplicate_field.state_fields.push(duplicate_field.state_fields[0].clone());
        assert_eq!(
            reflect_actors(&hir::Module { name: "demo".into(), decls: vec![hir::Decl::Actor(duplicate_field)] }),
            Err(ReflectionError::DuplicateMember { actor: "demo::Counter".into(), kind: "state field", name: "z".into() })
        );

        let mut duplicate_behavior = actor.clone();
        duplicate_behavior.behaviors.push(duplicate_behavior.behaviors[0].clone());
        assert_eq!(
            reflect_actors(&hir::Module { name: "demo".into(), decls: vec![hir::Decl::Actor(duplicate_behavior)] }),
            Err(ReflectionError::DuplicateMember { actor: "demo::Counter".into(), kind: "behavior", name: "write".into() })
        );

        let mut duplicate_event = actor;
        duplicate_event.events.push(duplicate_event.events[0].clone());
        assert_eq!(
            reflect_actors(&hir::Module { name: "demo".into(), decls: vec![hir::Decl::Actor(duplicate_event)] }),
            Err(ReflectionError::DuplicateMember { actor: "demo::Counter".into(), kind: "event", name: "Written".into() })
        );
    }
}
