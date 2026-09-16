#!/usr/bin/env python3
from pathlib import Path
import runpy


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


# The original storage patch correctly updates ActorSnapshot's SQL persistence
# surfaces and explicit persistence-test literals before reaching a marker that
# assumed runtime/mod.rs constructs snapshots exactly like callbacks.rs. That
# assumption is false: Runtime::build_actor_snapshot first captures
# waiting_signal into a local. Preserve the already-applied backend edits, catch
# only that known marker failure, and finish the remaining snapshot builders
# using their actual shapes.
try:
    runpy.run_path("scripts/apply_p0_actor_snapshot_storage.py", run_name="__main__")
except SystemExit as exc:
    expected = "src/runtime/mod.rs snapshot builder: no waiting_signal marker"
    if str(exc) != expected:
        raise
else:
    raise SystemExit(
        "snapshot storage v2: legacy storage patch unexpectedly completed; "
        "remove this compatibility wrapper instead of double-patching"
    )

# Runtime::build_actor_snapshot borrows the actor once to capture durable
# fields, waiting_signal, and authority. Capture nominal bytecode ownership in
# the same borrow so the subsequent ActorSnapshot construction does not need a
# second actor lookup and remains valid after migration/shadow serialization.
path = Path("src/runtime/mod.rs")
text = path.read_text()
text = replace_once(
    text,
    "let (waiting_signal, authority_tokens) = {",
    "let (waiting_signal, bytecode_schema_name, authority_tokens) = {",
    "runtime snapshot capture tuple",
)
text = replace_once(
    text,
    "(actor.waiting_signal.clone(), authority_tokens)",
    "(\n                actor.waiting_signal.clone(),\n                actor.bytecode_schema_name.clone(),\n                authority_tokens,\n            )",
    "runtime snapshot captured values",
)
text = replace_once(
    text,
    '''            state,
            waiting_signal,
            crdt_snapshot,''',
    '''            state,
            waiting_signal,
            bytecode_schema_name,
            crdt_snapshot,''',
    "runtime snapshot schema field",
)
path.write_text(text)

# Migration callback snapshot: actor is still directly borrowed here.
path = Path("src/runtime/callbacks.rs")
text = path.read_text()
text = replace_all_checked(
    text,
    "                    waiting_signal: actor.waiting_signal.clone(),\n",
    "                    waiting_signal: actor.waiting_signal.clone(),\n"
    "                    bytecode_schema_name: actor.bytecode_schema_name.clone(),\n",
    1,
    "callback migration snapshot schema",
)
path.write_text(text)

# Runtime tests have both hand-built migration snapshots and complete literal
# fixtures. Keep them compiling and make the migration fixture exercise schema
# propagation rather than silently dropping it.
path = Path("src/runtime/tests.rs")
text = path.read_text()
text = replace_all_checked(
    text,
    "            waiting_signal: actor.waiting_signal.clone(),\n",
    "            waiting_signal: actor.waiting_signal.clone(),\n"
    "            bytecode_schema_name: actor.bytecode_schema_name.clone(),\n",
    1,
    "runtime migration test snapshot schema",
)
text = replace_all_checked(
    text,
    '''        waiting_signal: None,
        crdt_snapshot: None,''',
    '''        waiting_signal: None,
        bytecode_schema_name: None,
        crdt_snapshot: None,''',
    2,
    "runtime explicit snapshot literals",
)
path.write_text(text)
