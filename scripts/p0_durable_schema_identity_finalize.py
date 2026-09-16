from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one exact match, found {count}")
    p.write_text(text.replace(old, new, 1))


# #316 owns behavior_ownership as an internal runtime module. Durable schema
# identity should live beside it instead of keeping temporary root-level shims.
replace_once(
    "src/runtime/mod.rs",
    "mod behavior_ownership;\nmod gc;\n",
    "mod behavior_ownership;\nmod schema_identity;\nmod gc;\n",
    "runtime schema module declaration",
)

schema_path = Path("src/runtime/schema_identity.rs")
schema_text = schema_path.read_text()
old = "crate::runtime_behavior_ownership::"
if schema_text.count(old) != 1:
    raise SystemExit(
        f"schema identity ownership path: expected one root shim reference, found {schema_text.count(old)}"
    )
schema_path.write_text(schema_text.replace(old, "super::behavior_ownership::", 1))

mod_path = Path("src/runtime/mod.rs")
mod_text = mod_path.read_text()
behavior_root = "crate::runtime_behavior_ownership::"
behavior_count = mod_text.count(behavior_root)
if behavior_count < 1:
    raise SystemExit("runtime integration contains no root ownership shim references to normalize")
mod_text = mod_text.replace(behavior_root, "behavior_ownership::")
schema_root = "crate::runtime_schema_identity::"
schema_count = mod_text.count(schema_root)
if schema_count < 1:
    raise SystemExit("runtime integration contains no root schema shim references to normalize")
mod_text = mod_text.replace(schema_root, "schema_identity::")
mod_path.write_text(mod_text)

for path_name in ("src/runtime/workflow.rs", "src/runtime/callbacks.rs"):
    path = Path(path_name)
    text = path.read_text()
    count = text.count(schema_root)
    if count != 1:
        raise SystemExit(
            f"{path_name}: expected one root schema shim reference, found {count}"
        )
    path.write_text(text.replace(schema_root, "super::schema_identity::", 1))

# Parent-cleaned src/lib.rs must not reintroduce either temporary root module.
lib = Path("src/lib.rs").read_text()
for forbidden in ("runtime_behavior_ownership", "runtime_schema_identity"):
    if forbidden in lib:
        raise SystemExit(f"src/lib.rs unexpectedly retains temporary {forbidden} shim")
