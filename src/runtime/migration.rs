//! Isolated RFC 0008 state-migration execution.
//!
//! Migration bytecode runs in a fresh VM bound directly to an unpublished
//! `Actor`. It never enters `Runtime::actors`, the scheduler, registries,
//! process groups, or distribution. The callback surface records any attempt
//! to escape pure state transformation and the migration fails closed.

use std::sync::{Arc, Mutex};

use crate::bytecode::{ActorMeta, CodeModule};
use crate::migration_manifest::MigrationManifest;
use crate::runtime::actor::Actor;
use crate::runtime::heap::{ActorHeap, TypeTag};
use crate::runtime::persistence::{ActorSnapshot, PersistedValue, StateModel};
use crate::vm::{
    ActorVmCallbacks, DistributedVmCallbacks, PerformAsyncResult, SignalWaitResult, Value, VM,
};

/// Durable-history facts gathered by the caller before attempting a state-only
/// migration. V1 deliberately refuses histories that would need semantic replay
/// under a different schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct StateMigrationHistory {
    pub has_pending_message_journal: bool,
    pub has_event_history: bool,
    pub has_workflow_history: bool,
}

#[derive(Debug, Clone, Default)]
struct ViolationFlag(Arc<Mutex<Option<String>>>);

impl ViolationFlag {
    fn record(&self, message: impl Into<String>) {
        if let Ok(mut slot) = self.0.lock() {
            if slot.is_none() {
                *slot = Some(message.into());
            }
        }
    }

    fn take(&self) -> Option<String> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// VM callbacks for one unpublished migration actor.
///
/// Only heap ownership and state get/set are real. Every operation capable of
/// producing an external side effect records a violation. Compiler purity is
/// the first line of defense; these callbacks are the artifact/runtime fence.
struct MigrationActorCallbacks {
    actor: *mut Actor,
    violation: ViolationFlag,
    allowed_state_fields: Option<std::collections::HashSet<String>>,
}

impl std::fmt::Debug for MigrationActorCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MigrationActorCallbacks(actor={:p})", self.actor)
    }
}

impl MigrationActorCallbacks {
    fn new(
        actor: &mut Actor,
        violation: ViolationFlag,
        allowed_state_fields: Option<std::collections::HashSet<String>>,
    ) -> Self {
        Self {
            actor: actor as *mut Actor,
            violation,
            allowed_state_fields,
        }
    }

    fn state_field_allowed(&self, field: &str) -> bool {
        self.allowed_state_fields
            .as_ref()
            .map(|fields| fields.contains(field))
            .unwrap_or(true)
    }
}

impl ActorVmCallbacks for MigrationActorCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        // SAFETY: the caller keeps the staged actor alive for the VM run.
        Some(unsafe { (*self.actor).id })
    }

    fn alloc(&mut self, size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        // SAFETY: the staged actor is exclusively owned by the migration
        // executor for the lifetime of this callback.
        unsafe { (*self.actor).heap.alloc(size, type_tag) }
    }

    fn alloc_arena(&mut self, size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        unsafe { (*self.actor).iso_arena.alloc(size, type_tag) }
    }

    fn reset_arena(&mut self) {
        unsafe { (*self.actor).iso_arena.reset() }
    }

    fn is_arena_ptr(&self, ptr: *const u8) -> bool {
        unsafe { (*self.actor).iso_arena.contains(ptr) }
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        unsafe {
            if (*self.actor).iso_arena.contains(ptr) {
                return;
            }
            (*self.actor)
                .orca_gc
                .drop_local_ref(&mut (*self.actor).heap, ptr);
        }
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        unsafe {
            if (*self.actor).iso_arena.contains(ptr) {
                return;
            }
            (*self.actor).orca_gc.local_ref(&(*self.actor).heap, ptr);
        }
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        unsafe {
            let header = &*ActorHeap::header_of(ptr);
            if header.type_tag == TypeTag::Array {
                let payload = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                Some(payload / std::mem::size_of::<Value>())
            } else {
                None
            }
        }
    }

    fn get_state_field(&self, field: &str) -> Value {
        if !self.state_field_allowed(field) {
            self.violation.record(format!(
                "isolated execution attempted to read forbidden state field '{field}'"
            ));
            return Value::nil();
        }
        unsafe { (*self.actor).get_state_field(field).unwrap_or(Value::nil()) }
    }

    fn set_state_field(&mut self, field: &str, value: Value) {
        if !self.state_field_allowed(field) {
            self.violation.record(format!(
                "isolated execution attempted to write forbidden state field '{field}'"
            ));
            return;
        }
        unsafe {
            if (*self.actor)
                .state_models
                .get(field)
                .map(|model| model.is_crdt())
                .unwrap_or(false)
            {
                self.violation.record(format!(
                    "migration attempted raw assignment to CRDT field '{field}'"
                ));
                return;
            }
            (*self.actor).set_state_field(field, value);
        }
    }

    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _spawn_pc: usize,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {
        self.violation
            .record("migration attempted to spawn an actor");
        Value::actor_ref(0)
    }

    fn send_message(&mut self, _target: Value, _behavior_id: u16, _args: &[Value]) {
        self.violation
            .record("migration attempted to send an actor message");
    }

    fn ask_actor(&mut self, _target: Value, _behavior_id: u16, _args: &[Value]) -> Value {
        self.violation
            .record("migration attempted to ask another actor");
        Value::nil()
    }

    fn emit_event(&mut self, event: &str, _args: &[Value]) {
        self.violation
            .record(format!("migration attempted to emit event '{event}'"));
    }

    fn authorize_ffi(&mut self, library: &str, symbol: &str) -> bool {
        self.violation.record(format!(
            "migration attempted FFI call '{library}::{symbol}'"
        ));
        false
    }

    fn perform_effect(&mut self, effect_name: &str, _regs: &[Value]) -> Option<Value> {
        self.violation
            .record(format!("migration attempted effect '{effect_name}'"));
        None
    }

    fn perform_builtin_effect(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        _constants: &[crate::bytecode::Constant],
        _regs: &[Value],
    ) -> Option<Value> {
        self.violation.record(format!(
            "migration attempted builtin effect '{}{}'",
            effect_name,
            op_name.map(|op| format!(".{op}")).unwrap_or_default()
        ));
        None
    }

    fn complete_llm(&mut self, model: &str, _prompt: &str) -> Option<String> {
        self.violation.record(format!(
            "migration attempted LLM request using model '{model}'"
        ));
        None
    }

    fn perform_async(
        &mut self,
        effect_op: &str,
        _constants: &[crate::bytecode::Constant],
        _args: &[Value],
    ) -> PerformAsyncResult {
        self.violation
            .record(format!("migration attempted async effect '{effect_op}'"));
        PerformAsyncResult::Ready(None)
    }

    fn try_receive(&mut self) -> Option<(u16, Value)> {
        self.violation.record("migration attempted mailbox receive");
        None
    }

    fn try_receive_match(&mut self, _behavior_ids: &[u16]) -> Option<(usize, Vec<Value>)> {
        self.violation
            .record("migration attempted selective mailbox receive");
        None
    }

    fn receive_wait_suspend(&mut self, _timeout_ms: i64) -> bool {
        self.violation
            .record("migration attempted timed mailbox receive");
        false
    }

    fn wait_signal(&mut self, name: &str) -> SignalWaitResult {
        self.violation
            .record(format!("migration attempted to wait for signal '{name}'"));
        SignalWaitResult::Ready(Value::unit())
    }
}

#[derive(Debug)]
struct MigrationDistributedCallbacks {
    violation: ViolationFlag,
}

impl DistributedVmCallbacks for MigrationDistributedCallbacks {
    fn node_id(&self) -> u64 {
        self.violation
            .record("migration transform attempted to observe runtime node identity");
        0
    }

    fn migrate(&mut self, _actor_id: u64, _target_node_id: u64) {
        self.violation
            .record("migration transform attempted actor node migration");
    }

    fn remote_ask(
        &mut self,
        _target_actor: u64,
        _behavior: &str,
        _args: &[Value],
        _timeout_ms: u64,
    ) -> Value {
        self.violation
            .record("migration transform attempted remote ask");
        Value::nil()
    }

    fn remote_send(
        &mut self,
        _target_actor: u64,
        _target_node: u64,
        _behavior: &str,
        _args: &[Value],
    ) {
        self.violation
            .record("migration transform attempted remote send");
    }

    fn gossip(&mut self, _message: &str) -> Value {
        self.violation
            .record("migration transform attempted gossip");
        Value::unit()
    }

    fn remote_spawn(
        &mut self,
        _target_node: u64,
        _behavior: &str,
        _init: &[(String, Value)],
    ) -> Value {
        self.violation
            .record("migration transform attempted remote spawn");
        Value::actor_ref(0)
    }
}

/// Resolve the compiler-owned declaration that owns a persisted snapshot.
pub(crate) fn resolve_snapshot_meta<'a>(
    module: &'a CodeModule,
    snapshot: &ActorSnapshot,
) -> Result<Option<&'a ActorMeta>, String> {
    if let Some(owner) = snapshot.schema_owner.as_deref() {
        return module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == owner)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "snapshot schema owner '{}' is absent from the recovery module",
                    owner
                )
            });
    }

    if snapshot.schema_version != 1 {
        return Err(format!(
            "legacy snapshot v{} has no compiler-owned schema metadata",
            snapshot.schema_version
        ));
    }

    let mut candidates = module.actor_metadata.iter().filter(|meta| meta.persistent);
    let first = candidates.next();
    match (first, candidates.next()) {
        (Some(meta), None) => Ok(Some(meta)),
        (None, _) => Ok(None),
        (Some(_), Some(_)) => Err(
            "legacy snapshot has no schema owner and the recovery module contains multiple persistent actors"
                .to_string(),
        ),
    }
}

/// Parse and validate the migration manifest plus its private code bindings.
pub(crate) fn validated_manifest(
    module: &CodeModule,
    meta: &ActorMeta,
) -> Result<Option<MigrationManifest>, String> {
    if meta.migrations.is_empty() {
        return Ok(None);
    }

    let manifest = MigrationManifest::from_json(&meta.migrations).map_err(|error| {
        format!(
            "actor '{}' carries an invalid migration manifest: {error}",
            meta.name
        )
    })?;
    if manifest.target_version != meta.version {
        return Err(format!(
            "actor '{}' migration manifest targets v{} but actor metadata declares v{}",
            meta.name, manifest.target_version, meta.version
        ));
    }

    for contract in &manifest.contracts {
        let Some(function_idx) = contract.state_function_index else {
            continue;
        };
        let function_offset = module
            .function_table
            .get(function_idx)
            .copied()
            .ok_or_else(|| {
                format!(
                    "actor '{}' migration {} -> {} binds out-of-range function index {}",
                    meta.name, contract.from_version, contract.to_version, function_idx
                )
            })?;
        let expected_name = format!(
            "{}.$migration_state_{}_{}",
            meta.name, contract.from_version, contract.to_version
        );
        let named_offset = module.function_offset_by_name(&expected_name).ok_or_else(|| {
            format!(
                "actor '{}' migration {} -> {} binds function index {} but compiler-owned function '{}' is absent",
                meta.name,
                contract.from_version,
                contract.to_version,
                function_idx,
                expected_name
            )
        })?;
        if named_offset != function_offset {
            return Err(format!(
                "actor '{}' migration {} -> {} binds function index {} at offset {}, but '{}' resolves to offset {}",
                meta.name,
                contract.from_version,
                contract.to_version,
                function_idx,
                function_offset,
                expected_name,
                named_offset
            ));
        }
    }

    Ok(Some(manifest))
}

pub(crate) fn persist_isolated_value(
    actor: &Actor,
    field: &str,
    value: &Value,
) -> Result<PersistedValue, String> {
    if let Some(ptr) = value.as_ptr() {
        if ptr.is_null() {
            return Ok(PersistedValue::String(String::new()));
        }
        let header = unsafe { &*ActorHeap::header_of(ptr) };
        if header.type_tag != TypeTag::String {
            return Err(format!(
                "isolated execution produced unsupported heap-backed durable value for field '{}': {:?}",
                field, header.type_tag
            ));
        }
        let content = unsafe {
            std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char)
                .to_str()
                .map_err(|error| {
                    format!(
                        "isolated execution produced non-UTF-8 durable string for field '{}': {error}",
                        field
                    )
                })?
                .to_string()
        };
        return Ok(PersistedValue::String(content));
    }

    let persisted = PersistedValue::from_value_resolved(value, actor.bytecode_module.as_ref());
    if matches!(persisted, PersistedValue::Nil) && !value.is_nil() {
        return Err(format!(
            "isolated execution produced unsupported durable value for field '{}'",
            field
        ));
    }
    Ok(persisted)
}

/// Execute one compiler-private function against an unpublished actor.
///
/// `allowed_state_fields` installs a runtime capability fence around actor
/// state access. `None` permits ordinary migration state access (CRDT raw
/// writes are still denied); replay passes an explicit event-sourced set.
pub(crate) fn run_isolated_actor_function(
    module: &CodeModule,
    actor: &mut Actor,
    function_idx: usize,
    args: &[Value],
    allowed_state_fields: Option<std::collections::HashSet<String>>,
    context: &str,
) -> Result<Value, String> {
    let offset = *module
        .function_table
        .get(function_idx)
        .ok_or_else(|| format!("{context} references missing function index {function_idx}"))?;
    let violation = ViolationFlag::default();
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm.set_actor_callbacks(Box::new(MigrationActorCallbacks::new(
        actor,
        violation.clone(),
        allowed_state_fields,
    )));
    vm.set_distributed_callbacks(Box::new(MigrationDistributedCallbacks {
        violation: violation.clone(),
    }));

    let result = vm
        .call_function(0, offset, args)
        .map_err(|error| format!("{context} failed: {error}"))?;
    if let Some(reason) = violation.take() {
        return Err(format!(
            "{context} violated the isolated execution boundary: {reason}"
        ));
    }
    Ok(result)
}

/// Execute a state-only migration chain against an unpublished actor and
/// return an upgraded snapshot. `Ok(None)` means no upgrade is required.
///
/// The caller is responsible for persisting the returned snapshot before
/// publishing or scheduling a recovered actor.
pub(crate) fn migrate_snapshot_state(
    module: &CodeModule,
    snapshot: &ActorSnapshot,
    history: StateMigrationHistory,
) -> Result<Option<ActorSnapshot>, String> {
    let Some(meta) = resolve_snapshot_meta(module, snapshot)? else {
        return Ok(None);
    };

    if snapshot.schema_version >= meta.version {
        return Ok(None);
    }

    if meta.is_workflow {
        return Err(format!(
            "state-only migration for '{}' refuses workflow snapshots until workflow-event migration is implemented",
            meta.name
        ));
    }
    if history.has_workflow_history {
        return Err(format!(
            "state-only migration for '{}' refuses persisted workflow history",
            meta.name
        ));
    }
    if history.has_event_history {
        return Err(format!(
            "state-only migration for '{}' refuses event-sourced history until event migration is implemented",
            meta.name
        ));
    }
    if history.has_pending_message_journal {
        return Err(format!(
            "state-only migration for '{}' refuses old-schema journal entries newer than the snapshot",
            meta.name
        ));
    }
    if snapshot
        .crdt_snapshot
        .as_ref()
        .map(|entries| !entries.is_empty())
        .unwrap_or(false)
        || snapshot
            .crdt_field_map
            .as_ref()
            .map(|entries| !entries.is_empty())
            .unwrap_or(false)
    {
        return Err(format!(
            "state-only migration for '{}' refuses CRDT snapshot state",
            meta.name
        ));
    }

    for (field, model) in &meta.state_models {
        match model {
            crate::ast::StateModel::EventSourced => {
                return Err(format!(
                    "state-only migration for '{}' refuses event-sourced field '{}'",
                    meta.name, field
                ));
            }
            crate::ast::StateModel::Crdt(_) => {
                return Err(format!(
                    "state-only migration for '{}' refuses CRDT field '{}'",
                    meta.name, field
                ));
            }
            crate::ast::StateModel::Local | crate::ast::StateModel::Durable => {}
        }
    }

    let manifest = validated_manifest(module, meta)?.ok_or_else(|| {
        format!(
            "persisted {}@v{} requires migration to {}@v{}, but this artifact has no migration manifest",
            snapshot
                .schema_owner
                .as_deref()
                .unwrap_or(meta.name.as_str()),
            snapshot.schema_version,
            meta.name,
            meta.version
        )
    })?;
    let plan = manifest
        .plan_from(snapshot.schema_version)
        .map_err(|error| {
            format!(
                "persisted {}@v{} cannot be migrated to {}@v{}: {error}",
                snapshot
                    .schema_owner
                    .as_deref()
                    .unwrap_or(meta.name.as_str()),
                snapshot.schema_version,
                meta.name,
                meta.version
            )
        })?;

    for step in &plan {
        if !step.event_transforms.is_empty() {
            return Err(format!(
                "migration {} -> {} for '{}' declares event transforms; event migration execution is not implemented",
                step.from_version, step.to_version, meta.name
            ));
        }
        if step.has_state_transform && step.state_function_index.is_none() {
            return Err(format!(
                "migration {} -> {} for '{}' has a state transform but no private executable function binding",
                step.from_version, step.to_version, meta.name
            ));
        }
    }

    let mut actor = Actor::new(snapshot.actor_id, format!("migration:{}", meta.name), 0);
    actor.persistent = true;
    actor.schema_owner = Some(meta.name.clone());
    actor.schema_version = snapshot.schema_version;
    actor.bytecode_module = Some(module.clone());
    actor.state_models = meta
        .state_models
        .iter()
        .map(|(name, model)| (name.clone(), super::map_ast_state_model(*model)))
        .collect();

    for (name, value) in &snapshot.state {
        let restored = value.to_value_on_heap(&mut actor);
        actor.set_state_field(name, restored);
    }

    // Current-schema defaults exist before each migration body. A body can
    // override them, while newly-added fields that need no custom transform
    // still obtain their declared default.
    for (name, constant) in &meta.state_defaults {
        if actor.get_state_field(name).is_some() {
            continue;
        }
        let value = match constant {
            crate::bytecode::Constant::String(s) => actor.allocate_string(s),
            other => crate::vm::constant_to_value(other),
        };
        actor.set_state_field(name, value);
    }

    let violation = ViolationFlag::default();
    let mut vm = VM::new();
    vm.load_module(module.clone());
    vm.set_actor_callbacks(Box::new(MigrationActorCallbacks::new(
        &mut actor,
        violation.clone(),
        None,
    )));
    vm.set_distributed_callbacks(Box::new(MigrationDistributedCallbacks {
        violation: violation.clone(),
    }));

    for step in &plan {
        if let Some(function_idx) = step.state_function_index {
            let offset = *module.function_table.get(function_idx).ok_or_else(|| {
                format!(
                    "migration {} -> {} for '{}' references missing function index {}",
                    step.from_version, step.to_version, meta.name, function_idx
                )
            })?;
            vm.call_function(0, offset, &[]).map_err(|error| {
                format!(
                    "migration {} -> {} for '{}' failed: {error}",
                    step.from_version, step.to_version, meta.name
                )
            })?;
            if let Some(reason) = violation.take() {
                return Err(format!(
                    "migration {} -> {} for '{}' violated the state-only execution boundary: {}",
                    step.from_version, step.to_version, meta.name, reason
                ));
            }
        }
        actor.schema_version = step.to_version;
    }

    drop(vm);

    if actor.schema_version != meta.version {
        return Err(format!(
            "migration for '{}' stopped at v{} instead of target v{}",
            meta.name, actor.schema_version, meta.version
        ));
    }

    let mut state = std::collections::HashMap::new();
    for (name, model) in &actor.state_models {
        if *model != StateModel::Durable {
            continue;
        }
        let value = actor.get_state_field(name).ok_or_else(|| {
            format!(
                "migration for '{}' did not produce required durable field '{}'",
                meta.name, name
            )
        })?;
        state.insert(name.clone(), persist_isolated_value(&actor, name, &value)?);
    }

    Ok(Some(ActorSnapshot {
        actor_id: snapshot.actor_id,
        sequence: snapshot.sequence,
        schema_owner: Some(meta.name.clone()),
        schema_version: meta.version,
        state,
        waiting_signal: snapshot.waiting_signal.clone(),
        crdt_snapshot: None,
        crdt_field_map: None,
        authority_tokens: snapshot.authority_tokens.clone(),
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
        crate::mir_codegen::compile_mir(&mut mir, "migration-test").expect("codegen")
    }

    #[test]
    fn migrates_durable_snapshot_without_publishing_actor() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state durable count: Int = 0
                migration from 1 to 2 {
                    state => { self.count = self.count + 5 }
                }
            }
            "#,
        );
        let mut snapshot = ActorSnapshot {
            actor_id: 42,
            sequence: 7,
            schema_owner: Some("Counter".to_string()),
            schema_version: 1,
            ..ActorSnapshot::default()
        };
        snapshot
            .state
            .insert("count".to_string(), PersistedValue::Int(10));

        let upgraded = migrate_snapshot_state(&module, &snapshot, StateMigrationHistory::default())
            .unwrap()
            .expect("upgrade");

        assert_eq!(
            upgraded.sequence, 7,
            "migration must not advance journal sequence"
        );
        assert_eq!(upgraded.schema_version, 2);
        assert_eq!(upgraded.state.get("count"), Some(&PersistedValue::Int(15)));
    }

    #[test]
    fn applies_adjacent_state_migrations_in_order() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 3
                state durable count: Int = 0
                migration from 1 to 2 {
                    state => { self.count = self.count + 2 }
                }
                migration from 2 to 3 {
                    state => { self.count = self.count * 3 }
                }
            }
            "#,
        );
        let mut snapshot = ActorSnapshot {
            actor_id: 43,
            sequence: 2,
            schema_owner: Some("Counter".to_string()),
            schema_version: 1,
            ..ActorSnapshot::default()
        };
        snapshot
            .state
            .insert("count".to_string(), PersistedValue::Int(4));

        let upgraded = migrate_snapshot_state(&module, &snapshot, StateMigrationHistory::default())
            .unwrap()
            .unwrap();
        assert_eq!(upgraded.state.get("count"), Some(&PersistedValue::Int(18)));
        assert_eq!(upgraded.schema_version, 3);
    }

    #[test]
    fn refuses_pending_old_schema_message_journal() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state durable count: Int = 0
                migration from 1 to 2 {
                    state => { self.count = self.count + 1 }
                }
            }
            "#,
        );
        let snapshot = ActorSnapshot {
            actor_id: 44,
            sequence: 5,
            schema_owner: Some("Counter".to_string()),
            schema_version: 1,
            ..ActorSnapshot::default()
        };

        let error = migrate_snapshot_state(
            &module,
            &snapshot,
            StateMigrationHistory {
                has_pending_message_journal: true,
                ..StateMigrationHistory::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("old-schema journal entries"), "{error}");
    }

    #[test]
    fn refuses_event_sourced_schema_until_event_executor_exists() {
        let module = compile_module(
            r#"
            entity Counter {
                version: 2
                state event_sourced count: Int = 0
                migration from 1 to 2 {
                    state => { self.count = self.count + 1 }
                }
            }
            "#,
        );
        let snapshot = ActorSnapshot {
            actor_id: 46,
            sequence: 1,
            schema_owner: Some("Counter".to_string()),
            schema_version: 1,
            ..ActorSnapshot::default()
        };

        let error = migrate_snapshot_state(&module, &snapshot, StateMigrationHistory::default())
            .unwrap_err();
        assert!(error.contains("event-sourced field"), "{error}");
    }

    #[test]
    fn preserves_heap_string_results_losslessly() {
        let module = compile_module(
            r#"
            entity Profile {
                version: 2
                state durable name: String = ""
                migration from 1 to 2 {
                    state => { self.name = self.name + "!" }
                }
            }
            "#,
        );
        let mut snapshot = ActorSnapshot {
            actor_id: 45,
            sequence: 1,
            schema_owner: Some("Profile".to_string()),
            schema_version: 1,
            ..ActorSnapshot::default()
        };
        snapshot.state.insert(
            "name".to_string(),
            PersistedValue::String("Ada".to_string()),
        );

        let upgraded = migrate_snapshot_state(&module, &snapshot, StateMigrationHistory::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            upgraded.state.get("name"),
            Some(&PersistedValue::String("Ada!".to_string()))
        );
    }
}
