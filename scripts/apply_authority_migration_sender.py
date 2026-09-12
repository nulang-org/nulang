#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected 1 occurrence, found {count}")
    return text.replace(old, new, 1)


path = Path("src/runtime/callbacks.rs")
text = path.read_text()

anchor = '''pub(crate) fn spawn_with_site_authority(\n    rt: &mut Runtime,\n    module: &crate::bytecode::CodeModule,\n    spawn_pc: usize,\n    behavior_idx: usize,\n    init: Vec<(String, crate::vm::Value)>,\n) -> crate::vm::Value {'''
if text.count(anchor) != 1:
    raise SystemExit(f"spawn helper anchor drift: {text.count(anchor)}")
helper = '''/// Return the canonical authority token set that must cross a migration\n/// boundary with `actor`. A malformed in-memory compatibility manifest is a\n/// security error: callers must abort migration before sending a packet or\n/// reaping the source actor.\npub(crate) fn migration_authority_tokens(\n    actor: &crate::runtime::Actor,\n) -> Result<std::collections::BTreeSet<String>, crate::authority_runtime::RuntimeAuthorityError> {\n    let manifest = actor.authority_manifest()?;\n    Ok(manifest.canonical_token_set())\n}\n\n'''
text = text.replace(anchor, helper + anchor, 1)

old = '''                let snapshot = crate::runtime::persistence::ActorSnapshot {\n                    actor_id,\n                    sequence: actor.sequence,\n                    state,\n                    waiting_signal: actor.waiting_signal.clone(),\n                    crdt_snapshot,\n                    crdt_field_map,\n                authority_tokens: Default::default(),\n                };'''
new = '''                let authority_tokens = match migration_authority_tokens(actor) {\n                    Ok(tokens) => tokens,\n                    Err(error) => {\n                        tracing::warn!(\n                            actor_id,\n                            %error,\n                            "nulang-migrate: refusing to migrate actor with invalid authority manifest"\n                        );\n                        return;\n                    }\n                };\n                let snapshot = crate::runtime::persistence::ActorSnapshot {\n                    actor_id,\n                    sequence: actor.sequence,\n                    state,\n                    waiting_signal: actor.waiting_signal.clone(),\n                    crdt_snapshot,\n                    crdt_field_map,\n                    authority_tokens,\n                };'''
text = replace_once(text, old, new, "migration snapshot authority")

text += '''\n\n#[cfg(test)]\nmod migration_authority_tests {\n    use super::migration_authority_tokens;\n    use crate::authority::AuthorityManifest;\n    use crate::authority_runtime::RuntimeAuthorityError;\n    use crate::runtime::Actor;\n\n    #[test]\n    fn migration_sender_preserves_canonical_actor_authority() {\n        let mut actor = Actor::new(700_001, "migration-authority", 8);\n        let manifest = AuthorityManifest::from_tokens([\n            "Secret::Read(MIGRATION_KEY)",\n            "Net::TcpOut(api.example.com:443)",\n        ])\n        .unwrap();\n        actor.install_authority_manifest(&manifest);\n\n        let tokens = migration_authority_tokens(&actor).unwrap();\n        assert_eq!(tokens, manifest.canonical_token_set());\n    }\n\n    #[test]\n    fn migration_sender_rejects_malformed_actor_authority() {\n        let mut actor = Actor::new(700_002, "migration-authority-invalid", 8);\n        actor\n            .capabilities\n            .insert("Net::TcpOut(malformed)".to_string());\n\n        assert!(matches!(\n            migration_authority_tokens(&actor),\n            Err(RuntimeAuthorityError::InvalidManifest(_))\n        ));\n    }\n}\n'''

path.write_text(text)
print("migration sender authority patch applied")
