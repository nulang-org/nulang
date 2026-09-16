#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def patch_actor() -> None:
    path = Path("src/runtime/actor.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''    /// Bytecode behavior offsets by behavior_id. Empty entries mean no bytecode
    /// handler for that behavior (native handler or missing).
    pub bytecode_offsets: Vec<usize>,''',
        '''    /// Bytecode behavior offsets by behavior_id. Empty entries mean no bytecode
    /// handler for that behavior (native handler or missing).
    pub bytecode_offsets: Vec<usize>,
    /// Nominal ActorMeta owner for this actor's bytecode behavior table.
    /// Backend-local behavior ids are valid only inside this schema.
    pub bytecode_schema_name: Option<String>,''',
        "actor schema field",
    )
    text = replace_once(
        text,
        '''            bytecode_offsets: Vec::new(),
            compensation_offsets: Vec::new(),''',
        '''            bytecode_offsets: Vec::new(),
            bytecode_schema_name: None,
            compensation_offsets: Vec::new(),''',
        "actor schema init",
    )
    path.write_text(text)


def patch_persistence() -> None:
    path = Path("src/runtime/persistence.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''    pub waiting_signal: Option<String>,
    /// CRDT state belonging to the runtime's CrdtManager, serialized as''',
        '''    pub waiting_signal: Option<String>,
    /// Nominal ActorMeta owner for bytecode dispatch. Missing on legacy
    /// snapshots; ambiguous legacy modules then fail closed at dispatch.
    #[serde(default)]
    pub bytecode_schema_name: Option<String>,
    /// CRDT state belonging to the runtime's CrdtManager, serialized as''',
        "snapshot schema field",
    )
    path.write_text(text)


def patch_workflow() -> None:
    path = Path("src/runtime/workflow.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''        state,
        waiting_signal: actor.waiting_signal.clone(),
        crdt_snapshot,''',
        '''        state,
        waiting_signal: actor.waiting_signal.clone(),
        bytecode_schema_name: actor.bytecode_schema_name.clone(),
        crdt_snapshot,''',
        "checkpoint schema",
    )
    path.write_text(text)


def patch_persist_bench() -> None:
    path = Path("benches/persist_bench.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''                waiting_signal: None,
                crdt_snapshot: None,''',
        '''                waiting_signal: None,
                bytecode_schema_name: None,
                crdt_snapshot: None,''',
        "persist bench schema",
    )
    path.write_text(text)


def patch_spawn() -> None:
    path = Path("src/runtime/spawn.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''        actor.bytecode_module = Some(module.clone());
        actor.bytecode_offsets = offsets.clone();
        actor.compensation_offsets = compensation_offsets.clone();
        if let Some(meta) = meta {''',
        '''        actor.bytecode_module = Some(module.clone());
        actor.bytecode_offsets = offsets.clone();
        actor.bytecode_schema_name = meta.map(|meta| meta.name.clone());
        actor.compensation_offsets = compensation_offsets.clone();
        if let Some(meta) = meta {''',
        "spawn schema owner",
    )
    path.write_text(text)


def patch_supervisor() -> None:
    path = Path("src/runtime/supervisor.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''    /// Bytecode behavior offsets by behavior id.
    pub bytecode_offsets: Vec<usize>,
    /// Saga compensation offsets by behavior id.''',
        '''    /// Bytecode behavior offsets by behavior id.
    pub bytecode_offsets: Vec<usize>,
    /// Nominal ActorMeta owner for bytecode dispatch.
    pub bytecode_schema_name: Option<String>,
    /// Saga compensation offsets by behavior id.''',
        "restart template schema field",
    )
    text = replace_once(
        text,
        '''        new_actor.bytecode_offsets = template.bytecode_offsets.clone();
        new_actor.compensation_offsets = template.compensation_offsets.clone();''',
        '''        new_actor.bytecode_offsets = template.bytecode_offsets.clone();
        new_actor.bytecode_schema_name = template.bytecode_schema_name.clone();
        if let Some(snap) = &snapshot {
            if snap.bytecode_schema_name.is_some() {
                new_actor.bytecode_schema_name = snap.bytecode_schema_name.clone();
            }
        }
        new_actor.compensation_offsets = template.compensation_offsets.clone();''',
        "supervisor restore schema",
    )
    path.write_text(text)


def patch_runtime() -> None:
    path = Path("src/runtime/mod.rs")
    text = path.read_text()

    text = replace_once(
        text,
        '''            bytecode_module: actor.bytecode_module.clone(),
            bytecode_offsets: actor.bytecode_offsets.clone(),
            compensation_offsets: actor.compensation_offsets.clone(),''',
        '''            bytecode_module: actor.bytecode_module.clone(),
            bytecode_offsets: actor.bytecode_offsets.clone(),
            bytecode_schema_name: actor.bytecode_schema_name.clone(),
            compensation_offsets: actor.compensation_offsets.clone(),''',
        "supervise child schema capture",
    )

    old_behavior_block = '''    pub fn behavior_id_for(&self, target_id: u64, behavior: &str) -> Option<u16> {
        let actor = self.actors.get(&target_id)?;
        // Allocation-free match: `entry.name == behavior`, or
        // `entry.name` ends with `.<behavior>` (qualified name).
        let matches = |name: &str| {
            name == behavior
                || name
                    .strip_suffix(behavior)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        };
        // Search the per-actor behavior table first (native handlers).
        if let Some(idx) = actor
            .behavior_table
            .iter()
            .position(|entry| matches(&entry.name))
        {
            return Some(idx as u16);
        }
        // Fall back to the module-level behavior table (bytecode handlers).
        // Returns the GLOBAL index into module.behaviors, which matches
        // what bytecode_offsets expects.
        let module = actor.bytecode_module.as_ref()?;
        module
            .behaviors
            .iter()
            .position(|b| matches(&b.name))
            .map(|idx| idx as u16)
    }

    /// Resolve a behavior name to a numeric id using the registered grain
    /// type's module. This lets `send_to_grain` route across shards before the
    /// target actor has been hydrated on the local shard.
    fn resolve_grain_behavior_id(&self, grain_id: &GrainId, behavior_name: &str) -> Option<u16> {
        let grain_type = self.grain_registry.get(&grain_id.grain_type)?;
        let suffix = format!(".{}", behavior_name);
        grain_type
            .module
            .behaviors
            .iter()
            .position(|b| b.name == behavior_name || b.name.ends_with(&suffix))
            .map(|idx| idx as u16)
    }
'''
    new_behavior_block = '''    fn module_meta_for_schema<'a>(
        module: &'a crate::bytecode::CodeModule,
        schema_name: Option<&str>,
    ) -> Option<&'a crate::bytecode::ActorMeta> {
        if let Some(schema_name) = schema_name {
            return module.actor_metadata.iter().find(|meta| meta.name == schema_name);
        }
        if module.actor_metadata.len() == 1 {
            return module.actor_metadata.first();
        }
        None
    }

    fn bytecode_meta_for_actor(
        actor: &Actor,
    ) -> Option<&crate::bytecode::ActorMeta> {
        let module = actor.bytecode_module.as_ref()?;
        let schema_name = actor.bytecode_schema_name.as_deref().or_else(|| {
            module
                .actor_metadata
                .iter()
                .any(|meta| meta.name == actor.name)
                .then_some(actor.name.as_str())
        });
        Self::module_meta_for_schema(module, schema_name)
    }

    /// Map an actor-local dispatch id to its global CodeModule behavior index,
    /// proving that the id belongs to the target actor's nominal ActorMeta.
    /// Plain actors use global ids directly; workflows use compact local ids.
    pub(crate) fn bytecode_global_behavior_idx(
        &self,
        actor_id: u64,
        behavior_idx: usize,
    ) -> Option<usize> {
        let actor = self.actors.get(&actor_id)?;
        let meta = Self::bytecode_meta_for_actor(actor)?;
        if actor.is_workflow {
            meta.behavior_indices.get(behavior_idx).copied()
        } else if meta.behavior_indices.contains(&behavior_idx) {
            Some(behavior_idx)
        } else {
            None
        }
    }

    pub fn behavior_id_for(&self, target_id: u64, behavior: &str) -> Option<u16> {
        let actor = self.actors.get(&target_id)?;
        // Native handlers are already actor-local, so a short name cannot
        // cross into another actor's table.
        if let Some(idx) = actor
            .behavior_table
            .iter()
            .position(|entry| entry.name == behavior)
        {
            return Some(idx as u16);
        }

        let module = actor.bytecode_module.as_ref()?;
        let meta = Self::bytecode_meta_for_actor(actor)?;
        let qualified = format!("{}.{}", meta.name, behavior);
        for (local_idx, global_idx) in meta.behavior_indices.iter().copied().enumerate() {
            let entry = module.behaviors.get(global_idx)?;
            if entry.name == behavior || entry.name == qualified {
                let dispatch_idx = if actor.is_workflow {
                    local_idx
                } else {
                    global_idx
                };
                return u16::try_from(dispatch_idx).ok();
            }
        }
        None
    }

    /// Resolve a behavior name to a numeric id using only the target grain
    /// type's ActorMeta. Before hydration grain ids use ordinary global
    /// behavior indices because grains are plain bytecode actors.
    fn resolve_grain_behavior_id(&self, grain_id: &GrainId, behavior_name: &str) -> Option<u16> {
        let grain_type = self.grain_registry.get(&grain_id.grain_type)?;
        let meta = grain_type
            .module
            .actor_metadata
            .iter()
            .find(|meta| meta.name == grain_id.grain_type)?;
        let qualified = format!("{}.{}", meta.name, behavior_name);
        meta.behavior_indices.iter().copied().find_map(|global_idx| {
            grain_type
                .module
                .behaviors
                .get(global_idx)
                .filter(|entry| entry.name == behavior_name || entry.name == qualified)
                .and_then(|_| u16::try_from(global_idx).ok())
        })
    }
'''
    text = replace_once(text, old_behavior_block, new_behavior_block, "target-scoped behavior resolution")

    text = replace_once(
        text,
        '''    fn has_bytecode_handler(&self, actor_id: u64, behavior_idx: usize) -> bool {
        self.actors
            .get(&actor_id)
            .map(|a| a.bytecode_module.is_some() && behavior_idx < a.bytecode_offsets.len())
            .unwrap_or(false)
    }''',
        '''    fn has_bytecode_handler(&self, actor_id: u64, behavior_idx: usize) -> bool {
        let Some(actor) = self.actors.get(&actor_id) else {
            return false;
        };
        let Some(module) = actor.bytecode_module.as_ref() else {
            return false;
        };
        let Some(global_idx) = self.bytecode_global_behavior_idx(actor_id, behavior_idx) else {
            return false;
        };
        if module.behaviors.get(global_idx).is_none() {
            return false;
        }
        let offset_idx = if actor.is_workflow {
            behavior_idx
        } else {
            global_idx
        };
        actor.bytecode_offsets.get(offset_idx).is_some()
    }''',
        "owned bytecode handler predicate",
    )

    text = replace_once(
        text,
        '''        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal;
        actor.install_authority_manifest(&authority_manifest);''',
        '''        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal;
        actor.bytecode_schema_name = snapshot.bytecode_schema_name.clone();
        actor.install_authority_manifest(&authority_manifest);''',
        "recover snapshot schema",
    )

    text = replace_once(
        text,
        '''            actor.bytecode_module = Some(module.clone());
            actor.bytecode_offsets = offsets.clone();
            actor.compensation_offsets = comp_offsets.clone();''',
        '''            actor.bytecode_module = Some(module.clone());
            actor.bytecode_offsets = offsets.clone();
            if actor.bytecode_schema_name.is_none() && module.actor_metadata.len() == 1 {
                actor.bytecode_schema_name = module.actor_metadata.first().map(|meta| meta.name.clone());
            }
            actor.compensation_offsets = comp_offsets.clone();''',
        "recover legacy unique schema",
    )

    text = replace_once(
        text,
        '''        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal.clone();
        actor.install_authority_manifest(&authority_manifest);
        actor.bytecode_module = Some(module.clone());''',
        '''        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal.clone();
        actor.bytecode_schema_name = snapshot.bytecode_schema_name.clone();
        if actor.bytecode_schema_name.is_none() && module.actor_metadata.len() == 1 {
            actor.bytecode_schema_name = module.actor_metadata.first().map(|meta| meta.name.clone());
        }
        actor.install_authority_manifest(&authority_manifest);
        actor.bytecode_module = Some(module.clone());''',
        "restore migrated schema",
    )

    text = replace_once(
        text,
        '''            actor.persistent = true;
            actor.bytecode_module = Some(grain_type.module.clone());
            actor.bytecode_offsets = grain_type.bytecode_offsets.clone();''',
        '''            actor.persistent = true;
            actor.bytecode_schema_name = Some(grain_id.grain_type.clone());
            actor.bytecode_module = Some(grain_type.module.clone());
            actor.bytecode_offsets = grain_type.bytecode_offsets.clone();''',
        "fresh grain schema",
    )

    path.write_text(text)


def patch_distributed() -> None:
    path = Path("src/runtime/distributed.rs")
    text = path.read_text()
    text = replace_once(
        text,
        '''    let module = match &actor.bytecode_module {
        Some(m) => m,
        None => return false,
    };
    let entry = match module.behaviors.get(behavior_id as usize) {
        Some(e) => e,
        None => return false,
    };''',
        '''    let module = match &actor.bytecode_module {
        Some(m) => m,
        None => return false,
    };
    let Some(global_idx) = runtime.bytecode_global_behavior_idx(target_actor, behavior_id as usize)
    else {
        return false;
    };
    let entry = match module.behaviors.get(global_idx) {
        Some(e) => e,
        None => return false,
    };''',
        "distributed hash ownership",
    )
    path.write_text(text)


def patch_tests() -> None:
    path = Path("src/runtime/tests.rs")
    text = path.read_text()
    marker = "// ========================================================================\n// Core Runtime Tests\n// ========================================================================\n"
    if marker not in text:
        raise SystemExit("runtime core-test marker not found")
    tests = r'''fn compile_actor_ownership_fixture() -> CodeModule {
    let source = r#"
actor First {
    behavior hit(): Int { 11 }
}
actor Second {
    behavior hit(): Int { 22 }
}
"#;
    let tokens = crate::lexer::Lexer::new(source).lex().expect("lex");
    let ast = crate::parser::Parser::new(tokens).parse_module().expect("parse");
    let mut tc = crate::typechecker::TypeChecker::new();
    tc.check_module(&ast).expect("typecheck");
    let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
    let mut mir = crate::mir_lower::lower_module(&hir).expect("MIR lower");
    crate::mir_codegen::compile_mir(&mut mir, "ownership-fixture").expect("bytecode")
}

#[test]
fn bytecode_dispatch_rejects_other_actor_schema_slot() {
    let module = compile_actor_ownership_fixture();
    let first_idx = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "First")
        .and_then(|meta| meta.behavior_indices.first().copied())
        .expect("First.hit");
    let second_idx = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Second")
        .and_then(|meta| meta.behavior_indices.first().copied())
        .expect("Second.hit");
    assert_ne!(first_idx, second_idx);

    let mut rt = Runtime::new();
    let second = rt
        .spawn_from_module(&module, second_idx, vec![])
        .as_actor_id()
        .expect("spawn Second");
    assert_eq!(
        rt.actors.get(&second).unwrap().bytecode_schema_name.as_deref(),
        Some("Second")
    );
    assert!(!rt.has_bytecode_handler(second, first_idx));
    assert!(rt.has_bytecode_handler(second, second_idx));
    assert_eq!(rt.behavior_id_for(second, "hit"), Some(second_idx as u16));
    assert!(rt.ask_actor_sync(second, first_idx as u16, &[]).is_err());
}

#[test]
fn actor_schema_identity_survives_snapshot_json_and_migration() {
    let module = compile_actor_ownership_fixture();
    let second_idx = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Second")
        .and_then(|meta| meta.behavior_indices.first().copied())
        .expect("Second.hit");
    let snapshot = ActorSnapshot {
        actor_id: 91_050,
        bytecode_schema_name: Some("Second".to_string()),
        ..ActorSnapshot::default()
    };
    let json = serde_json::to_vec(&snapshot).unwrap();
    let decoded: ActorSnapshot = serde_json::from_slice(&json).unwrap();
    assert_eq!(decoded.bytecode_schema_name.as_deref(), Some("Second"));

    let nbc = module.to_nbc(None).unwrap();
    let mut rt = Runtime::new();
    assert!(rt.receive_migrated_actor(snapshot.actor_id, nbc, json));
    let actor = rt.actors.get(&snapshot.actor_id).unwrap();
    assert_eq!(actor.bytecode_schema_name.as_deref(), Some("Second"));
    assert_eq!(rt.behavior_id_for(snapshot.actor_id, "hit"), Some(second_idx as u16));
}

#[test]
fn ambiguous_legacy_snapshot_has_no_cross_schema_bytecode_dispatch() {
    let module = compile_actor_ownership_fixture();
    let second_idx = module
        .actor_metadata
        .iter()
        .find(|meta| meta.name == "Second")
        .and_then(|meta| meta.behavior_indices.first().copied())
        .expect("Second.hit");
    let mut actor = Actor::new(91_051, "legacy", 0);
    actor.bytecode_module = Some(module.clone());
    actor.bytecode_offsets = module.behaviors.iter().map(|b| b.code_offset).collect();
    // No bytecode_schema_name: a legacy snapshot/module with multiple actor
    // schemas is ambiguous and must fail closed rather than suffix-match.
    let mut rt = Runtime::new();
    rt.actors.insert(actor.id, actor);
    assert!(!rt.has_bytecode_handler(91_051, second_idx));
    assert_eq!(rt.behavior_id_for(91_051, "hit"), None);
}

'''
    text = text.replace(marker, tests + marker, 1)
    path.write_text(text)


def main() -> None:
    patch_actor()
    patch_persistence()
    patch_workflow()
    patch_persist_bench()
    patch_spawn()
    patch_supervisor()
    patch_runtime()
    patch_distributed()
    patch_tests()


if __name__ == "__main__":
    main()
