#!/usr/bin/env python3
from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


def replace_all_checked(text: str, old: str, new: str, minimum: int, label: str) -> str:
    count = text.count(old)
    if count < minimum:
        raise SystemExit(f"{label}: expected at least {minimum} matches, found {count}")
    return text.replace(old, new)


# Every in-memory/JSON/RocksDB backend serializes ActorSnapshot wholesale.
# SQL backends store columns separately, so the nominal schema owner must be
# added explicitly to their table, save and load paths.
path = Path("src/runtime/persistence.rs")
text = path.read_text()

text = replace_once(
    text,
    '''                    waiting_signal TEXT,
                    crdt_snapshot TEXT,''',
    '''                    waiting_signal TEXT,
                    bytecode_schema_name TEXT,
                    crdt_snapshot TEXT,''',
    "libsql create schema column",
)
text = replace_once(
    text,
    '''            // Migrate databases created before the crdt_snapshot column existed.
            let _ = conn
                .execute("ALTER TABLE snapshots ADD COLUMN crdt_snapshot TEXT", ())
                .await;''',
    '''            // Migrate databases created before nominal actor schema identity.
            let _ = conn
                .execute(
                    "ALTER TABLE snapshots ADD COLUMN bytecode_schema_name TEXT",
                    (),
                )
                .await;
            // Migrate databases created before the crdt_snapshot column existed.
            let _ = conn
                .execute("ALTER TABLE snapshots ADD COLUMN crdt_snapshot TEXT", ())
                .await;''',
    "libsql alter schema column",
)
text = replace_once(
    text,
    '''                "INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(actor_id) DO UPDATE SET sequence=excluded.sequence, state=excluded.state, waiting_signal=excluded.waiting_signal, crdt_snapshot=excluded.crdt_snapshot, crdt_field_map=excluded.crdt_field_map, authority_tokens=excluded.authority_tokens",
                libsql::params![snapshot.actor_id as i64, snapshot.sequence as i64, state_json, snapshot.waiting_signal.as_deref(), crdt_json.as_str(), crdt_field_map_json.as_str(), authority_json.as_str()],''',
    '''                "INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, bytecode_schema_name, crdt_snapshot, crdt_field_map, authority_tokens) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(actor_id) DO UPDATE SET sequence=excluded.sequence, state=excluded.state, waiting_signal=excluded.waiting_signal, bytecode_schema_name=excluded.bytecode_schema_name, crdt_snapshot=excluded.crdt_snapshot, crdt_field_map=excluded.crdt_field_map, authority_tokens=excluded.authority_tokens",
                libsql::params![snapshot.actor_id as i64, snapshot.sequence as i64, state_json, snapshot.waiting_signal.as_deref(), snapshot.bytecode_schema_name.as_deref(), crdt_json.as_str(), crdt_field_map_json.as_str(), authority_json.as_str()],''',
    "libsql save schema",
)
text = replace_once(
    text,
    '''                    "SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens FROM snapshots WHERE actor_id = ?1",''',
    '''                    "SELECT sequence, state, waiting_signal, bytecode_schema_name, crdt_snapshot, crdt_field_map, authority_tokens FROM snapshots WHERE actor_id = ?1",''',
    "libsql load query",
)
text = replace_once(
    text,
    '''            let waiting_signal: Option<String> = row.get(2).ok()?;
            let crdt_json: Option<String> = row.get(3).ok()?;
            let crdt_field_map_json: Option<String> = row.get(4).ok()?;
            let authority_json: Option<String> = row.get(5).ok()?;''',
    '''            let waiting_signal: Option<String> = row.get(2).ok()?;
            let bytecode_schema_name: Option<String> = row.get(3).ok()?;
            let crdt_json: Option<String> = row.get(4).ok()?;
            let crdt_field_map_json: Option<String> = row.get(5).ok()?;
            let authority_json: Option<String> = row.get(6).ok()?;''',
    "libsql load columns",
)
text = replace_once(
    text,
    '''                waiting_signal,
                crdt_snapshot,
                crdt_field_map,
                authority_tokens,
            })''',
    '''                waiting_signal,
                bytecode_schema_name,
                crdt_snapshot,
                crdt_field_map,
                authority_tokens,
            })''',
    "libsql snapshot reconstruction",
)

text = replace_once(
    text,
    '''                waiting_signal TEXT,
                crdt_snapshot TEXT,
                crdt_field_map TEXT,
                authority_tokens TEXT''',
    '''                waiting_signal TEXT,
                bytecode_schema_name TEXT,
                crdt_snapshot TEXT,
                crdt_field_map TEXT,
                authority_tokens TEXT''',
    "postgres create schema column",
)
text = replace_once(
    text,
    '''        conn.execute(
            "ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS authority_tokens TEXT",
            &[],
        )''',
    '''        conn.execute(
            "ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS bytecode_schema_name TEXT",
            &[],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        conn.execute(
            "ALTER TABLE snapshots ADD COLUMN IF NOT EXISTS authority_tokens TEXT",
            &[],
        )''',
    "postgres alter schema column",
)
text = replace_once(
    text,
    '''            "INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (actor_id) DO UPDATE SET
               sequence = EXCLUDED.sequence,
               state = EXCLUDED.state,
               waiting_signal = EXCLUDED.waiting_signal,
               crdt_snapshot = EXCLUDED.crdt_snapshot,
               crdt_field_map = EXCLUDED.crdt_field_map,
               authority_tokens = EXCLUDED.authority_tokens",
            &[
                &(snapshot.actor_id as i64),
                &(snapshot.sequence as i64),
                &state_json,
                &snapshot.waiting_signal.as_deref(),
                &crdt_json.as_str(),
                &crdt_field_map_json.as_str(),
                &authority_json.as_str(),
            ],''',
    '''            "INSERT INTO snapshots (actor_id, sequence, state, waiting_signal, bytecode_schema_name, crdt_snapshot, crdt_field_map, authority_tokens)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (actor_id) DO UPDATE SET
               sequence = EXCLUDED.sequence,
               state = EXCLUDED.state,
               waiting_signal = EXCLUDED.waiting_signal,
               bytecode_schema_name = EXCLUDED.bytecode_schema_name,
               crdt_snapshot = EXCLUDED.crdt_snapshot,
               crdt_field_map = EXCLUDED.crdt_field_map,
               authority_tokens = EXCLUDED.authority_tokens",
            &[
                &(snapshot.actor_id as i64),
                &(snapshot.sequence as i64),
                &state_json,
                &snapshot.waiting_signal.as_deref(),
                &snapshot.bytecode_schema_name.as_deref(),
                &crdt_json.as_str(),
                &crdt_field_map_json.as_str(),
                &authority_json.as_str(),
            ],''',
    "postgres save schema",
)
text = replace_once(
    text,
    '''                "SELECT sequence, state, waiting_signal, crdt_snapshot, crdt_field_map, authority_tokens
                 FROM snapshots WHERE actor_id = $1",''',
    '''                "SELECT sequence, state, waiting_signal, bytecode_schema_name, crdt_snapshot, crdt_field_map, authority_tokens
                 FROM snapshots WHERE actor_id = $1",''',
    "postgres load query",
)
text = replace_once(
    text,
    '''        let waiting_signal: Option<String> = row.get(2);
        let crdt_json: Option<String> = row.get(3);
        let crdt_field_map_json: Option<String> = row.get(4);
        let authority_json: Option<String> = row.get(5);''',
    '''        let waiting_signal: Option<String> = row.get(2);
        let bytecode_schema_name: Option<String> = row.get(3);
        let crdt_json: Option<String> = row.get(4);
        let crdt_field_map_json: Option<String> = row.get(5);
        let authority_json: Option<String> = row.get(6);''',
    "postgres load columns",
)
text = replace_once(
    text,
    '''            waiting_signal,
            crdt_snapshot,
            crdt_field_map,
            authority_tokens,
        })''',
    '''            waiting_signal,
            bytecode_schema_name,
            crdt_snapshot,
            crdt_field_map,
            authority_tokens,
        })''',
    "postgres snapshot reconstruction",
)

# Persistence unit-test literals that spell every field explicitly.
text = replace_all_checked(
    text,
    '''                waiting_signal: None,
                crdt_snapshot: None,''',
    '''                waiting_signal: None,
                bytecode_schema_name: None,
                crdt_snapshot: None,''',
    2,
    "persistence test snapshot literals",
)
path.write_text(text)

# Migration/shadow snapshot builders must carry the owner too.
for filename in ["src/runtime/mod.rs", "src/runtime/callbacks.rs"]:
    path = Path(filename)
    text = path.read_text()
    old = "waiting_signal: actor.waiting_signal.clone(),\n"
    new = old + "            bytecode_schema_name: actor.bytecode_schema_name.clone(),\n"
    if filename.endswith("callbacks.rs"):
        new = old + "                    bytecode_schema_name: actor.bytecode_schema_name.clone(),\n"
    count = text.count(old)
    if count < 1:
        raise SystemExit(f"{filename} snapshot builder: no waiting_signal marker")
    text = text.replace(old, new)
    path.write_text(text)

# Runtime test literals with complete field lists.
path = Path("src/runtime/tests.rs")
text = path.read_text()
text = replace_all_checked(
    text,
    '''        waiting_signal: None,
        crdt_snapshot: None,''',
    '''        waiting_signal: None,
        bytecode_schema_name: None,
        crdt_snapshot: None,''',
    2,
    "runtime test snapshot literals",
)
path.write_text(text)
