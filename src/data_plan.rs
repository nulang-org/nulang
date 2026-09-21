//! Backend-independent logical query planning for durable entity state.
//!
//! This module deliberately plans access paths, not SQL. The compiler/runtime
//! owns the logical entity schema and index declarations; physical backends are
//! free to realize a chosen index using different storage engines.

use std::collections::BTreeSet;

use crate::hir::ActorDef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitySchema {
    pub name: String,
    pub fields: Vec<String>,
    pub indexes: Vec<crate::ast::IndexDecl>,
}

impl From<&ActorDef> for EntitySchema {
    fn from(actor: &ActorDef) -> Self {
        Self {
            name: actor.name.clone(),
            fields: actor
                .state_fields
                .iter()
                .map(|(name, _, _, _)| name.clone())
                .collect(),
            indexes: actor.indexes.clone(),
        }
    }
}

/// Collect the compact durable-data schemas needed by the runtime.
///
/// HIR behavior bodies, tools, workflow metadata, and other compiler-only
/// details are intentionally excluded so this type can become a versioned
/// artifact boundary later without serializing HIR wholesale.
pub fn collect_entity_schemas(module: &crate::hir::Module) -> Vec<EntitySchema> {
    fn collect(decls: &[crate::hir::Decl], out: &mut Vec<EntitySchema>) {
        for decl in decls {
            match decl {
                crate::hir::Decl::Actor(actor) if actor.persistent => {
                    out.push(EntitySchema::from(actor));
                }
                crate::hir::Decl::Module { decls, .. } => collect(decls, out),
                _ => {}
            }
        }
    }

    let mut schemas = Vec::new();
    collect(&module.decls, &mut schemas);
    schemas
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryPredicateKind {
    Eq,
    Range,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPredicate {
    pub field: String,
    pub kind: QueryPredicateKind,
}

impl QueryPredicate {
    pub fn eq(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            kind: QueryPredicateKind::Eq,
        }
    }

    pub fn range(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            kind: QueryPredicateKind::Range,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalAccessPath {
    EntityScan,
    Index {
        name: String,
        fields: Vec<String>,
        unique: bool,
        matched_prefix: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalQueryPlan {
    pub entity: String,
    pub access: LogicalAccessPath,
    /// Predicates not satisfied by the selected access path. A physical
    /// executor applies these after retrieving candidate rows/entities.
    pub residual_predicates: Vec<QueryPredicate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSuggestion {
    pub name: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryPlanDiagnostic {
    pub code: &'static str,
    pub message: String,
    pub suggested_index: Option<IndexSuggestion>,
}

/// Produce deterministic, backend-independent query diagnostics.
///
/// Diagnostics describe logical work only; they never invent latency or
/// cardinality estimates. Runtime feedback can enrich these later.
pub fn diagnose_query_plan(
    plan: &LogicalQueryPlan,
    predicates: &[QueryPredicate],
) -> Vec<QueryPlanDiagnostic> {
    let mut diagnostics = Vec::new();

    if plan.access == LogicalAccessPath::EntityScan {
        let mut seen = BTreeSet::new();
        let equality_fields: Vec<String> = predicates
            .iter()
            .filter(|predicate| predicate.kind == QueryPredicateKind::Eq)
            .filter_map(|predicate| {
                if seen.insert(predicate.field.as_str()) {
                    Some(predicate.field.clone())
                } else {
                    None
                }
            })
            .collect();

        let suggested_index = if equality_fields.is_empty() {
            None
        } else {
            Some(IndexSuggestion {
                name: format!("by_{}", equality_fields.join("_")),
                fields: equality_fields,
            })
        };

        diagnostics.push(QueryPlanDiagnostic {
            code: "NQ001",
            message: format!(
                "query on '{}' requires an entity scan; no declared index matches its equality prefix",
                plan.entity
            ),
            suggested_index,
        });
    }

    if !plan.residual_predicates.is_empty() {
        diagnostics.push(QueryPlanDiagnostic {
            code: "NQ002",
            message: format!(
                "query on '{}' has {} residual predicate(s) evaluated after candidate lookup",
                plan.entity,
                plan.residual_predicates.len()
            ),
            suggested_index: None,
        });
    }

    diagnostics
}

/// Select the strongest declared access path for an entity query shape.
///
/// Phase 2 intentionally uses only deterministic structural rules:
/// - equality predicates can satisfy an index prefix;
/// - a fully matched unique index wins over non-unique alternatives;
/// - otherwise the longest matched prefix wins;
/// - ties preserve declaration order.
///
/// Cardinality feedback and backend cost models belong to a later optimizer
/// layer and must not change the source-level meaning of an index declaration.
pub fn plan_entity_query(
    actor: &ActorDef,
    predicates: &[QueryPredicate],
) -> Result<LogicalQueryPlan, String> {
    plan_schema_query(&EntitySchema::from(actor), predicates)
}

pub fn plan_schema_query(
    schema: &EntitySchema,
    predicates: &[QueryPredicate],
) -> Result<LogicalQueryPlan, String> {
    let fields: BTreeSet<&str> = schema.fields.iter().map(String::as_str).collect();

    for predicate in predicates {
        if !fields.contains(predicate.field.as_str()) {
            return Err(format!(
                "query on '{}' references unknown state field '{}'",
                schema.name, predicate.field
            ));
        }
    }

    let equality_fields: BTreeSet<&str> = predicates
        .iter()
        .filter(|predicate| predicate.kind == QueryPredicateKind::Eq)
        .map(|predicate| predicate.field.as_str())
        .collect();

    let mut selected: Option<(&crate::ast::IndexDecl, usize, bool)> = None;
    for index in &schema.indexes {
        let matched_prefix = index
            .fields
            .iter()
            .take_while(|field| equality_fields.contains(field.as_str()))
            .count();
        if matched_prefix == 0 {
            continue;
        }

        let unique_full_match = index.unique && matched_prefix == index.fields.len();
        let replace = match selected {
            None => true,
            Some((_, selected_prefix, selected_unique_full)) => {
                (unique_full_match, matched_prefix) > (selected_unique_full, selected_prefix)
            }
        };
        if replace {
            selected = Some((index, matched_prefix, unique_full_match));
        }
    }

    let (access, covered_fields): (LogicalAccessPath, BTreeSet<&str>) =
        if let Some((index, matched_prefix, _)) = selected {
            let covered = index
                .fields
                .iter()
                .take(matched_prefix)
                .map(String::as_str)
                .collect();
            (
                LogicalAccessPath::Index {
                    name: index.name.clone(),
                    fields: index.fields.clone(),
                    unique: index.unique,
                    matched_prefix,
                },
                covered,
            )
        } else {
            (LogicalAccessPath::EntityScan, BTreeSet::new())
        };

    let residual_predicates = predicates
        .iter()
        .filter(|predicate| {
            predicate.kind != QueryPredicateKind::Eq
                || !covered_fields.contains(predicate.field.as_str())
        })
        .cloned()
        .collect();

    Ok(LogicalQueryPlan {
        entity: schema.name.clone(),
        access,
        residual_predicates,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{IndexDecl, StateModel};
    use crate::hir::Operand;
    use crate::types::{Span, Type};

    fn actor() -> ActorDef {
        ActorDef {
            name: "Customer".to_string(),
            type_params: vec![],
            persistent: true,
            state_fields: vec![
                (
                    "email".to_string(),
                    StateModel::EventSourced,
                    Type::string(),
                    Operand::Unit,
                ),
                (
                    "company".to_string(),
                    StateModel::EventSourced,
                    Type::string(),
                    Operand::Unit,
                ),
                (
                    "status".to_string(),
                    StateModel::EventSourced,
                    Type::string(),
                    Operand::Unit,
                ),
            ],
            indexes: vec![
                IndexDecl {
                    name: "email".to_string(),
                    fields: vec!["email".to_string()],
                    unique: true,
                    span: Span::default(),
                },
                IndexDecl {
                    name: "by_company_status".to_string(),
                    fields: vec!["company".to_string(), "status".to_string()],
                    unique: false,
                    span: Span::default(),
                },
            ],
            behaviors: vec![],
            init: vec![],
            events: vec![],
            apply_handlers: vec![],
            version: 1,
            migrations: vec![],
            is_workflow: false,
            is_organization: false,
            is_agent: false,
            virtual_: false,
            tools: vec![],
            semantic_memory_dimensions: None,
            procedural_memory_namespace: None,
            fallback_config: String::new(),
            retry_config: String::new(),
            span: Span::default(),
        }
    }

    #[test]
    fn fully_matched_unique_index_wins() {
        let plan = plan_entity_query(
            &actor(),
            &[QueryPredicate::eq("email"), QueryPredicate::eq("company")],
        )
        .unwrap();

        assert!(matches!(
            plan.access,
            LogicalAccessPath::Index {
                ref name,
                unique: true,
                matched_prefix: 1,
                ..
            } if name == "email"
        ));
        assert_eq!(
            plan.residual_predicates,
            vec![QueryPredicate::eq("company")]
        );
    }

    #[test]
    fn composite_index_supports_left_prefix_lookup() {
        let plan = plan_entity_query(&actor(), &[QueryPredicate::eq("company")]).unwrap();

        assert!(matches!(
            plan.access,
            LogicalAccessPath::Index {
                ref name,
                matched_prefix: 1,
                ..
            } if name == "by_company_status"
        ));
        assert!(plan.residual_predicates.is_empty());
    }

    #[test]
    fn non_prefix_filter_falls_back_to_entity_scan() {
        let plan = plan_entity_query(&actor(), &[QueryPredicate::eq("status")]).unwrap();

        assert_eq!(plan.access, LogicalAccessPath::EntityScan);
        assert_eq!(plan.residual_predicates, vec![QueryPredicate::eq("status")]);
    }

    #[test]
    fn range_filter_is_residual_in_phase_two_planner() {
        let plan = plan_entity_query(
            &actor(),
            &[
                QueryPredicate::eq("company"),
                QueryPredicate::range("status"),
            ],
        )
        .unwrap();

        assert!(matches!(
            plan.access,
            LogicalAccessPath::Index {
                ref name,
                matched_prefix: 1,
                ..
            } if name == "by_company_status"
        ));
        assert_eq!(
            plan.residual_predicates,
            vec![QueryPredicate::range("status")]
        );
    }

    #[test]
    fn scan_diagnostic_suggests_equality_index_without_cost_guessing() {
        let schema = EntitySchema {
            name: "Customer".to_string(),
            fields: vec!["company".to_string(), "status".to_string()],
            indexes: vec![],
        };
        let predicates = vec![QueryPredicate::eq("company"), QueryPredicate::eq("status")];
        let plan = plan_schema_query(&schema, &predicates).unwrap();
        let diagnostics = diagnose_query_plan(&plan, &predicates);

        assert_eq!(diagnostics[0].code, "NQ001");
        assert_eq!(
            diagnostics[0].suggested_index,
            Some(IndexSuggestion {
                name: "by_company_status".to_string(),
                fields: vec!["company".to_string(), "status".to_string()],
            })
        );
    }

    #[test]
    fn indexed_query_reports_only_residual_work() {
        let predicates = vec![
            QueryPredicate::eq("company"),
            QueryPredicate::range("status"),
        ];
        let plan = plan_entity_query(&actor(), &predicates).unwrap();
        let diagnostics = diagnose_query_plan(&plan, &predicates);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "NQ002");
        assert!(diagnostics[0].suggested_index.is_none());
    }

    #[test]
    fn unknown_query_field_fails_before_backend_selection() {
        let err = plan_entity_query(&actor(), &[QueryPredicate::eq("missing")]).unwrap_err();
        assert!(err.contains("unknown state field 'missing'"));
    }
}
