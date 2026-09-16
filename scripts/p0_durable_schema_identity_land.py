from pathlib import Path
import re


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one exact match, found {count}")
    p.write_text(text.replace(old, new, 1))


def transform_region(path: str, start_marker: str, end_marker: str, transform, label: str) -> None:
    p = Path(path)
    text = p.read_text()
    start = text.index(start_marker)
    end = text.index(end_marker, start)
    old = text[start:end]
    new = transform(old)
    if new == old:
        raise SystemExit(f"{label}: transform made no change")
    p.write_text(text[:start] + new + text[end:])


def ensure_snapshot_literal_schema(path: Path) -> None:
    text = path.read_text()
    pos = 0
    changed = False
    while True:
        idx = text.find("ActorSnapshot {", pos)
        if idx < 0:
            break
        brace = text.find("{", idx)
        depth = 0
        end = None
        for i in range(brace, len(text)):
            c = text[i]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    end = i + 1
                    break
        if end is None:
            raise SystemExit(f"{path}: unterminated ActorSnapshot literal")
        block = text[idx:end]
        if "schema_name:" not in block and "authority_tokens" in block:
            updated, n = re.subn(
                r"(?m)^(\s*)authority_tokens([,:])",
                r"\1schema_name: None,\n\1authority_tokens\2",
                block,
                count=1,
            )
            if n != 1:
                raise SystemExit(f"{path}: could not add schema_name to ActorSnapshot literal")
            text = text[:idx] + updated + text[end:]
            end = idx + len(updated)
            changed = True
        pos = end
    if changed:
        path.write_text(text)


# Backward-compatible durable format extension.
replace_once(
    "src/runtime/persistence.rs",
    "    /// Canonical external-authority tokens held by the actor at the time\n",
    "    /// Canonical declared actor schema (`ActorMeta.name`) represented by\n"
    "    /// this snapshot. Missing on legacy snapshots is accepted only when the\n"
    "    /// loaded module contains exactly one unambiguous actor schema.\n"
    "    #[serde(default)]\n"
    "    pub schema_name: Option<String>,\n"
    "    /// Canonical external-authority tokens held by the actor at the time\n",
    "ActorSnapshot schema field",
)

# Keep all existing literal construction sites compiling first. Runtime
# writers below are then upgraded from None to canonical schema proof.
for root in (Path("src"), Path("tests"), Path("benches")):
    if root.exists():
        for path in root.rglob("*.rs"):
            ensure_snapshot_literal_schema(path)

# Canonical schema is stamped on ordinary/workflow checkpoints.
replace_once(
    "src/runtime/workflow.rs",
    "    let snapshot = crate::runtime::persistence::ActorSnapshot {\n",
    "    let schema_name = actor\n"
    "        .bytecode_module\n"
    "        .as_ref()\n"
    "        .and_then(|module| {\n"
    "            crate::runtime_schema_identity::canonical_schema_name_for_runtime_actor(\n"
    "                module,\n"
    "                &actor.name,\n"
    "            )\n"
    "        })\n"
    "        .map(str::to_owned);\n"
    "    let snapshot = crate::runtime::persistence::ActorSnapshot {\n",
    "workflow snapshot schema capture",
)
replace_once(
    "src/runtime/workflow.rs",
    "        schema_name: None,\n        authority_tokens,\n",
    "        schema_name,\n        authority_tokens,\n",
    "workflow snapshot schema field",
)

# Migration packets must carry the same canonical schema proof.
replace_once(
    "src/runtime/callbacks.rs",
    "                let snapshot = crate::runtime::persistence::ActorSnapshot {\n",
    "                let schema_name = actor\n"
    "                    .bytecode_module\n"
    "                    .as_ref()\n"
    "                    .and_then(|module| {\n"
    "                        crate::runtime_schema_identity::canonical_schema_name_for_runtime_actor(\n"
    "                            module,\n"
    "                            &actor.name,\n"
    "                        )\n"
    "                    })\n"
    "                    .map(str::to_owned);\n"
    "                let snapshot = crate::runtime::persistence::ActorSnapshot {\n",
    "migration snapshot schema capture",
)
replace_once(
    "src/runtime/callbacks.rs",
    "                    schema_name: None,\n                    authority_tokens,\n",
    "                    schema_name,\n                    authority_tokens,\n",
    "migration snapshot schema field",
)

# Runtime-level snapshot construction (shadow replication, etc.).
replace_once(
    "src/runtime/mod.rs",
    "    fn actor_module_hash(&self, actor_id: u64) -> [u8; 32] {\n"
    "        self.actors\n"
    "            .get(&actor_id)\n"
    "            .and_then(|a| a.bytecode_module.as_ref())\n"
    "            .and_then(|m| m.actor_metadata.iter().find_map(|m| m.type_hash))\n"
    "            .unwrap_or([0u8; 32])\n"
    "    }\n",
    "    fn actor_module_hash(&self, actor_id: u64) -> [u8; 32] {\n"
    "        self.actors\n"
    "            .get(&actor_id)\n"
    "            .and_then(|actor| {\n"
    "                let module = actor.bytecode_module.as_ref()?;\n"
    "                crate::runtime_behavior_ownership::actor_meta_for_runtime_name(\n"
    "                    module,\n"
    "                    &actor.name,\n"
    "                )?\n"
    "                .type_hash\n"
    "            })\n"
    "            .unwrap_or([0u8; 32])\n"
    "    }\n",
    "schema-owned module hash",
)


def patch_build_snapshot(region: str) -> str:
    needle = "        Some(ActorSnapshot {\n"
    if region.count(needle) != 1:
        raise SystemExit("build_actor_snapshot: expected one ActorSnapshot literal")
    region = region.replace(
        needle,
        "        let schema_name = self.actors.get(&actor_id).and_then(|actor| {\n"
        "            let module = actor.bytecode_module.as_ref()?;\n"
        "            crate::runtime_schema_identity::canonical_schema_name_for_runtime_actor(\n"
        "                module,\n"
        "                &actor.name,\n"
        "            )\n"
        "            .map(str::to_owned)\n"
        "        });\n"
        "        Some(ActorSnapshot {\n",
        1,
    )
    if region.count("            schema_name: None,\n") != 1:
        raise SystemExit("build_actor_snapshot: expected one default schema field")
    return region.replace("            schema_name: None,\n", "            schema_name,\n", 1)


transform_region(
    "src/runtime/mod.rs",
    "    fn build_actor_snapshot(&self, actor_id: u64) -> Option<ActorSnapshot> {\n",
    "    /// Scan resident grain actors and hibernate any that have been idle long\n",
    patch_build_snapshot,
    "build_actor_snapshot schema capture",
)

# Recovery chooses one ActorMeta and never flattens metadata from the rest of
# a multi-actor module into the recovered actor.
def patch_recover(region: str) -> str:
    workflow_events = "        let workflow_events = self.persistence.read_workflow_events(actor_id);\n"
    if region.count(workflow_events) != 1:
        raise SystemExit("recover_actor: workflow event marker mismatch")
    region = region.replace(
        workflow_events,
        workflow_events
        + "        let recovery_schema_name = if let Some((module, _, _)) =\n"
        "            self.recovery_modules.get(&actor_id)\n"
        "        {\n"
        "            match crate::runtime_schema_identity::resolve_snapshot_actor_meta(\n"
        "                module,\n"
        "                snapshot.schema_name.as_deref(),\n"
        "            ) {\n"
        "                Ok(meta) => Some(meta.name.clone()),\n"
        "                Err(err) => {\n"
        "                    warn!(\n"
        "                        \"nulang-recover: refusing actor {} with invalid schema identity: {}\",\n"
        "                        actor_id, err\n"
        "                    );\n"
        "                    return None;\n"
        "                }\n"
        "            }\n"
        "        } else {\n"
        "            snapshot.schema_name.clone()\n"
        "        };\n",
        1,
    )
    old_role = (
        "        let is_workflow = self\n"
        "            .recovery_modules\n"
        "            .get(&actor_id)\n"
        "            .map(|(m, _, _)| m.actor_metadata.iter().any(|meta| meta.is_workflow))\n"
        "            .unwrap_or(!workflow_events.is_empty());\n"
        "        let is_agent = self\n"
        "            .recovery_modules\n"
        "            .get(&actor_id)\n"
        "            .map(|(m, _, _)| m.actor_metadata.iter().any(|meta| meta.is_agent))\n"
        "            .unwrap_or(false);\n\n"
        "        let mut actor = Actor::new(actor_id, format!(\"actor_{}\", actor_id), 0);\n"
    )
    new_role = (
        "        let selected_meta = self.recovery_modules.get(&actor_id).and_then(|(module, _, _)| {\n"
        "            recovery_schema_name.as_deref().and_then(|name| {\n"
        "                crate::runtime_behavior_ownership::actor_meta_for_schema(module, name)\n"
        "            })\n"
        "        });\n"
        "        let is_workflow = selected_meta\n"
        "            .map(|meta| meta.is_workflow)\n"
        "            .unwrap_or(!workflow_events.is_empty());\n"
        "        let is_agent = selected_meta.map(|meta| meta.is_agent).unwrap_or(false);\n\n"
        "        let mut actor = Actor::new(\n"
        "            actor_id,\n"
        "            recovery_schema_name\n"
        "                .clone()\n"
        "                .unwrap_or_else(|| format!(\"actor_{}\", actor_id)),\n"
        "            0,\n"
        "        );\n"
    )
    if region.count(old_role) != 1:
        raise SystemExit("recover_actor: legacy role/name block mismatch")
    region = region.replace(old_role, new_role, 1)

    old_defaults = "module.actor_metadata.iter().flat_map(|m| &m.state_defaults)"
    new_defaults = (
        "recovery_schema_name\n"
        "                    .as_deref()\n"
        "                    .and_then(|name| {\n"
        "                        crate::runtime_behavior_ownership::actor_meta_for_schema(module, name)\n"
        "                    })\n"
        "                    .into_iter()\n"
        "                    .flat_map(|meta| &meta.state_defaults)"
    )
    if region.count(old_defaults) != 2:
        raise SystemExit(
            f"recover_actor: expected two all-schema default traversals, found {region.count(old_defaults)}"
        )
    region = region.replace(old_defaults, new_defaults)

    old_models = (
        "            actor.state_models = module\n"
        "                .actor_metadata\n"
        "                .iter()\n"
        "                .flat_map(|m| &m.state_models)\n"
        "                .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))\n"
        "                .collect();\n"
    )
    new_models = (
        "            actor.state_models = recovery_schema_name\n"
        "                .as_deref()\n"
        "                .and_then(|name| {\n"
        "                    crate::runtime_behavior_ownership::actor_meta_for_schema(module, name)\n"
        "                })\n"
        "                .into_iter()\n"
        "                .flat_map(|meta| &meta.state_models)\n"
        "                .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))\n"
        "                .collect();\n"
    )
    if region.count(old_models) != 1:
        raise SystemExit("recover_actor: state-model traversal mismatch")
    return region.replace(old_models, new_models, 1)


transform_region(
    "src/runtime/mod.rs",
    "    pub fn recover_actor(&mut self, actor_id: u64) -> Option<u64> {\n",
    "    /// Build an actor from a snapshot and bytecode module - the common core\n",
    patch_recover,
    "recover_actor schema ownership",
)

# Common snapshot restore path used by migration and grains.
def replace_restore(_region: str) -> str:
    return '''    fn restore_actor_from_snapshot(
        actor_id: u64,
        module: &crate::bytecode::CodeModule,
        snapshot: &ActorSnapshot,
        expected_schema_name: Option<&str>,
        runtime_name: Option<String>,
    ) -> Result<Actor, String> {
        let meta = match expected_schema_name {
            Some(expected) => crate::runtime_schema_identity::resolve_expected_snapshot_actor_meta(
                module,
                snapshot.schema_name.as_deref(),
                expected,
            ),
            None => crate::runtime_schema_identity::resolve_snapshot_actor_meta(
                module,
                snapshot.schema_name.as_deref(),
            ),
        }
        .map_err(|err| err.to_string())?;
        let authority_manifest =
            crate::authority::AuthorityManifest::from_token_set(&snapshot.authority_tokens)
                .map_err(|err| err.to_string())?;
        let is_workflow = meta.is_workflow;
        let is_agent = meta.is_agent;
        let offsets: Vec<usize> = crate::runtime::spawn::bytecode_offsets_for(module, is_workflow);
        let compensation_offsets: Vec<Option<usize>> = if is_workflow {
            meta.behavior_indices
                .iter()
                .map(|&i| module.behaviors[i].compensate_offset.map(|o| o as usize))
                .collect()
        } else {
            module
                .behaviors
                .iter()
                .map(|b| b.compensate_offset.map(|o| o as usize))
                .collect()
        };

        let mut actor = Actor::new(
            actor_id,
            runtime_name.unwrap_or_else(|| meta.name.clone()),
            0,
        );
        actor.persistent = true;
        actor.is_workflow = is_workflow;
        actor.is_agent = is_agent;
        actor.sequence = snapshot.sequence;
        actor.waiting_signal = snapshot.waiting_signal.clone();
        actor.install_authority_manifest(&authority_manifest);
        actor.bytecode_module = Some(module.clone());
        actor.bytecode_offsets = offsets;
        actor.compensation_offsets = compensation_offsets;

        actor.state_models = meta
            .state_models
            .iter()
            .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
            .collect();

        for (name, value) in &snapshot.state {
            if name == "semantic_memory" || name == "procedural_memory" {
                if let PersistedValue::String(json) = value {
                    let ptr = actor.allocate_string(json);
                    actor.set_state_field(name, ptr);
                    continue;
                }
            }
            let v = value.to_value_on_heap(&mut actor);
            actor.set_state_field(name, v);
        }

        for (name, c) in &meta.state_defaults {
            if actor.get_state_field(name).is_some() {
                continue;
            }
            let v = match c {
                crate::bytecode::Constant::String(s) => actor.allocate_string(s),
                other => crate::vm::constant_to_value(other),
            };
            actor.set_state_field(name, v);
        }

        if is_agent {
            for (name, c) in &meta.state_defaults {
                if let crate::bytecode::Constant::String(json) = c {
                    if name == "retry_config" {
                        actor.retry_config = serde_json::from_str(json).ok();
                    } else if name == "fallback_config" {
                        actor.fallback_config = serde_json::from_str(json).unwrap_or_default();
                    }
                }
            }
        }

        Ok(actor)
    }

'''


transform_region(
    "src/runtime/mod.rs",
    "    fn restore_actor_from_snapshot(\n",
    "    /// Resolve a virtual actor (grain) identity to a resident actor id,\n",
    replace_restore,
    "shared snapshot restore schema ownership",
)

# Grain hydration binds the persisted schema to the requested virtual actor
# type and preserves the human-readable Type@key runtime name.
def patch_grain(region: str) -> str:
    old_call = '''            Self::restore_actor_from_snapshot(
                stable_actor_id,
                &grain_type.module,
                snap,
                false,
                false,
            )
'''
    new_call = '''            Self::restore_actor_from_snapshot(
                stable_actor_id,
                &grain_type.module,
                snap,
                Some(&grain_id.grain_type),
                Some(grain_id.actor_name()),
            )
'''
    if region.count(old_call) != 1:
        raise SystemExit("grain hydration: restore call mismatch")
    region = region.replace(old_call, new_call, 1)
    region = region.replace(
        '"invalid authority snapshot for virtual actor {}: {}",',
        '"invalid durable snapshot for virtual actor {}: {}",',
        1,
    )
    old_models = '''            actor.state_models = grain_type
                .default_models
                .iter()
                .map(|(name, model)| (name.clone(), *model))
                .collect();
'''
    new_models = '''            let grain_meta = crate::runtime_behavior_ownership::actor_meta_for_schema(
                &grain_type.module,
                &grain_id.grain_type,
            )
            .expect("registered grain type must retain its ActorMeta");
            actor.state_models = grain_meta
                .state_models
                .iter()
                .map(|(name, model)| (name.clone(), map_ast_state_model(*model)))
                .collect();
'''
    if region.count(old_models) != 1:
        raise SystemExit("grain hydration: default model block mismatch")
    region = region.replace(old_models, new_models, 1)
    old_defaults = '''            for (name, c) in grain_type
                .module
                .actor_metadata
                .iter()
                .flat_map(|m| &m.state_defaults)
            {
'''
    new_defaults = '''            for (name, c) in &grain_meta.state_defaults {
'''
    if region.count(old_defaults) != 1:
        raise SystemExit("grain hydration: all-schema default block mismatch")
    return region.replace(old_defaults, new_defaults, 1)


transform_region(
    "src/runtime/mod.rs",
    "    pub fn resolve_or_hydrate_grain(&mut self, grain_id: GrainId) -> Result<u64, NuError> {\n",
    "    /// Receive a migrated actor from another node.\n",
    patch_grain,
    "grain durable schema ownership",
)

# Migration validates schema before actor construction and derives role and
# workflow compensation layout only from that selected metadata.
def patch_migration(region: str) -> str:
    old_roles = '''        let is_workflow = module.actor_metadata.iter().any(|m| m.is_workflow);
        let is_agent = module.actor_metadata.iter().any(|m| m.is_agent);

        let actor = match Self::restore_actor_from_snapshot(
            actor_id,
            &module,
            &snapshot,
            is_workflow,
            is_agent,
        ) {
'''
    new_roles = '''        let schema_meta = match crate::runtime_schema_identity::resolve_snapshot_actor_meta(
            &module,
            snapshot.schema_name.as_deref(),
        ) {
            Ok(meta) => meta,
            Err(err) => {
                warn!(
                    "nulang-migrate: invalid schema identity for actor {}: {}",
                    actor_id, err
                );
                return false;
            }
        };
        let is_workflow = schema_meta.is_workflow;

        let actor = match Self::restore_actor_from_snapshot(
            actor_id,
            &module,
            &snapshot,
            None,
            None,
        ) {
'''
    if region.count(old_roles) != 1:
        raise SystemExit("migration: role/restore block mismatch")
    region = region.replace(old_roles, new_roles, 1)
    region = region.replace(
        '"nulang-migrate: invalid authority manifest for actor {}: {}",',
        '"nulang-migrate: invalid durable snapshot for actor {}: {}",',
        1,
    )
    old_comp = '''        let compensation_offsets: Vec<Option<usize>> = module
            .actor_metadata
            .iter()
            .find(|m| m.is_workflow)
            .map(|meta| {
                meta.behavior_indices
                    .iter()
                    .map(|&i| module.behaviors[i].compensate_offset.map(|o| o as usize))
                    .collect()
            })
            .unwrap_or_else(|| {
                module
                    .behaviors
                    .iter()
                    .map(|b| b.compensate_offset.map(|o| o as usize))
                    .collect()
            });
'''
    new_comp = '''        let compensation_offsets: Vec<Option<usize>> = if is_workflow {
            schema_meta
                .behavior_indices
                .iter()
                .map(|&i| module.behaviors[i].compensate_offset.map(|o| o as usize))
                .collect()
        } else {
            module
                .behaviors
                .iter()
                .map(|b| b.compensate_offset.map(|o| o as usize))
                .collect()
        };
'''
    if region.count(old_comp) != 1:
        raise SystemExit("migration: workflow compensation block mismatch")
    return region.replace(old_comp, new_comp, 1)


transform_region(
    "src/runtime/mod.rs",
    "    pub fn receive_migrated_actor(\n",
    "    /// Apply a single workflow event to an actor's state.  Used during recovery\n",
    patch_migration,
    "migration durable schema ownership",
)

# Focused end-to-end regressions.
Path("tests/runtime_snapshot_schema_identity.rs").write_text(r'''use nulang::lexer::Lexer;
use nulang::parser::Parser;
use nulang::runtime::{ActorSnapshot, GrainId, PersistenceStore, Runtime};
use nulang::typechecker::TypeChecker;

fn compile(source: &str) -> nulang::bytecode::CodeModule {
    let tokens = Lexer::new(source).lex().expect("lex");
    let ast = Parser::new(tokens).parse_module().expect("parse");
    let mut typechecker = TypeChecker::new();
    typechecker.check_module(&ast).expect("typecheck");
    let hir = nulang::hir_lower::lower_module(&ast, &typechecker.inferred_decl_types);
    let mut mir = nulang::mir_lower::lower_module(&hir).expect("MIR lowering");
    nulang::mir_codegen::compile_mir(&mut mir, "runtime_snapshot_schema_identity")
        .expect("bytecode codegen")
}

fn register_for_recovery(rt: &mut Runtime, actor_id: u64, module: nulang::bytecode::CodeModule) {
    let offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.code_offset as usize)
        .collect();
    let compensation_offsets = module
        .behaviors
        .iter()
        .map(|behavior| behavior.compensate_offset.map(|offset| offset as usize))
        .collect();
    rt.register_recovery_module(actor_id, module, offsets, compensation_offsets);
}

#[test]
fn legacy_snapshot_json_without_schema_name_remains_readable() {
    let snapshot: ActorSnapshot = serde_json::from_str(
        r#"{
            "actor_id": 7,
            "sequence": 3,
            "state": {},
            "waiting_signal": null,
            "crdt_snapshot": null,
            "crdt_field_map": null,
            "authority_tokens": []
        }"#,
    )
    .expect("legacy snapshot json");
    assert_eq!(snapshot.actor_id, 7);
    assert_eq!(snapshot.schema_name, None);
}

#[test]
fn recovery_restores_only_the_persisted_actor_schema() {
    let module = compile(
        r#"
        persistent actor First {
            state durable first_only: Int = 11
            behavior hit() -> Int { self.first_only }
        }
        persistent actor Second {
            state durable second_only: Int = 22
            behavior hit() -> Int { self.second_only }
        }
        "#,
    );
    let actor_id = 7001;
    let mut rt = Runtime::new();
    register_for_recovery(&mut rt, actor_id, module);
    rt.persistence
        .save_snapshot(ActorSnapshot {
            actor_id,
            sequence: 0,
            state: Default::default(),
            waiting_signal: None,
            crdt_snapshot: None,
            crdt_field_map: None,
            schema_name: Some("Second".to_string()),
            authority_tokens: Default::default(),
        })
        .expect("snapshot");

    assert_eq!(rt.recover_actor(actor_id), Some(actor_id));
    let actor = &rt.actors[&actor_id];
    assert_eq!(actor.name, "Second");
    assert!(actor.get_state_field("first_only").is_none());
    assert_eq!(
        actor
            .get_state_field("second_only")
            .and_then(|value| value.as_int()),
        Some(22)
    );
}

#[test]
fn recovery_rejects_unknown_or_ambiguous_schema_identity() {
    let module = compile(
        r#"
        persistent actor First { behavior hit() { nil } }
        persistent actor Second { behavior hit() { nil } }
        "#,
    );

    let mut unknown = Runtime::new();
    register_for_recovery(&mut unknown, 7002, module.clone());
    unknown
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id: 7002,
            schema_name: Some("Missing".to_string()),
            ..Default::default()
        })
        .expect("snapshot");
    assert_eq!(unknown.recover_actor(7002), None);

    let mut legacy = Runtime::new();
    register_for_recovery(&mut legacy, 7003, module);
    legacy
        .persistence
        .save_snapshot(ActorSnapshot {
            actor_id: 7003,
            ..Default::default()
        })
        .expect("legacy snapshot");
    assert_eq!(legacy.recover_actor(7003), None);
}

#[test]
fn grain_hydration_rejects_a_different_valid_module_schema() {
    let module = compile(
        r#"
        virtual entity Counter(key: String) {
            state durable n: Int = 0
            behavior hit() { self.n = self.n + 1 }
        }
        actor Other { behavior hit() { nil } }
        "#,
    );
    let grain_id = GrainId::new("Counter", "customer-42");
    let mut rt = Runtime::new();
    rt.register_module_grains(&module);
    let stable_id = rt
        .resolve_or_hydrate_grain(grain_id.clone())
        .expect("fresh grain");
    rt.actors.remove(&stable_id);
    rt.grain_residents.remove(&grain_id);
    rt.actor_grain_id.remove(&stable_id);

    rt.persistence
        .save_snapshot(ActorSnapshot {
            actor_id: stable_id,
            schema_name: Some("Other".to_string()),
            ..Default::default()
        })
        .expect("mismatched grain snapshot");

    let error = rt
        .resolve_or_hydrate_grain(grain_id)
        .expect_err("Counter must reject an Other snapshot");
    assert!(error.to_string().contains("does not match expected schema"));
}
''')
