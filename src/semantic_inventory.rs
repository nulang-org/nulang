//! Compiler-derived semantic inventory for deployable artifacts.
//!
//! This module is the compiler-owned bridge into RFC 0020 Behavior Manifests.
//! It operates only on an already type/effect-checked AST and the exact
//! EffectChecker instance that established module-function fixpoint rows.
//! Package and Cloud layers consume this output; they must not re-infer source
//! semantics independently.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ast::{self, AstModule, Behavior, Decl, WorkflowItem};
use crate::effect_checker::{
    effect_resource_category, flatten_decls, EffectChecker, EffectContext,
};
use crate::protocol::{ProtocolMember, ProtocolSchema};
use crate::types::{Effect, EffectRow, NuResult};

pub const COMPILER_SEMANTIC_INVENTORY_SCHEMA: &str =
    "nulang.compiler-semantics/v0alpha1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompilerSemanticInventory {
    pub schema: String,
    pub effects: Vec<SemanticEffect>,
    pub actors: Vec<SemanticActor>,
    pub required_authority: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticEffect {
    pub subject_kind: String,
    pub subject: String,
    pub effects: Vec<String>,
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticActor {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_id: Option<String>,
    pub protocol_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_error: Option<String>,
    pub behaviors: Vec<SemanticBehavior>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticBehavior {
    pub name: String,
    pub effects: Vec<String>,
    pub open_effects: bool,
}

impl CompilerSemanticInventory {
    /// Extract a deterministic compiler-owned inventory from an already
    /// checked module.
    ///
    /// The result is conservative at module scope: every declared function,
    /// actor behavior, and workflow step is inventoried, whether or not it is
    /// reachable from the package entry point. That over-approximation is
    /// deliberate for security/admission use.
    pub fn from_checked_module(
        module: &AstModule,
        effect_checker: &mut EffectChecker,
    ) -> NuResult<Self> {
        let mut effects = Vec::new();
        let mut actors = Vec::new();

        for decl in flatten_decls(&module.decls) {
            match decl {
                Decl::Function { name, .. } => {
                    if let Some(row) = effect_checker.function_row(name) {
                        effects.push(effect_entry("function", name, row));
                    }
                }
                Decl::Actor {
                    name, behaviors, ..
                } => {
                    let (actor, mut actor_effects) =
                        actor_inventory(name, behaviors, effect_checker)?;
                    actors.push(actor);
                    effects.append(&mut actor_effects);
                }
                Decl::StateMachine {
                    name,
                    states,
                    events,
                    entry_hooks,
                    exit_hooks,
                    span,
                } => {
                    let desugared = ast::desugar_state_machine(
                        name,
                        states,
                        events,
                        entry_hooks,
                        exit_hooks,
                        *span,
                    );
                    if let Decl::Actor {
                        behaviors, ..
                    } = desugared
                    {
                        let (actor, mut actor_effects) =
                            actor_inventory(name, &behaviors, effect_checker)?;
                        actors.push(actor);
                        effects.append(&mut actor_effects);
                    }
                }
                Decl::Workflow {
                    name,
                    items,
                    compensate,
                    ..
                } => {
                    let ctx = EffectContext::empty();
                    for item in items {
                        let steps = match item {
                            WorkflowItem::Step(step) => std::slice::from_ref(step),
                            WorkflowItem::Parallel(steps) => steps.as_slice(),
                        };
                        for step in steps {
                            let row = effect_checker.infer_effects(&ctx, &step.body)?;
                            effects.push(effect_entry(
                                "workflow_step",
                                &format!("{name}.{}", step.name),
                                &row,
                            ));
                            if let Some(compensate) = &step.compensate {
                                let row =
                                    effect_checker.infer_effects(&ctx, compensate)?;
                                effects.push(effect_entry(
                                    "workflow_compensation",
                                    &format!("{name}.{}", step.name),
                                    &row,
                                ));
                            }
                        }
                    }
                    if let Some(compensate) = compensate {
                        let row = effect_checker.infer_effects(&ctx, compensate)?;
                        effects.push(effect_entry(
                            "workflow_compensation",
                            name,
                            &row,
                        ));
                    }
                }
                _ => {}
            }
        }

        effects.sort_by(|a, b| {
            (&a.subject_kind, &a.subject).cmp(&(&b.subject_kind, &b.subject))
        });
        actors.sort_by(|a, b| a.name.cmp(&b.name));

        let required_authority = authority_categories(&effects);

        Ok(Self {
            schema: COMPILER_SEMANTIC_INVENTORY_SCHEMA.to_string(),
            effects,
            actors,
            required_authority,
        })
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

fn actor_inventory(
    actor_name: &str,
    behaviors: &[Behavior],
    effect_checker: &mut EffectChecker,
) -> NuResult<(SemanticActor, Vec<SemanticEffect>)> {
    let ctx = EffectContext::empty();
    let mut members = Vec::new();
    let mut behavior_inventory = Vec::new();
    let mut effect_inventory = Vec::new();
    let mut protocol_error = None;

    for behavior in behaviors {
        let row = match &behavior.effect {
            Some(declared) => declared.clone(),
            None => effect_checker.infer_effects(&ctx, &behavior.body)?,
        };
        let (effect_names, open_effects) = row_parts(&row);
        behavior_inventory.push(SemanticBehavior {
            name: behavior.name.clone(),
            effects: effect_names.clone(),
            open_effects,
        });
        effect_inventory.push(SemanticEffect {
            subject_kind: "actor_behavior".to_string(),
            subject: format!("{actor_name}.{}", behavior.name),
            effects: effect_names,
            open: open_effects,
        });

        if protocol_error.is_some() {
            continue;
        }
        let Some(response) = behavior.ret_type.clone() else {
            protocol_error = Some(format!(
                "behavior '{}' has no explicit return type",
                behavior.name
            ));
            continue;
        };
        let mut params = Vec::with_capacity(behavior.params.len());
        for param in &behavior.params {
            let Some(ty) = param.ty.clone() else {
                protocol_error = Some(format!(
                    "behavior '{}' parameter '{}' has no explicit type",
                    behavior.name, param.name
                ));
                break;
            };
            params.push(ty);
        }
        if protocol_error.is_some() {
            continue;
        }

        match ProtocolMember::behavior(
            behavior.name.clone(),
            params,
            response,
            row,
            behavior.cap,
        ) {
            Ok(member) => members.push(member),
            Err(error) => protocol_error = Some(error.to_string()),
        }
    }

    behavior_inventory.sort_by(|a, b| a.name.cmp(&b.name));

    let (protocol_id, protocol_status) = if let Some(error) = &protocol_error {
        (
            None,
            if error.contains("no explicit") {
                "incomplete-signature".to_string()
            } else if error.contains("open effect row") {
                "open-effects".to_string()
            } else {
                "unavailable".to_string()
            },
        )
    } else {
        match ProtocolSchema::new(actor_name, members) {
            Ok(schema) => (Some(format!("blake3:{}", schema.id())), "complete".to_string()),
            Err(error) => {
                protocol_error = Some(error.to_string());
                (None, "unavailable".to_string())
            }
        }
    };

    Ok((
        SemanticActor {
            name: actor_name.to_string(),
            protocol_id,
            protocol_status,
            protocol_error,
            behaviors: behavior_inventory,
        },
        effect_inventory,
    ))
}

fn effect_entry(kind: &str, subject: &str, row: &EffectRow) -> SemanticEffect {
    let (effects, open) = row_parts(row);
    SemanticEffect {
        subject_kind: kind.to_string(),
        subject: subject.to_string(),
        effects,
        open,
    }
}

fn row_parts(row: &EffectRow) -> (Vec<String>, bool) {
    let mut effects = BTreeSet::new();
    for effect in row.effects() {
        effects.insert(effect.to_string());
    }
    (
        effects.into_iter().collect(),
        matches!(row, EffectRow::Open(_, _)),
    )
}

fn authority_categories(effects: &[SemanticEffect]) -> Vec<String> {
    let mut categories = BTreeSet::new();
    for entry in effects {
        for effect_name in &entry.effects {
            let effect = effect_from_canonical_name(effect_name);
            if let Some(effect) = effect.as_ref() {
                if let Some(category) = effect_resource_category(effect) {
                    categories.insert(category.to_string());
                }
            }
        }
    }
    categories.into_iter().collect()
}

/// Reverse the stable built-in Display names used by the semantic inventory.
/// User-defined effects do not map to a host resource category here.
fn effect_from_canonical_name(name: &str) -> Option<Effect> {
    Some(match name {
        "IO" => Effect::IO,
        "Net" => Effect::Net,
        "String" => Effect::String,
        "FS" => Effect::FS,
        "Rand" => Effect::Rand,
        "Time" => Effect::Time,
        "Spawn" => Effect::Spawn,
        "Send" => Effect::Send,
        "Receive" => Effect::Receive,
        "Migrate" => Effect::Migrate,
        "STM" => Effect::STM,
        "Async" => Effect::Async,
        "Inference" => Effect::Inference,
        "Cost" => Effect::Cost,
        "Event" => Effect::Event,
        "Array" => Effect::Array,
        "FFI" => Effect::FFI,
        "Test" => Effect::Test,
        "DB" => Effect::DB,
        "Python" => Effect::Python,
        "Env" => Effect::Env,
        "Process" => Effect::Process,
        "System" => Effect::System,
        "Render" => Effect::Render,
        "Request" => Effect::Request,
        "Respond" => Effect::Respond,
        "Realtime" => Effect::Realtime,
        "Client" => Effect::Client,
        "Web" => Effect::Web,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Literal, Param};
    use crate::types::{Capability, Span, Type};

    fn behavior(
        name: &str,
        params: Vec<(&str, Type)>,
        ret: Option<Type>,
        effect: Option<EffectRow>,
    ) -> Behavior {
        Behavior {
            name: name.to_string(),
            params: params
                .into_iter()
                .map(|(name, ty)| Param {
                    name: name.to_string(),
                    ty: Some(ty),
                    cap: None,
                })
                .collect(),
            body: ast::Expr::Literal(Literal::Unit, Span::default()),
            effect,
            cap: Capability::Ref,
            ret_type: ret,
            span: Span::default(),
        }
    }

    #[test]
    fn complete_actor_protocol_uses_canonical_protocol_identity() {
        let behaviors = vec![behavior(
            "ping",
            vec![("value", Type::int())],
            Some(Type::int()),
            Some(EffectRow::singleton(Effect::Net)),
        )];
        let mut checker = EffectChecker::new();
        let (actor, effects) =
            actor_inventory("Worker", &behaviors, &mut checker).unwrap();

        assert_eq!(actor.protocol_status, "complete");
        assert!(actor
            .protocol_id
            .as_deref()
            .is_some_and(|id| id.starts_with("blake3:")));
        assert_eq!(effects[0].effects, vec!["Net"]);
    }

    #[test]
    fn missing_behavior_type_does_not_mint_protocol_identity() {
        let behaviors = vec![behavior(
            "ping",
            vec![("value", Type::int())],
            None,
            Some(EffectRow::empty()),
        )];
        let mut checker = EffectChecker::new();
        let (actor, _) =
            actor_inventory("Worker", &behaviors, &mut checker).unwrap();

        assert_eq!(actor.protocol_status, "incomplete-signature");
        assert!(actor.protocol_id.is_none());
    }

    #[test]
    fn authority_categories_are_derived_from_compiler_effects() {
        let effects = vec![
            SemanticEffect {
                subject_kind: "function".into(),
                subject: "fetch".into(),
                effects: vec!["Net".into(), "IO".into()],
                open: false,
            },
            SemanticEffect {
                subject_kind: "function".into(),
                subject: "load".into(),
                effects: vec!["FS".into(), "DB".into()],
                open: false,
            },
        ];

        assert_eq!(authority_categories(&effects), vec!["fs", "net", "os"]);
    }
}
