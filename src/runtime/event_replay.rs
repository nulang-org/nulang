//! Deterministic current-schema event replay.
//!
//! This is deliberately narrower than RFC 0008 event migration: it replays
//! events only when their durable schema identity already matches the current
//! actor artifact. Historical-version transformation is layered on top later.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::bytecode::{ActorMeta, CodeModule};
use crate::runtime::actor::Actor;
use crate::runtime::persistence::{
    durable_schema_compatible, EventEntry, PersistedValue,
};
use crate::vm::Value;

use super::migration::{persist_isolated_value, run_isolated_actor_function};
use super::map_ast_state_model;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventReplayResult {
    pub state: HashMap<String, PersistedValue>,
    pub field_sequences: HashMap<String, u64>,
}

fn resolve_event_meta<'a>(
    module: &'a CodeModule,
    events: &[EventEntry],
) -> Result<Option<&'a ActorMeta>, String> {
    let Some(first) = events.first() else {
        return Ok(None);
    };

    if let Some(owner) = first.schema_owner.as_deref() {
        return module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == owner)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "event schema owner '{}' is absent from the recovery module",
                    owner
                )
            });
    }

    if first.schema_version != 1 {
        return Err(format!(
            "legacy event history v{} has no compiler-owned schema owner",
            first.schema_version
        ));
    }

    let mut candidates = module.actor_metadata.iter().filter(|meta| meta.persistent);
    let first = candidates.next();
    match (first, candidates.next()) {
        (Some(meta), None) => Ok(Some(meta)),
        (None, _) => Ok(None),
        (Some(_), Some(_)) => Err(
            "legacy event history has no schema owner and the recovery module contains multiple persistent actors"
                .to_string(),
        ),
    }
}

fn validate_apply_binding(
    module: &CodeModule,
    meta: &ActorMeta,
    event: &str,
) -> Result<Option<usize>, String> {
    let Some(handler) = meta.apply_handlers.iter().find(|handler| handler.event == event) else {
        return Ok(None);
    };
    if !handler.replay_safe {
        return Ok(None);
    }

    let indexed_offset = *module
        .function_table
        .get(handler.function_index)
        .ok_or_else(|| {
            format!(
                "replay handler '{}.{}' references out-of-range function index {}",
                meta.name, event, handler.function_index
            )
        })?;
    let expected_name = format!("{}.$apply_{}", meta.name, event);
    let named_offset = module.function_offset_by_name(&expected_name).ok_or_else(|| {
        format!(
            "replay handler '{}.{}' is missing compiler-owned function '{}'",
            meta.name, event, expected_name
        )
    })?;
    if indexed_offset != named_offset {
        return Err(format!(
            "replay handler '{}.{}' binds function index {} at offset {}, but '{}' resolves to offset {}",
            meta.name,
            event,
            handler.function_index,
            indexed_offset,
            expected_name,
            named_offset
        ));
    }
    Ok(Some(handler.function_index))
}

/// Reconstruct current-schema event-sourced state by replaying one logical
/// event per durable sequence.
///
/// `Ok(None)` means executable replay is intentionally unavailable (legacy
/// artifact, old-schema history, missing/unsafe apply projection). Callers that
/// are preserving backward compatibility may use recorded post-apply values.
/// RFC 0008 migration callers must treat `None` as unsupported and fail closed.
pub(crate) fn replay_current_event_history(
    module: &CodeModule,
    actor_id: u64,
    events: &[EventEntry],
) -> Result<Option<EventReplayResult>, String> {
    let Some(meta) = resolve_event_meta(module, events)? else {
        return Ok(None);
    };
    if meta.is_workflow {
        return Ok(None);
    }

    let event_sourced_fields: HashSet<String> = meta
        .state_models
        .iter()
        .filter(|(_, model)| matches!(model, crate::ast::StateModel::EventSourced))
        .map(|(name, _)| name.clone())
        .collect();
    if event_sourced_fields.is_empty() {
        return Ok(None);
    }

    // Current-schema replay only. A future RFC 0008 layer transforms each
    // old-version logical event before this apply stage.
    for entry in events {
        if !durable_schema_compatible(
            entry.schema_owner.as_deref(),
            entry.schema_version,
            Some(meta.name.as_str()),
            meta.version,
        ) {
            return Ok(None);
        }
        if !event_sourced_fields.contains(&entry.field_name) {
            return Err(format!(
                "event sequence {} references field '{}' which is not event_sourced in current schema '{}@v{}'",
                entry.sequence, entry.field_name, meta.name, meta.version
            ));
        }
    }

    let mut logical: BTreeMap<u64, Vec<&EventEntry>> = BTreeMap::new();
    for entry in events {
        logical.entry(entry.sequence).or_default().push(entry);
    }

    let mut actor = Actor::new(actor_id, format!("event-replay:{}", meta.name), 0);
    actor.persistent = true;
    actor.schema_owner = Some(meta.name.clone());
    actor.schema_version = meta.version;
    actor.bytecode_module = Some(module.clone());
    actor.state_models = meta
        .state_models
        .iter()
        .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
        .collect();

    // Replay starts from current event-projection defaults, never from durable
    // snapshot state. The runtime callback fence makes non-event-sourced state
    // inaccessible even if a stale/tampered artifact attempts to reference it.
    for (name, constant) in &meta.state_defaults {
        if !event_sourced_fields.contains(name) {
            continue;
        }
        let value = match constant {
            crate::bytecode::Constant::String(s) => actor.allocate_string(s),
            other => crate::vm::constant_to_value(other),
        };
        actor.set_state_field(name, value);
    }

    let mut field_sequences = HashMap::new();

    for (sequence, siblings) in logical {
        let first = siblings[0];
        let mut seen_fields = HashSet::new();
        for sibling in &siblings {
            if sibling.event_name != first.event_name
                || sibling.args != first.args
                || sibling.schema_owner != first.schema_owner
                || sibling.schema_version != first.schema_version
            {
                return Err(format!(
                    "event sequence {} contains inconsistent per-field sibling rows",
                    sequence
                ));
            }
            if !seen_fields.insert(sibling.field_name.as_str()) {
                return Err(format!(
                    "event sequence {} contains duplicate projection row for field '{}'",
                    sequence, sibling.field_name
                ));
            }
        }

        let Some(function_idx) = validate_apply_binding(module, meta, &first.event_name)? else {
            return Ok(None);
        };
        let handler = meta
            .apply_handlers
            .iter()
            .find(|handler| handler.event == first.event_name)
            .expect("validated apply handler exists");
        if handler.param_count != first.args.len() {
            return Err(format!(
                "event '{}' at sequence {} has {} argument(s), but current replay handler expects {}",
                first.event_name,
                sequence,
                first.args.len(),
                handler.param_count
            ));
        }

        let args: Vec<Value> = first
            .args
            .iter()
            .map(|value| value.to_value_on_heap(&mut actor))
            .collect();
        run_isolated_actor_function(
            module,
            &mut actor,
            function_idx,
            &args,
            Some(event_sourced_fields.clone()),
            &format!(
                "current-schema replay of '{}.{}' at sequence {}",
                meta.name, first.event_name, sequence
            ),
        )?;

        // Preserve the current live semantics exactly: Runtime::emit_event
        // increments every integer event_sourced field once after the
        // compiler-inlined apply projection executes.
        for field in &event_sourced_fields {
            if let Some(value) = actor.get_state_field(field) {
                if let Some(current) = value.as_int() {
                    actor.set_state_field(field, Value::int(current + 1));
                }
            }
            field_sequences.insert(field.clone(), sequence);
        }
    }

    let mut state = HashMap::new();
    for field in &event_sourced_fields {
        let value = actor.get_state_field(field).ok_or_else(|| {
            format!(
                "event replay for '{}@v{}' did not produce event_sourced field '{}'",
                meta.name, meta.version, field
            )
        })?;
        state.insert(
            field.clone(),
            persist_isolated_value(&actor, field, &value)?,
        );
    }

    Ok(Some(EventReplayResult {
        state,
        field_sequences,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_module(source: &str) -> CodeModule {
        let tokens = crate::lexer::Lexer::new(source).lex().expect("lex");
        let ast = crate::parser::Parser::new(tokens)
            .parse_module()
            .expect("parse");
        let mut type_checker = crate::typechecker::TypeChecker::new();
        type_checker.check_module(&ast).expect("typecheck");
        let hir = crate::hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
        let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lower");
        crate::mir_codegen::compile_mir(&mut mir, "event-replay-test").expect("codegen")
    }

    fn entry(sequence: u64, field: &str, by: i64) -> EventEntry {
        EventEntry {
            sequence,
            schema_owner: Some("Counter".to_string()),
            schema_version: 1,
            field_name: field.to_string(),
            event_name: "Incremented".to_string(),
            args: vec![PersistedValue::Int(by)],
            value: PersistedValue::Int(-999),
        }
    }

    #[test]
    fn groups_sibling_projection_rows_and_applies_logical_event_once() {
        let module = compile_module(
            r#"
            entity Counter {
                state event_sourced count: Int = 0
                state event_sourced mirror: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => {
                        self.count = self.count + by
                        self.mirror = self.mirror + by
                    }
                behavior inc(by: Int) { emit Incremented(by) }
            }
            "#,
        );

        let events = vec![
            entry(1, "count", 3),
            entry(1, "mirror", 3),
            entry(2, "count", 4),
            entry(2, "mirror", 4),
        ];
        let replay = replay_current_event_history(&module, 7, &events)
            .unwrap()
            .expect("executable replay");

        // Each logical emit: apply +by once, then live runtime +1 once.
        assert_eq!(replay.state.get("count"), Some(&PersistedValue::Int(9)));
        assert_eq!(replay.state.get("mirror"), Some(&PersistedValue::Int(9)));
        assert_eq!(replay.field_sequences.get("count"), Some(&2));
        assert_eq!(replay.field_sequences.get("mirror"), Some(&2));
    }

    #[test]
    fn returns_legacy_fallback_for_non_replayable_projection() {
        let module = compile_module(
            r#"
            entity Counter {
                state event_sourced count: Int = 0
                state durable audit: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => {
                        self.audit = self.audit + by
                        self.count = self.count + by
                    }
                behavior inc(by: Int) { emit Incremented(by) }
            }
            "#,
        );
        let events = vec![entry(1, "count", 3)];
        assert!(
            replay_current_event_history(&module, 8, &events)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_inconsistent_sibling_rows() {
        let module = compile_module(
            r#"
            entity Counter {
                state event_sourced count: Int = 0
                state event_sourced mirror: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => {
                        self.count = self.count + by
                        self.mirror = self.mirror + by
                    }
                behavior inc(by: Int) { emit Incremented(by) }
            }
            "#,
        );
        let events = vec![entry(1, "count", 3), entry(1, "mirror", 4)];
        let error = replay_current_event_history(&module, 9, &events).unwrap_err();
        assert!(error.contains("inconsistent per-field sibling"), "{error}");
    }
}
