# Actor schema upgrade preflight (Experimental)

Nulang can compare two **compiler-derived** `ActorStateSchema` values
without parsing source in the deployment layer or reading untrusted manifests
as authoritative source declarations.

The additive API is `schema_upgrade::analyze_actor_schema_upgrade(before, after)`.
It returns an `ActorSchemaUpgradeReport` containing:

- `same_canonical_schema`: equality under the existing compiler-owned canonical
  actor schema encoding. Declaration order is ignored.
- `differences`: deterministic actor identity, version, field-addition,
  field-removal, and field-type diagnostics.
- `migration_path`: whether the **new** schema declares an unambiguous
  contiguous version chain from the previously persisted schema.

## Safety and compatibility limits

**This is evidence, not authorization.**

`DeclaredMigrationPath::Complete` says only that migration *metadata* names
exactly one step from each version `v` to `v + 1`. It does **not** prove
migration purity, idempotence, effect replay safety, compatibility of behavior
protocols, historical code availability, or a successful atomic commit.

The runtime must continue to validate canonical semantic/artifact/protocol
identity, migration body semantics, activation fencing, and persisted history
before permitting recovery. A renamed actor is not automatically the same
actor even if its field layouts match. A type change cannot be accepted only
because it has a declared version step.

This API does not change persistent formats, bytecode, the NUL0 protocol,
the runtime's recovery policy, or existing source syntax.

## Validation

```sh
cargo test --locked --test actor_schema_upgrade_preflight
cargo test --locked --lib semantic_schema
cargo fmt --all -- --check
```

An exact-head full CI result is required before this experimental preflight
is promoted. Because it is not wired into runtime admission, the tests do
not establish complete durable-actor crash safety.
