#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {count}")
    return text.replace(old, new, 1)


path = Path("src/runtime/spawn.rs")
text = path.read_text()

text = replace_once(
    text,
    "use crate::runtime::persistence::{PersistedValue, StateModel, WorkflowEvent};",
    "use crate::runtime::persistence::{ActorSnapshot, PersistedValue, StateModel, WorkflowEvent};",
    "ActorSnapshot import",
)

anchor = "/// Spawn an actor with a pre-assigned id. `Runtime::spawn_actor_near` uses\n"
preflight = '''/// Load and validate legacy restart snapshot authority before actor initialization.\n///\n/// A malformed persisted manifest aborts activation before the init closure,\n/// CRDT registration, actor insertion, or scheduler enqueue. Pre-authority\n/// snapshots deserialize with an empty token set and therefore remain\n/// deny-by-default.\nfn preflight_persistent_snapshot(\n    rt: &Runtime,\n    actor_id: u64,\n) -> Result<Option<(ActorSnapshot, AuthorityManifest)>, RuntimeAuthorityError> {\n    let Some(snapshot) = rt.persistence.load_snapshot(actor_id) else {\n        return Ok(None);\n    };\n    let manifest = AuthorityManifest::from_tokens(\n        snapshot.authority_tokens.iter().map(String::as_str),\n    )?;\n    Ok(Some((snapshot, manifest)))\n}\n\n'''
if text.count(anchor) != 1:
    raise SystemExit("preflight anchor drift")
text = text.replace(anchor, preflight + anchor, 1)

text = replace_once(
    text,
    ") -> u64 {\n    let mut actor = Actor::new(id, format!(\"actor_{}\", id), 0);\n    let state_fields = init();",
    ") -> u64 {\n    let restart_snapshot = if persistent && workflow.is_none() {\n        match preflight_persistent_snapshot(rt, id) {\n            Ok(snapshot) => snapshot,\n            Err(error) => {\n                tracing::warn!(\n                    actor_id = id,\n                    %error,\n                    \"refusing to activate persistent actor with invalid authority snapshot\"\n                );\n                return id;\n            }\n        }\n    } else {\n        None\n    };\n\n    let mut actor = Actor::new(id, format!(\"actor_{}\", id), 0);\n    let state_fields = init();",
    "persistent preflight before init",
)

text = replace_once(
    text,
    "    actor.state_models = state_models;\n    // Register CRDT-backed fields with the CrdtManager.\n    if let Some(ref mut mgr) = rt.crdt_manager {\n        mgr.register_actor_fields(id, &actor);\n    }\n    actor.persistent = persistent;",
    "    actor.state_models = state_models;\n    actor.persistent = persistent;",
    "defer CRDT registration until validated restore",
)

text = replace_once(
    text,
    "    if persistent && workflow.is_none() {\n        restore_persistent_state(rt, &mut actor);\n    }\n    rt.actors.insert(id, actor);",
    "    if persistent && workflow.is_none() {\n        restore_persistent_state(rt, &mut actor, restart_snapshot);\n    }\n    // Register CRDT-backed fields only after persisted authority has been\n    // validated and installed, so rejected activations leave no manager state.\n    if let Some(ref mut mgr) = rt.crdt_manager {\n        mgr.register_actor_fields(id, &actor);\n    }\n    rt.actors.insert(id, actor);",
    "restore prevalidated snapshot then register CRDTs",
)

old_restore = '''fn restore_persistent_state(rt: &Runtime, actor: &mut Actor) {\n    // Event-sourced-only actors may have an event log but no snapshot\n    // (EventSourced fields are excluded from snapshots by design), so both\n    // halves run independently.\n    if let Some(snapshot) = rt.persistence.load_snapshot(actor.id) {\n        actor.sequence = snapshot.sequence;\n        actor.waiting_signal = snapshot.waiting_signal;\n        for (name, value) in snapshot.state {\n            let v = value.to_value_on_heap(actor);\n            actor.set_state_field(name, v);\n        }\n    }\n'''
new_restore = '''fn restore_persistent_state(\n    rt: &Runtime,\n    actor: &mut Actor,\n    snapshot: Option<(ActorSnapshot, AuthorityManifest)>,\n) {\n    // Event-sourced-only actors may have an event log but no snapshot\n    // (EventSourced fields are excluded from snapshots by design), so both\n    // halves run independently. Authority was parsed in the preflight phase\n    // before the actor was initialized or made observable.\n    if let Some((snapshot, authority)) = snapshot {\n        actor.install_authority_manifest(&authority);\n        actor.sequence = snapshot.sequence;\n        actor.waiting_signal = snapshot.waiting_signal;\n        for (name, value) in snapshot.state {\n            let v = value.to_value_on_heap(actor);\n            actor.set_state_field(name, v);\n        }\n    }\n'''
text = replace_once(text, old_restore, new_restore, "restore prevalidated authority")

closing = '''    #[test]\n    fn malformed_parent_manifest_fails_before_child_creation() {\n        let mut rt = Runtime::new();\n        let parent_id = rt.spawn_actor(Box::new(|| vec![]));\n        rt.actors\n            .get_mut(&parent_id)\n            .unwrap()\n            .capabilities\n            .insert("Net::TcpOut(malformed)".to_string());\n        rt.current_actor = Some(parent_id);\n        let before = rt.actors.len();\n        let requested = AuthorityManifest::new();\n        let module = CodeModule::new("malformed-parent-authority");\n\n        let result = spawn_from_module_with_authority(&mut rt, &module, 0, vec![], &requested);\n\n        assert!(matches!(\n            result,\n            Err(RuntimeAuthorityError::InvalidManifest(_))\n        ));\n        assert_eq!(rt.actors.len(), before);\n    }\n}'''
replacement = '''    #[test]\n    fn malformed_parent_manifest_fails_before_child_creation() {\n        let mut rt = Runtime::new();\n        let parent_id = rt.spawn_actor(Box::new(|| vec![]));\n        rt.actors\n            .get_mut(&parent_id)\n            .unwrap()\n            .capabilities\n            .insert("Net::TcpOut(malformed)".to_string());\n        rt.current_actor = Some(parent_id);\n        let before = rt.actors.len();\n        let requested = AuthorityManifest::new();\n        let module = CodeModule::new("malformed-parent-authority");\n\n        let result = spawn_from_module_with_authority(&mut rt, &module, 0, vec![], &requested);\n\n        assert!(matches!(\n            result,\n            Err(RuntimeAuthorityError::InvalidManifest(_))\n        ));\n        assert_eq!(rt.actors.len(), before);\n    }\n\n    #[test]\n    fn legacy_restart_restores_snapshot_authority() {\n        let mut rt = Runtime::new();\n        let actor_id = 910_001;\n        rt.persistence\n            .save_snapshot(ActorSnapshot {\n                actor_id,\n                sequence: 7,\n                authority_tokens: std::collections::BTreeSet::from([\n                    "Secret::Read(RESTART_KEY)".to_string(),\n                ]),\n                ..ActorSnapshot::default()\n            })\n            .unwrap();\n\n        let returned = spawn_actor_with_id(\n            &mut rt,\n            actor_id,\n            Box::new(|| vec![]),\n            std::collections::HashMap::new(),\n            true,\n            None,\n        );\n\n        assert_eq!(returned, actor_id);\n        let actor = rt.actors.get(&actor_id).expect("persistent actor published");\n        assert_eq!(actor.sequence, 7);\n        assert!(actor\n            .authority_manifest()\n            .unwrap()\n            .allows(&AuthorityGrant::SecretRead {\n                name: "RESTART_KEY".into(),\n            }));\n    }\n\n    #[test]\n    fn malformed_legacy_restart_authority_fails_before_init_or_publish() {\n        use std::cell::Cell;\n        use std::rc::Rc;\n\n        let mut rt = Runtime::new();\n        let actor_id = 910_002;\n        rt.persistence\n            .save_snapshot(ActorSnapshot {\n                actor_id,\n                authority_tokens: std::collections::BTreeSet::from([\n                    "Net::TcpOut(malformed)".to_string(),\n                ]),\n                ..ActorSnapshot::default()\n            })\n            .unwrap();\n\n        let init_ran = Rc::new(Cell::new(false));\n        let init_flag = Rc::clone(&init_ran);\n        let returned = spawn_actor_with_id(\n            &mut rt,\n            actor_id,\n            Box::new(move || {\n                init_flag.set(true);\n                vec![]\n            }),\n            std::collections::HashMap::new(),\n            true,\n            None,\n        );\n\n        assert_eq!(returned, actor_id);\n        assert!(!init_ran.get(), "invalid authority must abort before init");\n        assert!(\n            !rt.actors.contains_key(&actor_id),\n            "invalid authority must not publish a runnable actor"\n        );\n    }\n}'''
text = replace_once(text, closing, replacement, "legacy restart regression tests")

path.write_text(text)
print("legacy authority recovery patch applied")
