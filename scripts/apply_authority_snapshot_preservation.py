#!/usr/bin/env python3
from pathlib import Path
import re


def read(path):
    return Path(path).read_text()


def write(path, text):
    Path(path).write_text(text)


def replace_once(text, old, new, label):
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly 1 occurrence, found {count}")
    return text.replace(old, new, 1)


def replace_n(text, old, new, expected, label):
    count = text.count(old)
    if count != expected:
        raise SystemExit(f"{label}: expected {expected} occurrences, found {count}")
    return text.replace(old, new)


# ---------------------------------------------------------------------------
# Snapshot model + relational persistence backends
# ---------------------------------------------------------------------------
p = "src/runtime/persistence.rs"
s = read(p)
s = replace_once(
    s,
    "use std::collections::HashMap;",
    "use std::collections::{BTreeSet, HashMap};",
    "persistence imports",
)
s = replace_once(
    s,
    "    #[serde(default)]\n    pub crdt_field_map: Option<HashMap<String, u64>>,\n}",
    "    #[serde(default)]\n    pub crdt_field_map: Option<HashMap<String, u64>>,\n"
    "    /// Canonical external-authority tokens held by the actor at the time\n"
    "    /// of the snapshot. Missing on pre-authority snapshots means empty\n"
    "    /// authority (deny by default). Values are reparsed as a complete\n"
    "    /// typed manifest before any recovered actor becomes observable.\n"
    "    #[serde(default)]\n"
    "    pub authority_tokens: BTreeSet<String>,\n}",
    "ActorSnapshot authority field",
)

# libSQL/Turso schema + migration.
s = replace_once(
    s,
    "                    crdt_snapshot TEXT,\n                    crdt_field_map TEXT\n                )\"",
    "                    crdt_snapshot TEXT,\n                    crdt_field_map TEXT,\n                    authority_tokens TEXT\n                )\"",
    "libsql snapshots schema",
)
s = replace_once(
    s,
    "            // Migrate databases created before the crdt_field_map column existed.\n"
    "            let _ = conn\n"
    "                .execute(\"ALTER TABLE snapshots ADD COLUMN crdt_field_map TEXT\", ())\n"
    "                .await;",
    "            // Migrate databases created before the crdt_field_map column existed.\n"
    "            let _ = conn\n"
    "                .execute(\"ALTER TABLE snapshots ADD COLUMN crdt_field_map TEXT\", ())\n"
    "                .await;\n"
    "            // Authority was added after the original persistence schema. Old rows\n"
    "            // remain NULL and therefore restore with empty (deny-by-default) authority.\n"
    "            let _ = conn\n"
    "                .execute(\"ALTER TABLE snapshots ADD COLUMN authority_tokens TEXT\", ())\n"
    "                .await;",
    "libsql authority migration",
)
s = replace_once(
    s,
    "        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)\n"
    "            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;\n"
    "        let conn = self.conn();\n"
    "        self.rt.block_on(async {\n"
    "            conn.execute(\n"
    "                \"INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map) VALUES (?1, ?2, ?3, ?4, ?5, ?6)\n"
    "                 ON CONFLICT(actor_id) DO UPDATE SET sequence=excluded.sequence, state=excluded.state, waiting_signal=excluded.waiting_signal, crdt_snapshot=excluded.crdt_snapshot, crdt_field_map=excluded.crdt_field_map\",\n"
    "                libsql::params![snapshot.actor_id as i64, snapshot.sequence as i64, state_json, snapshot.waiting_signal.as_deref(), crdt_json.as_str(), crdt_field_map_json.as_str()],\n"
    "            ).await.map(|_| ()).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))\n"
    "        })",
    "        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)\n"
    "            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;\n"
    "        let authority_json = serde_json::to_string(&snapshot.authority_tokens)\n"
    "            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;\n"
    "        let conn = self.conn();\n"
    "        self.rt.block_on(async {\n"
    "            conn.execute(\n"
    "                \"INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)\n"
    "                 ON CONFLICT(actor_id) DO UPDATE SET sequence=excluded.sequence, state=excluded.state, waiting_signal=excluded.waiting_signal, crdt_snapshot=excluded.crdt_snapshot, crdt_field_map=excluded.crdt_field_map, authority_tokens=excluded.authority_tokens\",\n"
    "                libsql::params![snapshot.actor_id as i64, snapshot.sequence as i64, state_json, snapshot.waiting_signal.as_deref(), crdt_json.as_str(), crdt_field_map_json.as_str(), authority_json.as_str()],\n"
    "            ).await.map(|_| ()).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))\n"
    "        })",
    "libsql save snapshot authority",
)
s = replace_once(
    s,
    "                    \"SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map FROM snapshots WHERE actor_id = ?1\",",
    "                    \"SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens FROM snapshots WHERE actor_id = ?1\",",
    "libsql select authority",
)
s = replace_once(
    s,
    "            let crdt_field_map_json: Option<String> = row.get(4).ok()?;\n"
    "            let crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>> = match crdt_json {",
    "            let crdt_field_map_json: Option<String> = row.get(4).ok()?;\n"
    "            let authority_json: Option<String> = row.get(5).ok()?;\n"
    "            let crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>> = match crdt_json {",
    "libsql authority row",
)
s = replace_once(
    s,
    "            let state: HashMap<String, PersistedValue> = serde_json::from_str(&state_json).ok()?;\n"
    "            Some(ActorSnapshot {\n"
    "                actor_id,\n"
    "                sequence: sequence as u64,\n"
    "                state,\n"
    "                waiting_signal,\n"
    "                crdt_snapshot,\n"
    "                crdt_field_map,\n"
    "            })",
    "            let authority_tokens: BTreeSet<String> = match authority_json {\n"
    "                Some(json) => match serde_json::from_str(&json) {\n"
    "                    Ok(tokens) => tokens,\n"
    "                    Err(err) => {\n"
    "                        warn!(\n"
    "                            \"nulang-persist: invalid authority metadata for actor {}: {}\",\n"
    "                            actor_id, err\n"
    "                        );\n"
    "                        return None;\n"
    "                    }\n"
    "                },\n"
    "                None => BTreeSet::new(),\n"
    "            };\n"
    "            let state: HashMap<String, PersistedValue> = serde_json::from_str(&state_json).ok()?;\n"
    "            Some(ActorSnapshot {\n"
    "                actor_id,\n"
    "                sequence: sequence as u64,\n"
    "                state,\n"
    "                waiting_signal,\n"
    "                crdt_snapshot,\n"
    "                crdt_field_map,\n"
    "                authority_tokens,\n"
    "            })",
    "libsql load snapshot authority",
)

# PostgreSQL schema, migration, save and load.
s = replace_once(
    s,
    "                crdt_snapshot TEXT,\n                crdt_field_map TEXT\n            )\"",
    "                crdt_snapshot TEXT,\n                crdt_field_map TEXT,\n                authority_tokens TEXT\n            )\"",
    "postgres snapshots schema",
)
s = replace_once(
    s,
    "        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;\n"
    "        conn.execute(\n"
    "            \"CREATE TABLE IF NOT EXISTS journal (",
    "        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;\n"
    "        conn.execute(\n"
    "            \"ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS authority_tokens TEXT\",\n"
    "            &[],\n"
    "        )\n"
    "        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;\n"
    "        conn.execute(\n"
    "            \"CREATE TABLE IF NOT EXISTS journal (",
    "postgres authority migration",
)
# The serialization prefix occurs once in each relational backend. The libSQL one
# was already rewritten with authority_json, so one original occurrence remains.
s = replace_once(
    s,
    "        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)\n"
    "            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;\n"
    "        let mut conn = self.conn.lock().unwrap();",
    "        let crdt_field_map_json = serde_json::to_string(&snapshot.crdt_field_map)\n"
    "            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;\n"
    "        let authority_json = serde_json::to_string(&snapshot.authority_tokens)\n"
    "            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;\n"
    "        let mut conn = self.conn.lock().unwrap();",
    "postgres authority serialization",
)
s = replace_once(
    s,
    "            \"INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map)\n"
    "             VALUES ($1, $2, $3, $4, $5, $6)\n"
    "             ON CONFLICT (actor_id) DO UPDATE SET\n"
    "               sequence = EXCLUDED.sequence,\n"
    "               state = EXCLUDED.state,\n"
    "               waiting_signal = EXCLUDED.waiting_signal,\n"
    "               crdt_snapshot = EXCLUDED.crdt_snapshot,\n"
    "               crdt_field_map = EXCLUDED.crdt_field_map\",\n"
    "            &[\n"
    "                &(snapshot.actor_id as i64),\n"
    "                &(snapshot.sequence as i64),\n"
    "                &state_json,\n"
    "                &snapshot.waiting_signal.as_deref(),\n"
    "                &crdt_json.as_str(),\n"
    "                &crdt_field_map_json.as_str(),\n"
    "            ],",
    "            \"INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens)\n"
    "             VALUES ($1, $2, $3, $4, $5, $6, $7)\n"
    "             ON CONFLICT (actor_id) DO UPDATE SET\n"
    "               sequence = EXCLUDED.sequence,\n"
    "               state = EXCLUDED.state,\n"
    "               waiting_signal = EXCLUDED.waiting_signal,\n"
    "               crdt_snapshot = EXCLUDED.crdt_snapshot,\n"
    "               crdt_field_map = EXCLUDED.crdt_field_map,\n"
    "               authority_tokens = EXCLUDED.authority_tokens\",\n"
    "            &[\n"
    "                &(snapshot.actor_id as i64),\n"
    "                &(snapshot.sequence as i64),\n"
    "                &state_json,\n"
    "                &snapshot.waiting_signal.as_deref(),\n"
    "                &crdt_json.as_str(),\n"
    "                &crdt_field_map_json.as_str(),\n"
    "                &authority_json.as_str(),\n"
    "            ],",
    "postgres save snapshot authority",
)
s = replace_once(
    s,
    "                \"SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map\n                 FROM snapshots WHERE actor_id = $1\",",
    "                \"SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens\n                 FROM snapshots WHERE actor_id = $1\",",
    "postgres select authority",
)
s = replace_once(
    s,
    "        let crdt_field_map_json: Option<String> = row.get(4);\n"
    "        let crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>> =\n"
    "            crdt_json.and_then(|j| serde_json::from_str(&j).ok());",
    "        let crdt_field_map_json: Option<String> = row.get(4);\n"
    "        let authority_json: Option<String> = row.get(5);\n"
    "        let crdt_snapshot: Option<Vec<(u64, u8, Vec<u8>)>> =\n"
    "            crdt_json.and_then(|j| serde_json::from_str(&j).ok());",
    "postgres authority row",
)
s = replace_once(
    s,
    "        let state: HashMap<String, PersistedValue> = serde_json::from_str(&state_json).ok()?;\n"
    "        Some(ActorSnapshot {\n"
    "            actor_id,\n"
    "            sequence: sequence as u64,\n"
    "            state,\n"
    "            waiting_signal,\n"
    "            crdt_snapshot,\n"
    "            crdt_field_map,\n"
    "        })",
    "        let authority_tokens: BTreeSet<String> = match authority_json {\n"
    "            Some(json) => match serde_json::from_str(&json) {\n"
    "                Ok(tokens) => tokens,\n"
    "                Err(err) => {\n"
    "                    warn!(\n"
    "                        \"nulang-persist: invalid authority metadata for actor {}: {}\",\n"
    "                        actor_id, err\n"
    "                    );\n"
    "                    return None;\n"
    "                }\n"
    "            },\n"
    "            None => BTreeSet::new(),\n"
    "        };\n"
    "        let state: HashMap<String, PersistedValue> = serde_json::from_str(&state_json).ok()?;\n"
    "        Some(ActorSnapshot {\n"
    "            actor_id,\n"
    "            sequence: sequence as u64,\n"
    "            state,\n"
    "            waiting_signal,\n"
    "            crdt_snapshot,\n"
    "            crdt_field_map,\n"
    "            authority_tokens,\n"
    "        })",
    "postgres load snapshot authority",
)
write(p, s)

# ---------------------------------------------------------------------------
# Normal checkpoint path: validate full typed manifest, then persist canonical.
# ---------------------------------------------------------------------------
p = "src/runtime/workflow.rs"
s = read(p)
s = replace_once(
    s,
    "    // Snapshot the global CRDT state alongside durable actor fields.\n",
    "    let authority_tokens = match actor.authority_manifest() {\n"
    "        Ok(manifest) => manifest.canonical_token_set(),\n"
    "        Err(err) => {\n"
    "            tracing::warn!(\n"
    "                \"nulang-persist: refusing to checkpoint actor {} with invalid authority: {}\",\n"
    "                actor_id, err\n"
    "            );\n"
    "            return;\n"
    "        }\n"
    "    };\n"
    "    // Snapshot the global CRDT state alongside durable actor fields.\n",
    "checkpoint authority validation",
)
s = replace_once(
    s,
    "        waiting_signal: actor.waiting_signal.clone(),\n        crdt_snapshot,\n        crdt_field_map,\n    };",
    "        waiting_signal: actor.waiting_signal.clone(),\n        crdt_snapshot,\n        crdt_field_map,\n        authority_tokens,\n    };",
    "checkpoint snapshot authority",
)
write(p, s)

# ---------------------------------------------------------------------------
# Runtime recovery, grain dehydration/hydration, and migration.
# ---------------------------------------------------------------------------
p = "src/runtime/mod.rs"
s = read(p)

# build_actor_snapshot: refuse to serialize a malformed in-memory token set.
s = replace_once(
    s,
    "        let waiting_signal = {\n            let actor = self.actors.get(&actor_id)?;",
    "        let (waiting_signal, authority_tokens) = {\n            let actor = self.actors.get(&actor_id)?;",
    "build snapshot tuple",
)
s = replace_once(
    s,
    "            actor.waiting_signal.clone()\n        };\n        let sequence = self.next_sequence(actor_id);",
    "            let authority_tokens = match actor.authority_manifest() {\n"
    "                Ok(manifest) => manifest.canonical_token_set(),\n"
    "                Err(err) => {\n"
    "                    warn!(\n"
    "                        \"nulang-persist: refusing to snapshot actor {} with invalid authority: {}\",\n"
    "                        actor_id, err\n"
    "                    );\n"
    "                    return None;\n"
    "                }\n"
    "            };\n"
    "            (actor.waiting_signal.clone(), authority_tokens)\n"
    "        };\n"
    "        let sequence = self.next_sequence(actor_id);",
    "build snapshot authority validation",
)
s = replace_once(
    s,
    "            waiting_signal,\n            crdt_snapshot,\n            crdt_field_map,\n        })",
    "            waiting_signal,\n            crdt_snapshot,\n            crdt_field_map,\n            authority_tokens,\n        })",
    "build snapshot authority field",
)

# Restart recovery: parse the entire manifest before any runtime mutation.
s = replace_once(
    s,
    "    pub fn recover_actor(&mut self, actor_id: u64) -> Option<u64> {\n        let snapshot = self.persistence.load_snapshot(actor_id)?;\n        let workflow_events = self.persistence.read_workflow_events(actor_id);",
    "    pub fn recover_actor(&mut self, actor_id: u64) -> Option<u64> {\n"
    "        let snapshot = self.persistence.load_snapshot(actor_id)?;\n"
    "        let authority_manifest = match crate::authority::AuthorityManifest::from_token_set(\n"
    "            &snapshot.authority_tokens,\n"
    "        ) {\n"
    "            Ok(manifest) => manifest,\n"
    "            Err(err) => {\n"
    "                warn!(\n"
    "                    \"nulang-recover: refusing actor {} with invalid authority manifest: {}\",\n"
    "                    actor_id, err\n"
    "                );\n"
    "                return None;\n"
    "            }\n"
    "        };\n"
    "        let workflow_events = self.persistence.read_workflow_events(actor_id);",
    "recover authority preflight",
)
s = replace_once(
    s,
    "        actor.sequence = snapshot.sequence;\n        actor.waiting_signal = snapshot.waiting_signal;\n        // Restore CRDT state if present in the snapshot.",
    "        actor.sequence = snapshot.sequence;\n"
    "        actor.waiting_signal = snapshot.waiting_signal;\n"
    "        actor.install_authority_manifest(&authority_manifest);\n"
    "        // Restore CRDT state if present in the snapshot.",
    "recover install authority",
)

# Shared restore helper becomes fallible and validates before building the actor.
s = replace_once(
    s,
    "    ) -> Actor {\n        let offsets: Vec<usize> = crate::runtime::spawn::bytecode_offsets_for(module, is_workflow);",
    "    ) -> Result<Actor, crate::authority_runtime::RuntimeAuthorityError> {\n"
    "        let authority_manifest =\n"
    "            crate::authority::AuthorityManifest::from_token_set(&snapshot.authority_tokens)?;\n"
    "        let offsets: Vec<usize> = crate::runtime::spawn::bytecode_offsets_for(module, is_workflow);",
    "restore helper result",
)
s = replace_once(
    s,
    "        actor.sequence = snapshot.sequence;\n        actor.waiting_signal = snapshot.waiting_signal.clone();\n        actor.bytecode_module = Some(module.clone());",
    "        actor.sequence = snapshot.sequence;\n"
    "        actor.waiting_signal = snapshot.waiting_signal.clone();\n"
    "        actor.install_authority_manifest(&authority_manifest);\n"
    "        actor.bytecode_module = Some(module.clone());",
    "restore helper install authority",
)
# The helper's final `actor` immediately precedes resolve_or_hydrate_grain docs.
s = replace_once(
    s,
    "        actor\n    }\n\n    /// Resolve a virtual actor (grain) identity to a resident actor id,",
    "        Ok(actor)\n    }\n\n    /// Resolve a virtual actor (grain) identity to a resident actor id,",
    "restore helper return",
)

# Grain hydration maps authority corruption to a normal runtime error before insertion.
s = replace_once(
    s,
    "            Self::restore_actor_from_snapshot(\n                stable_actor_id,\n                &grain_type.module,\n                snap,\n                false,\n                false,\n            )",
    "            Self::restore_actor_from_snapshot(\n"
    "                stable_actor_id,\n"
    "                &grain_type.module,\n"
    "                snap,\n"
    "                false,\n"
    "                false,\n"
    "            )\n"
    "            .map_err(|err| NuError::RuntimeError {\n"
    "                msg: format!(\n"
    "                    \"invalid authority snapshot for virtual actor {}: {}\",\n"
    "                    grain_id.actor_name(), err\n"
    "                ),\n"
    "                span: Span::new(0, 0),\n"
    "            })?",
    "grain authority restore",
)

# Migrated actors fail before recovery-module/CRDT/actor-map mutation.
s = replace_once(
    s,
    "        let actor =\n            Self::restore_actor_from_snapshot(actor_id, &module, &snapshot, is_workflow, is_agent);\n\n        // Register the recovery module.",
    "        let actor = match Self::restore_actor_from_snapshot(\n"
    "            actor_id,\n"
    "            &module,\n"
    "            &snapshot,\n"
    "            is_workflow,\n"
    "            is_agent,\n"
    "        ) {\n"
    "            Ok(actor) => actor,\n"
    "            Err(err) => {\n"
    "                warn!(\n"
    "                    \"nulang-migrate: invalid authority manifest for actor {}: {}\",\n"
    "                    actor_id, err\n"
    "                );\n"
    "                return false;\n"
    "            }\n"
    "        };\n\n"
    "        // Register the recovery module.",
    "migration authority restore",
)
write(p, s)

# ---------------------------------------------------------------------------
# Regression tests: restart, malformed recovery, migration, legacy snapshots.
# ---------------------------------------------------------------------------
p = "src/runtime/tests.rs"
s = read(p)
marker = "\n// ========================================================================\n// Core Runtime Tests\n// ========================================================================\n"
if marker not in s:
    raise SystemExit("runtime tests insertion marker missing")
tests = r'''

#[test]
fn test_authority_snapshot_round_trip_recovery() {
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_persistent_actor(Box::new(Vec::new), HashMap::new());
    let manifest = crate::authority::AuthorityManifest::from_tokens([
        "Secret::Read(PAYMENTS_KEY)",
        "Net::TcpOut(api.example.com:443)",
    ])
    .unwrap();
    rt.actors
        .get_mut(&actor_id)
        .unwrap()
        .install_authority_manifest(&manifest);

    rt.checkpoint_actor(actor_id);
    let snapshot = rt.persistence.load_snapshot(actor_id).unwrap();
    assert_eq!(snapshot.authority_tokens, manifest.canonical_token_set());

    rt.actors.remove(&actor_id);
    assert_eq!(rt.recover_actor(actor_id), Some(actor_id));
    let recovered = rt.actors.get(&actor_id).unwrap();
    assert_eq!(recovered.authority_manifest().unwrap(), manifest);
}

#[test]
fn test_malformed_authority_snapshot_fails_recovery_closed() {
    let mut rt = Runtime::new();
    let actor_id = 91_001;
    let mut snapshot = ActorSnapshot::default();
    snapshot.actor_id = actor_id;
    snapshot
        .authority_tokens
        .insert("Net::TcpOut(malformed)".to_string());
    rt.persistence.save_snapshot(snapshot).unwrap();

    assert_eq!(rt.recover_actor(actor_id), None);
    assert!(!rt.actors.contains_key(&actor_id));
}

#[test]
fn test_migration_preserves_authority_manifest() {
    let actor_id = 91_002;
    let module = CodeModule::new("authority-migration");
    let nbc = module.to_nbc(None).unwrap();
    let manifest = crate::authority::AuthorityManifest::from_tokens([
        "Fs::Read(/srv/input)",
        "Env::Read(REGION)",
    ])
    .unwrap();
    let snapshot = ActorSnapshot {
        actor_id,
        authority_tokens: manifest.canonical_token_set(),
        ..ActorSnapshot::default()
    };
    let json = serde_json::to_vec(&snapshot).unwrap();

    let mut rt = Runtime::new();
    assert!(rt.receive_migrated_actor(actor_id, nbc, json));
    let actor = rt.actors.get(&actor_id).unwrap();
    assert_eq!(actor.authority_manifest().unwrap(), manifest);
}

#[test]
fn test_migration_rejects_malformed_authority_before_insertion() {
    let actor_id = 91_003;
    let module = CodeModule::new("authority-migration-invalid");
    let nbc = module.to_nbc(None).unwrap();
    let mut snapshot = ActorSnapshot {
        actor_id,
        ..ActorSnapshot::default()
    };
    snapshot
        .authority_tokens
        .insert("Secret::Read(".to_string());
    let json = serde_json::to_vec(&snapshot).unwrap();

    let mut rt = Runtime::new();
    assert!(!rt.receive_migrated_actor(actor_id, nbc, json));
    assert!(!rt.actors.contains_key(&actor_id));
    assert!(!rt.recovery_modules.contains_key(&actor_id));
}

#[test]
fn test_legacy_snapshot_without_authority_is_deny_by_default() {
    let snapshot: ActorSnapshot = serde_json::from_str(
        r#"{"actor_id":91004,"sequence":7,"state":{},"waiting_signal":null,"crdt_snapshot":null,"crdt_field_map":null}"#,
    )
    .unwrap();
    assert!(snapshot.authority_tokens.is_empty());
}
'''
s = s.replace(marker, tests + marker, 1)
write(p, s)

# ---------------------------------------------------------------------------
# Keep all pre-existing explicit ActorSnapshot literals compiling. Production
# constructors above already carry real authority; tests/benches that are not
# exercising authority get an explicit empty default.
# ---------------------------------------------------------------------------

def add_default_authority_to_literals(path: Path):
    text = path.read_text()
    needle = "ActorSnapshot {"
    pos = 0
    edits = []
    while True:
        start = text.find(needle, pos)
        if start < 0:
            break
        prefix = text[max(0, start - 20):start]
        pos = start + len(needle)
        if re.search(r"struct\s+$", prefix):
            continue
        brace = text.find("{", start)
        depth = 0
        in_string = False
        escape = False
        close = None
        i = brace
        while i < len(text):
            ch = text[i]
            if in_string:
                if escape:
                    escape = False
                elif ch == "\\":
                    escape = True
                elif ch == '"':
                    in_string = False
            else:
                if ch == '"':
                    in_string = True
                elif ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                    if depth == 0:
                        close = i
                        break
            i += 1
        if close is None:
            raise SystemExit(f"{path}: unbalanced ActorSnapshot literal")
        block = text[start:close]
        if "authority_tokens" in block or "..ActorSnapshot::default()" in block or "..Default::default()" in block:
            pos = close + 1
            continue
        line_start = text.rfind("\n", 0, close) + 1
        indent = re.match(r"[ \t]*", text[line_start:close]).group(0)
        edits.append((close, f"authority_tokens: Default::default(),\n{indent}"))
        pos = close + 1
    for close, insertion in reversed(edits):
        text = text[:close] + insertion + text[close:]
    if edits:
        path.write_text(text)
    return len(edits)

literal_edits = {}
for path in Path(".").rglob("*.rs"):
    if any(part in {"target", ".git"} for part in path.parts):
        continue
    count = add_default_authority_to_literals(path)
    if count:
        literal_edits[str(path)] = count
print("updated ActorSnapshot literals:", literal_edits)

# Security assertions: every runtime construction/restoration path must mention
# the persisted authority field after the transform.
checks = {
    "src/runtime/persistence.rs": [
        "pub authority_tokens: BTreeSet<String>",
        "ALTER TABLE snapshots ADD COLUMN authority_tokens TEXT",
        "ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS authority_tokens TEXT",
        "authority_tokens=excluded.authority_tokens",
        "authority_tokens = EXCLUDED.authority_tokens",
    ],
    "src/runtime/workflow.rs": [
        "manifest.canonical_token_set()",
        "authority_tokens,",
    ],
    "src/runtime/mod.rs": [
        "invalid authority manifest",
        "actor.install_authority_manifest(&authority_manifest)",
        "invalid authority snapshot for virtual actor",
        "nulang-migrate: invalid authority manifest",
    ],
}
for path, needles in checks.items():
    text = read(path)
    for needle in needles:
        if needle not in text:
            raise SystemExit(f"{path}: missing required transformed text: {needle}")

print("authority snapshot preservation patch applied")
