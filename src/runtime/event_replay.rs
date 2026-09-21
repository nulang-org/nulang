//! Deterministic event-history migration and replay.
//!
//! Persistence stores one EventEntry per event_sourced field projection. This
//! module first reconstructs logical domain events, optionally transforms old
//! schema events through RFC 0008 migration contracts in memory, and only then
//! executes the current-schema apply projection. Historical EventEntry rows are
//! never rewritten.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::bytecode::{ActorMeta, CodeModule};
use crate::runtime::actor::Actor;
use crate::runtime::persistence::{
    durable_schema_compatible, EventEntry, PersistedValue,
};
use crate::vm::Value;

use super::migration::{
    persist_isolated_value, run_isolated_actor_function, run_isolated_event_transform,
    validated_manifest,
};
use super::map_ast_state_model;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventReplayResult {
    pub state: HashMap<String, PersistedValue>,
    pub field_sequences: HashMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq)]
struct LogicalEvent {
    sequence: u64,
    schema_owner: Option<String>,
    schema_version: u32,
    event_name: String,
    args: Vec<PersistedValue>,
}

fn group_event_rows(events: &[EventEntry]) -> Result<Vec<LogicalEvent>, String> {
    let mut by_sequence: BTreeMap<u64, Vec<&EventEntry>> = BTreeMap::new();
    for entry in events {
        by_sequence.entry(entry.sequence).or_default().push(entry);
    }

    let mut logical = Vec::with_capacity(by_sequence.len());
    for (sequence, siblings) in by_sequence {
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

        logical.push(LogicalEvent {
            sequence,
            schema_owner: first.schema_owner.clone(),
            schema_version: first.schema_version,
            event_name: first.event_name.clone(),
            args: first.args.clone(),
        });
    }
    Ok(logical)
}

fn resolve_event_meta<'a>(
    module: &'a CodeModule,
    events: &[LogicalEvent],
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

fn validate_history_owner(meta: &ActorMeta, event: &LogicalEvent) -> Result<(), String> {
    match event.schema_owner.as_deref() {
        Some(owner) if owner != meta.name => Err(format!(
            "event sequence {} belongs to schema owner '{}' but recovery selected '{}'",
            event.sequence, owner, meta.name
        )),
        None if event.schema_version != 1 => Err(format!(
            "event sequence {} has schema version {} but no schema owner",
            event.sequence, event.schema_version
        )),
        _ => Ok(()),
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

fn transform_logical_event(
    module: &CodeModule,
    meta: &ActorMeta,
    actor_id: u64,
    event: LogicalEvent,
) -> Result<Vec<LogicalEvent>, String> {
    validate_history_owner(meta, &event)?;
    if event.schema_version > meta.version {
        return Err(format!(
            "event '{}' at sequence {} was written by future schema v{}, current '{}' is v{}",
            event.event_name, event.sequence, event.schema_version, meta.name, meta.version
        ));
    }
    if event.schema_version == meta.version {
        return Ok(vec![event]);
    }

    let manifest = validated_manifest(module, meta)?.ok_or_else(|| {
        format!(
            "event '{}' at sequence {} requires migration from v{} to v{}, but '{}' carries no migration manifest",
            event.event_name,
            event.sequence,
            event.schema_version,
            meta.version,
            meta.name
        )
    })?;
    let plan = manifest.plan_from(event.schema_version).map_err(|error| {
        format!(
            "event '{}' at sequence {} cannot migrate from v{} to '{}@v{}': {error}",
            event.event_name,
            event.sequence,
            event.schema_version,
            meta.name,
            meta.version
        )
    })?;

    let mut stream = vec![event];
    for contract in plan {
        let mut next = Vec::new();

        for current in stream {
            if current.schema_version != contract.from_version {
                return Err(format!(
                    "event migration internal version mismatch at sequence {}: event '{}' is v{}, contract is {} -> {}",
                    current.sequence,
                    current.event_name,
                    current.schema_version,
                    contract.from_version,
                    contract.to_version
                ));
            }

            if let Some(transform) = contract
                .event_transforms
                .iter()
                .find(|candidate| !candidate.catch_all && candidate.event_name == current.event_name)
            {
                if transform.parameter_count as usize != current.args.len() {
                    return Err(format!(
                        "migration {} -> {} event '{}' at sequence {} expects {} argument(s), history contains {}",
                        contract.from_version,
                        contract.to_version,
                        current.event_name,
                        current.sequence,
                        transform.parameter_count,
                        current.args.len()
                    ));
                }
                let function_idx = transform.function_index.ok_or_else(|| {
                    format!(
                        "migration {} -> {} event '{}' for '{}' has no executable private function binding",
                        contract.from_version,
                        contract.to_version,
                        current.event_name,
                        meta.name
                    )
                })?;
                let emitted = run_isolated_event_transform(
                    module,
                    actor_id,
                    function_idx,
                    &current.args,
                    &format!(
                        "event migration '{}.{}' {} -> {} at sequence {}",
                        meta.name,
                        current.event_name,
                        contract.from_version,
                        contract.to_version,
                        current.sequence
                    ),
                )?;
                for replacement in emitted {
                    next.push(LogicalEvent {
                        sequence: current.sequence,
                        schema_owner: Some(meta.name.clone()),
                        schema_version: contract.to_version,
                        event_name: replacement.event_name,
                        args: replacement.args,
                    });
                }
                continue;
            }

            if contract.event_transforms.iter().any(|candidate| candidate.catch_all) {
                next.push(LogicalEvent {
                    sequence: current.sequence,
                    schema_owner: Some(meta.name.clone()),
                    schema_version: contract.to_version,
                    event_name: current.event_name,
                    args: current.args,
                });
                continue;
            }

            return Err(format!(
                "migration {} -> {} for '{}' has no event arm for '{}' at sequence {} and no 'other => other' pass-through",
                contract.from_version,
                contract.to_version,
                meta.name,
                current.event_name,
                current.sequence
            ));
        }

        stream = next;
        if stream.is_empty() {
            break;
        }
    }

    for transformed in &stream {
        if transformed.schema_version != meta.version {
            return Err(format!(
                "event '{}' at sequence {} stopped migration at v{} instead of current v{}",
                transformed.event_name,
                transformed.sequence,
                transformed.schema_version,
                meta.version
            ));
        }
    }

    Ok(stream)
}

fn replay_logical_events(
    module: &CodeModule,
    meta: &ActorMeta,
    actor_id: u64,
    events: &[LogicalEvent],
    require_executable: bool,
) -> Result<Option<EventReplayResult>, String> {
    if meta.is_workflow {
        return if require_executable {
            Err(format!(
                "event migration for workflow actor '{}' is not implemented",
                meta.name
            ))
        } else {
            Ok(None)
        };
    }

    let event_sourced_fields: HashSet<String> = meta
        .state_models
        .iter()
        .filter(|(_, model)| matches!(model, crate::ast::StateModel::EventSourced))
        .map(|(name, _)| name.clone())
        .collect();
    if event_sourced_fields.is_empty() {
        return if require_executable {
            Err(format!(
                "migrated event history for '{}' has no current event_sourced projection fields",
                meta.name
            ))
        } else {
            Ok(None)
        };
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

    for event in events {
        if !durable_schema_compatible(
            event.schema_owner.as_deref(),
            event.schema_version,
            Some(meta.name.as_str()),
            meta.version,
        ) {
            return Err(format!(
                "logical event '{}' at sequence {} did not reach current schema '{}@v{}'",
                event.event_name, event.sequence, meta.name, meta.version
            ));
        }

        let Some(function_idx) = validate_apply_binding(module, meta, &event.event_name)? else {
            if require_executable {
                return Err(format!(
                    "migrated event '{}' at sequence {} has no replay-safe current apply projection",
                    event.event_name, event.sequence
                ));
            }
            return Ok(None);
        };
        let handler = meta
            .apply_handlers
            .iter()
            .find(|handler| handler.event == event.event_name)
            .expect("validated apply handler exists");
        if handler.param_count != event.args.len() {
            return Err(format!(
                "event '{}' at sequence {} has {} argument(s), but current replay handler expects {}",
                event.event_name,
                event.sequence,
                event.args.len(),
                handler.param_count
            ));
        }

        let args: Vec<Value> = event
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
                meta.name, event.event_name, event.sequence
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
            field_sequences.insert(field.clone(), event.sequence);
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

/// Reconstruct event-sourced state from persisted history.
///
/// Current-schema histories retain the compatibility behavior introduced by
/// #699: if a current apply handler lacks a replay-safe private artifact,
/// callers may fall back to the recorded post-apply EventEntry.value.
///
/// Historical-schema histories are different: recorded projection values are
/// old-schema data and cannot safely be used under the current schema. Such
/// histories must migrate all logical events to the current schema and execute
/// replay-safe current apply projections; missing transforms or projections
/// fail closed.
pub(crate) fn replay_event_history(
    module: &CodeModule,
    actor_id: u64,
    events: &[EventEntry],
) -> Result<Option<EventReplayResult>, String> {
    let logical = group_event_rows(events)?;
    let Some(meta) = resolve_event_meta(module, &logical)? else {
        return Ok(None);
    };

    let has_historical_events = logical.iter().any(|event| {
        !durable_schema_compatible(
            event.schema_owner.as_deref(),
            event.schema_version,
            Some(meta.name.as_str()),
            meta.version,
        )
    });

    if !has_historical_events {
        // A current-schema persisted row must still refer to a field that is
        // part of today's event-sourced projection. Historical rows are not
        // checked here because their field names may legitimately have been
        // renamed or removed.
        let current_fields: HashSet<&str> = meta
            .state_models
            .iter()
            .filter(|(_, model)| matches!(model, crate::ast::StateModel::EventSourced))
            .map(|(name, _)| name.as_str())
            .collect();
        for entry in events {
            if !current_fields.contains(entry.field_name.as_str()) {
                return Err(format!(
                    "event sequence {} references field '{}' which is not event_sourced in current schema '{}@v{}'",
                    entry.sequence, entry.field_name, meta.name, meta.version
                ));
            }
        }
        return replay_logical_events(module, meta, actor_id, &logical, false);
    }

    let mut migrated = Vec::new();
    for event in logical {
        migrated.extend(transform_logical_event(module, meta, actor_id, event)?);
    }

    replay_logical_events(module, meta, actor_id, &migrated, true)
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

    fn entry_with(
        sequence: u64,
        field: &str,
        schema_version: u32,
        event_name: &str,
        args: Vec<PersistedValue>,
    ) -> EventEntry {
        EventEntry {
            sequence,
            schema_owner: Some("Counter".to_string()),
            schema_version,
            field_name: field.to_string(),
            event_name: event_name.to_string(),
            args,
            value: PersistedValue::Int(-999),
        }
    }

    fn entry(sequence: u64, field: &str, by: i64) -> EventEntry {
        entry_with(
            sequence,
            field,
            1,
            "Incremented",
            vec![PersistedValue::Int(by)],
        )
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
        let replay = replay_event_history(&module, 7, &events)
            .unwrap()
            .expect("executable replay");

        assert_eq!(replay.state.get("count"), Some(&PersistedValue::Int(9)));
        assert_eq!(replay.state.get("mirror"), Some(&PersistedValue::Int(9)));
        assert_eq!(replay.field_sequences.get("count"), Some(&2));
        assert_eq!(replay.field_sequences.get("mirror"), Some(&2));
    }

    #[test]
    fn returns_legacy_fallback_for_non_replayable_current_projection() {
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
        assert!(replay_event_history(&module, 8, &events).unwrap().is_none());
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
        let error = replay_event_history(&module, 9, &events).unwrap_err();
        assert!(error.contains("inconsistent per-field sibling"), "{error}");
    }

    #[test]
    fn migrates_named_historical_event_then_replays_current_apply() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state event_sourced count: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => { self.count = self.count + by }
                behavior inc(by: Int) { emit Incremented(by) }

                migration from 1 to 2 {
                    events {
                        | Added(by) => emit Incremented(by)
                        | other => other
                    }
                }
            }
            "#,
        );

        let events = vec![entry_with(
            1,
            "legacy_count",
            1,
            "Added",
            vec![PersistedValue::Int(3)],
        )];
        let replay = replay_event_history(&module, 10, &events)
            .unwrap()
            .expect("migrated replay");
        assert_eq!(replay.state.get("count"), Some(&PersistedValue::Int(4)));
        assert_eq!(replay.field_sequences.get("count"), Some(&1));
    }

    #[test]
    fn migration_event_can_split_into_multiple_current_events_in_order() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state event_sourced count: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => { self.count = self.count + by }
                behavior inc(by: Int) { emit Incremented(by) }

                migration from 1 to 2 {
                    events {
                        | Added(by) => {
                            emit Incremented(by)
                            emit Incremented(1)
                        }
                    }
                }
            }
            "#,
        );

        let events = vec![entry_with(
            7,
            "legacy_count",
            1,
            "Added",
            vec![PersistedValue::Int(3)],
        )];
        let replay = replay_event_history(&module, 11, &events)
            .unwrap()
            .expect("migrated replay");
        // First logical event: +3 then runtime +1 => 4.
        // Second logical event: +1 then runtime +1 => 6.
        assert_eq!(replay.state.get("count"), Some(&PersistedValue::Int(6)));
        assert_eq!(replay.field_sequences.get("count"), Some(&7));
    }

    #[test]
    fn catchall_passes_historical_event_through_without_bytecode() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state event_sourced count: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => { self.count = self.count + by }
                behavior inc(by: Int) { emit Incremented(by) }

                migration from 1 to 2 {
                    events {
                        | other => other
                    }
                }
            }
            "#,
        );

        let events = vec![entry_with(
            3,
            "legacy_count",
            1,
            "Incremented",
            vec![PersistedValue::Int(2)],
        )];
        let replay = replay_event_history(&module, 12, &events)
            .unwrap()
            .expect("catch-all migrated replay");
        assert_eq!(replay.state.get("count"), Some(&PersistedValue::Int(3)));
    }

    #[test]
    fn historical_event_without_matching_arm_or_catchall_fails_closed() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state event_sourced count: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => { self.count = self.count + by }
                behavior inc(by: Int) { emit Incremented(by) }

                migration from 1 to 2 {
                    events {
                        | Added(by) => emit Incremented(by)
                    }
                }
            }
            "#,
        );

        let events = vec![entry_with(
            4,
            "legacy_count",
            1,
            "Unknown",
            vec![PersistedValue::Int(2)],
        )];
        let error = replay_event_history(&module, 13, &events).unwrap_err();
        assert!(error.contains("no event arm for 'Unknown'"), "{error}");
    }

    #[test]
    fn event_transform_state_access_fails_closed_until_old_state_view_exists() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state event_sourced count: Int = 0
                events
                    | Incremented(by: Int)
                apply
                    | Incremented(by) => { self.count = self.count + by }
                behavior inc(by: Int) { emit Incremented(by) }

                migration from 1 to 2 {
                    events {
                        | Added(by) => emit Incremented(self.count + by)
                    }
                }
            }
            "#,
        );

        let events = vec![entry_with(
            5,
            "legacy_count",
            1,
            "Added",
            vec![PersistedValue::Int(2)],
        )];
        let error = replay_event_history(&module, 14, &events).unwrap_err();
        assert!(error.contains("forbidden state field 'count'"), "{error}");
    }
}
