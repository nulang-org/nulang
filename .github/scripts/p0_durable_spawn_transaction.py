from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    file_path = Path(path)
    text = file_path.read_text()
    count = text.count(old)
    assert count == 1, f"{label}: expected one anchor, found {count}"
    file_path.write_text(text.replace(old, new, 1))


# workflow.rs: preserve legacy best-effort checkpoint semantics, while adding
# a fallible local-commit-first path for initial durable workflow publication.
path = Path("src/runtime/workflow.rs")
text = path.read_text()
start = text.index(
    "/// Snapshot the durable and CRDT state of a persistent actor.\n"
    "pub(crate) fn checkpoint_actor"
)
end = text.index(
    "// ---------------------------------------------------------------------------\n"
    "// Event emission",
    start,
)
replacement = '''/// Build the next persistent snapshot without producing external side effects.
fn build_actor_snapshot(
    rt: &Runtime,
    actor_id: u64,
) -> std::io::Result<Option<(crate::runtime::persistence::ActorSnapshot, u64)>> {
    let actor = match rt.actors.get(&actor_id) {
        Some(actor) => actor,
        None => return Ok(None),
    };
    if !actor.persistent {
        return Ok(None);
    }
    let seq = next_sequence(rt, actor_id);
    let mut state = std::collections::HashMap::new();
    for (name, value) in &actor.state_data {
        let model = actor
            .state_models
            .get(name)
            .copied()
            .unwrap_or(StateModel::Local);
        if model == StateModel::Durable || model.is_crdt() {
            let persisted = if name == "semantic_memory" || name == "procedural_memory" {
                vm_value_to_string_in_actor(value, actor)
                    .map(PersistedValue::String)
                    .unwrap_or_else(|| {
                        PersistedValue::from_value_resolved(value, actor.bytecode_module.as_ref())
                    })
            } else {
                PersistedValue::from_value_resolved(value, actor.bytecode_module.as_ref())
            };
            state.insert(name.clone(), persisted);
        }
    }
    let authority_tokens = actor
        .authority_manifest()
        .map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid actor authority during checkpoint: {error}"),
            )
        })?
        .canonical_token_set();
    let crdt_snapshot = rt.crdt_manager.as_ref().map(|manager| {
        manager
            .snapshot()
            .into_iter()
            .map(|(id, (ty, bytes))| (id.0, ty.to_u8(), bytes))
            .collect()
    });
    let crdt_field_map = rt.crdt_manager.as_ref().map(|manager| {
        manager
            .field_map
            .iter()
            .filter(|((aid, _), _)| *aid == actor_id)
            .map(|((_, name), id)| (name.clone(), id.0))
            .collect()
    });
    Ok(Some((
        crate::runtime::persistence::ActorSnapshot {
            actor_id,
            sequence: seq,
            state,
            waiting_signal: actor.waiting_signal.clone(),
            crdt_snapshot,
            crdt_field_map,
            authority_tokens,
        },
        seq,
    )))
}

/// Fallible checkpoint used by initial durable publication. The local snapshot
/// commits before any shadow replica is emitted, so a failed local commit
/// cannot leave a remotely recoverable actor that was never published here.
pub(crate) fn try_checkpoint_actor(rt: &mut Runtime, actor_id: u64) -> std::io::Result<()> {
    let Some((snapshot, seq)) = build_actor_snapshot(rt, actor_id)? else {
        return Ok(());
    };
    rt.persistence.save_snapshot(snapshot.clone())?;
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = seq;
        actor.dirty_fields.clear();
    }
    Ok(())
}

/// Snapshot the durable and CRDT state of a persistent actor.
///
/// Existing runtime checkpoint sites remain best-effort for compatibility;
/// security-sensitive initial publication uses [`try_checkpoint_actor`].
pub(crate) fn checkpoint_actor(rt: &mut Runtime, actor_id: u64) {
    let Some((snapshot, seq)) = (match build_actor_snapshot(rt, actor_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::warn!(actor_id, %error, "refusing invalid actor checkpoint");
            return;
        }
    }) else {
        return;
    };
    // RFC 0014 compatibility: ordinary checkpoints retain the existing
    // shadow-before-local ordering. Initial publication uses the fallible path
    // above and therefore cannot leak a shadow before local durability.
    rt.maybe_shadow_replicate(actor_id, &snapshot);
    if let Err(error) = rt.persistence.save_snapshot(snapshot) {
        tracing::warn!(actor_id, %error, "failed to save actor checkpoint");
    }
    if let Some(actor) = rt.actors.get_mut(&actor_id) {
        actor.sequence = seq;
        actor.dirty_fields.clear();
    }
}

'''
path.write_text(text[:start] + replacement + text[end:])


# crdt_manager.rs: rollback primitive for failed pre-publication actors.
replace_once(
    "src/runtime/crdt_manager.rs",
    "    /// Register a CRDT-backed state field for an actor.\n",
    '''    /// Remove every CRDT registration owned by one actor.
    ///
    /// Used when actor construction fails before publication. This removes the
    /// forward/reverse mappings, local replicas, and delta-sync bases so a
    /// failed spawn cannot leave externally synchronizable CRDT residue.
    pub fn unregister_actor_fields(&mut self, actor_id: u64) {
        let owned: Vec<(String, CrdtId)> = self
            .field_map
            .iter()
            .filter_map(|((aid, name), id)| {
                (*aid == actor_id).then(|| (name.clone(), *id))
            })
            .collect();
        for (field_name, id) in owned {
            self.field_map.remove(&(actor_id, field_name));
            self.field_reverse.remove(&id);
            self.entries.remove(&id);
            self.sync_base.remove(&id);
        }
    }

    /// Register a CRDT-backed state field for an actor.
''',
    "CRDT registration anchor drifted",
)


# spawn.rs: typed privileged-spawn errors + fallible initial workflow commit.
replace_once(
    "src/runtime/spawn.rs",
    "use std::collections::HashMap;\n",
    "use std::collections::HashMap;\nuse std::fmt;\nuse std::io;\n",
    "spawn imports drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    "use crate::vm::Value;\n\n",
    '''use crate::vm::Value;

#[derive(Debug)]
pub(crate) enum SpawnWithAuthorityError {
    Authority(RuntimeAuthorityError),
    Persistence(io::Error),
}

impl fmt::Display for SpawnWithAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authority(error) => write!(f, "{error}"),
            Self::Persistence(error) => write!(f, "durable spawn persistence failed: {error}"),
        }
    }
}

impl std::error::Error for SpawnWithAuthorityError {}

impl From<RuntimeAuthorityError> for SpawnWithAuthorityError {
    fn from(error: RuntimeAuthorityError) -> Self {
        Self::Authority(error)
    }
}

impl From<io::Error> for SpawnWithAuthorityError {
    fn from(error: io::Error) -> Self {
        Self::Persistence(error)
    }
}

''',
    "spawn error anchor drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''pub(crate) fn spawn_actor_with_models(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    spawn_actor_with_models_with_authority(rt, init, state_models, persistent, workflow, None)
}

fn spawn_actor_with_models_with_authority(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
    initial_authority: Option<&AuthorityManifest>,
) -> u64 {
    spawn_actor_with_id_with_authority(
        rt,
        fresh_actor_id(),
        init,
        state_models,
        persistent,
        workflow,
        initial_authority,
    )
}
''',
    '''pub(crate) fn spawn_actor_with_models(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    let id = fresh_actor_id();
    match spawn_actor_with_id_with_authority(
        rt, id, init, state_models, persistent, workflow, None,
    ) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(actor_id = id, %error, "actor spawn persistence failed");
            id
        }
    }
}

fn spawn_actor_with_models_with_authority(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
    initial_authority: Option<&AuthorityManifest>,
) -> io::Result<u64> {
    spawn_actor_with_id_with_authority(
        rt,
        fresh_actor_id(),
        init,
        state_models,
        persistent,
        workflow,
        initial_authority,
    )
}
''',
    "spawn_actor_with_models block drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''pub(crate) fn spawn_actor_with_id(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    spawn_actor_with_id_with_authority(rt, id, init, state_models, persistent, workflow, None)
}

fn spawn_actor_with_id_with_authority(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
    initial_authority: Option<&AuthorityManifest>,
) -> u64 {
''',
    '''pub(crate) fn spawn_actor_with_id(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    match spawn_actor_with_id_with_authority(
        rt, id, init, state_models, persistent, workflow, None,
    ) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(actor_id = id, %error, "actor spawn persistence failed");
            id
        }
    }
}

fn spawn_actor_with_id_with_authority(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
    initial_authority: Option<&AuthorityManifest>,
) -> io::Result<u64> {
''',
    "spawn_actor_with_id block drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    "                return id;\n",
    "                return Ok(id);\n",
    "preflight invalid authority return drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''        let _ = rt.persistence.append_workflow_event(
            id,
            WorkflowEvent::WorkflowStarted {
                sequence: seq,
                name: workflow_name.as_ref().unwrap().clone(),
                state,
            },
        );
        crate::runtime::workflow::checkpoint_actor(rt, id);
    }
    rt.enqueue_actor(id);
    id
}
''',
    '''        let initial_commit = (|| -> io::Result<()> {
            rt.persistence.append_workflow_event(
                id,
                WorkflowEvent::WorkflowStarted {
                    sequence: seq,
                    name: workflow_name.as_ref().unwrap().clone(),
                    state,
                },
            )?;
            crate::runtime::workflow::try_checkpoint_actor(rt, id)?;
            Ok(())
        })();
        if let Err(error) = initial_commit {
            rollback_failed_initial_workflow_spawn(rt, id);
            return Err(error);
        }
    }
    rt.enqueue_actor(id);
    Ok(id)
}

fn rollback_failed_initial_workflow_spawn(rt: &mut Runtime, actor_id: u64) {
    rt.actors.remove(&actor_id);
    if let Some(manager) = rt.crdt_manager.as_mut() {
        manager.unregister_actor_fields(actor_id);
    }
    if let Err(error) = rt.persistence.clear(actor_id) {
        tracing::warn!(
            actor_id,
            %error,
            "failed to clear partial durable state after rejected workflow spawn"
        );
    }
}
''',
    "initial workflow persistence block drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''pub(crate) fn spawn_from_module(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
) -> Value {
    spawn_from_module_with_initial_authority(rt, module, behavior_idx, init, None)
}

fn spawn_from_module_with_initial_authority(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
    initial_authority: Option<&AuthorityManifest>,
) -> Value {
''',
    '''pub(crate) fn spawn_from_module(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
) -> Value {
    match spawn_from_module_with_initial_authority(rt, module, behavior_idx, init, None) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "actor spawn persistence failed");
            Value::nil()
        }
    }
}

fn spawn_from_module_with_initial_authority(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
    initial_authority: Option<&AuthorityManifest>,
) -> io::Result<Value> {
''',
    "spawn_from_module signature drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    "                return Value::nil();\n",
    "                return Ok(Value::nil());\n",
    "conflicting role return drifted",
)
text = Path("src/runtime/spawn.rs").read_text()
marker = "            initial_authority,\n        )\n    } else {"
assert text.count(marker) == 1, "metadata authority call tail drifted"
text = text.replace(marker, "            initial_authority,\n        )?\n    } else {", 1)
marker = "            initial_authority,\n        )\n    };"
assert text.count(marker) == 1, "plain authority call tail drifted"
text = text.replace(marker, "            initial_authority,\n        )?\n    };", 1)
marker = "    Value::actor_ref(id)\n}\n\n/// Spawn from a bytecode module while enforcing"
assert text.count(marker) == 1, "spawn_from_module return drifted"
text = text.replace(
    marker,
    "    Ok(Value::actor_ref(id))\n}\n\n/// Spawn from a bytecode module while enforcing",
    1,
)
Path("src/runtime/spawn.rs").write_text(text)
replace_once(
    "src/runtime/spawn.rs",
    ") -> Result<Value, RuntimeAuthorityError> {\n",
    ") -> Result<Value, SpawnWithAuthorityError> {\n",
    "privileged spawn result drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    "            return Err(RuntimeAuthorityError::Denied(grant.clone()));\n",
    "            return Err(RuntimeAuthorityError::Denied(grant.clone()).into());\n",
    "missing parent denial drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''    Ok(spawn_from_module_with_initial_authority(
        rt,
        module,
        behavior_idx,
        init,
        Some(requested),
    ))
''',
    '''    Ok(spawn_from_module_with_initial_authority(
        rt,
        module,
        behavior_idx,
        init,
        Some(requested),
    )?)
''',
    "privileged spawn propagation drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''        assert_eq!(
            result,
            Err(RuntimeAuthorityError::Denied(AuthorityGrant::SecretRead {
                name: "STRIPE_KEY".into(),
            }))
        );
''',
    '''        assert!(matches!(
            result,
            Err(SpawnWithAuthorityError::Authority(RuntimeAuthorityError::Denied(
                AuthorityGrant::SecretRead { ref name }
            ))) if name == "STRIPE_KEY"
        ));
''',
    "denied authority assertion drifted",
)
replace_once(
    "src/runtime/spawn.rs",
    '''        assert!(matches!(
            result,
            Err(RuntimeAuthorityError::InvalidManifest(_))
        ));
''',
    '''        assert!(matches!(
            result,
            Err(SpawnWithAuthorityError::Authority(
                RuntimeAuthorityError::InvalidManifest(_)
            ))
        ));
''',
    "invalid manifest assertion drifted",
)

# Real failure injection: this delegates every successful operation to
# MemoryStore and fails exactly the initial journal or snapshot boundary.
replace_once(
    "src/runtime/spawn.rs",
    "    use crate::bytecode::CodeModule;\n\n    fn secret_manifest",
    '''    use crate::bytecode::CodeModule;
    use crate::runtime::persistence::{
        EventEntry, JournalEntry, MemoryStore, PersistenceStore,
    };

    struct FailingStore {
        inner: MemoryStore,
        fail_workflow_start: bool,
        fail_snapshot: bool,
    }

    impl FailingStore {
        fn new(fail_workflow_start: bool, fail_snapshot: bool) -> Self {
            Self {
                inner: MemoryStore::new(),
                fail_workflow_start,
                fail_snapshot,
            }
        }
    }

    impl PersistenceStore for FailingStore {
        fn save_snapshot(&mut self, snapshot: ActorSnapshot) -> io::Result<()> {
            if self.fail_snapshot {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected snapshot failure",
                ));
            }
            self.inner.save_snapshot(snapshot)
        }

        fn load_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {
            self.inner.load_snapshot(actor_id)
        }

        fn append_journal(&mut self, actor_id: u64, entry: JournalEntry) -> io::Result<()> {
            self.inner.append_journal(actor_id, entry)
        }

        fn read_journal(&self, actor_id: u64) -> Vec<JournalEntry> {
            self.inner.read_journal(actor_id)
        }

        fn append_workflow_event(
            &mut self,
            actor_id: u64,
            event: WorkflowEvent,
        ) -> io::Result<()> {
            if self.fail_workflow_start
                && matches!(event, WorkflowEvent::WorkflowStarted { .. })
            {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected workflow-start failure",
                ));
            }
            self.inner.append_workflow_event(actor_id, event)
        }

        fn read_workflow_events(&self, actor_id: u64) -> Vec<WorkflowEvent> {
            self.inner.read_workflow_events(actor_id)
        }

        fn append_event(&mut self, actor_id: u64, entry: EventEntry) -> io::Result<()> {
            self.inner.append_event(actor_id, entry)
        }

        fn read_events(&self, actor_id: u64) -> Vec<EventEntry> {
            self.inner.read_events(actor_id)
        }

        fn latest_sequence(&self, actor_id: u64) -> u64 {
            self.inner.latest_sequence(actor_id)
        }

        fn clear(&mut self, actor_id: u64) -> io::Result<()> {
            self.inner.clear(actor_id)
        }
    }

    fn secret_manifest''',
    "authority test imports drifted",
)
insert_marker = "    #[test]\n    fn workflow_spawn_authority_snapshot_survives_recovery() {"
tests = '''    #[test]
    fn workflow_start_append_failure_rolls_back_before_publication() {
        let mut rt = Runtime::new();
        rt.persistence = Box::new(FailingStore::new(true, false));
        rt.crdt_manager = Some(crate::runtime::crdt_manager::CrdtManager::new(1));
        let actor_id = 910_101;
        let requested = secret_manifest("WORKFLOW_KEY");
        let state_models = HashMap::from([(
            "counter".to_string(),
            StateModel::Crdt(crate::ast::CrdtType::GCounter),
        )]);

        let result = spawn_actor_with_id_with_authority(
            &mut rt,
            actor_id,
            Box::new(|| vec![("counter".to_string(), Value::int(1))]),
            state_models,
            true,
            Some("append-fails"),
            Some(&requested),
        );

        assert!(result.is_err());
        assert!(!rt.actors.contains_key(&actor_id));
        let manager = rt.crdt_manager.as_ref().unwrap();
        assert!(manager.get_field_id(actor_id, "counter").is_none());
        assert!(rt.persistence.load_snapshot(actor_id).is_none());
        assert!(rt.persistence.read_workflow_events(actor_id).is_empty());
        assert_eq!(rt.recover_actor(actor_id), None);
    }

    #[test]
    fn initial_snapshot_failure_rolls_back_journal_and_crdt_state() {
        let mut rt = Runtime::new();
        rt.persistence = Box::new(FailingStore::new(false, true));
        rt.crdt_manager = Some(crate::runtime::crdt_manager::CrdtManager::new(1));
        let actor_id = 910_102;
        let requested = secret_manifest("WORKFLOW_KEY");
        let state_models = HashMap::from([(
            "counter".to_string(),
            StateModel::Crdt(crate::ast::CrdtType::GCounter),
        )]);

        let result = spawn_actor_with_id_with_authority(
            &mut rt,
            actor_id,
            Box::new(|| vec![("counter".to_string(), Value::int(1))]),
            state_models,
            true,
            Some("snapshot-fails"),
            Some(&requested),
        );

        assert!(result.is_err());
        assert!(!rt.actors.contains_key(&actor_id));
        let manager = rt.crdt_manager.as_ref().unwrap();
        assert!(manager.get_field_id(actor_id, "counter").is_none());
        assert!(rt.persistence.load_snapshot(actor_id).is_none());
        assert!(rt.persistence.read_workflow_events(actor_id).is_empty());
        assert_eq!(rt.persistence.latest_sequence(actor_id), 0);
        assert_eq!(rt.recover_actor(actor_id), None);
    }

'''
text = Path("src/runtime/spawn.rs").read_text()
assert text.count(insert_marker) == 1, "durability test insertion anchor drifted"
Path("src/runtime/spawn.rs").write_text(text.replace(insert_marker, tests + insert_marker, 1))


# callbacks.rs: persistence rejection must not be mislabeled as authority denial.
replace_once(
    "src/runtime/callbacks.rs",
    '''        Err(error) => {
            tracing::warn!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing actor spawn whose authority is not delegated by the parent"
            );
            crate::vm::Value::nil()
        }
''',
    '''        Err(super::spawn::SpawnWithAuthorityError::Authority(error)) => {
            tracing::warn!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing actor spawn whose authority is not delegated by the parent"
            );
            crate::vm::Value::nil()
        }
        Err(super::spawn::SpawnWithAuthorityError::Persistence(error)) => {
            tracing::error!(
                spawn_pc,
                behavior_idx,
                %error,
                "refusing durable actor spawn because initial persistence failed"
            );
            crate::vm::Value::nil()
        }
''',
    "callback spawn error branch drifted",
)
