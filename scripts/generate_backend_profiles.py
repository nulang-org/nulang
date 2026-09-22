#!/usr/bin/env python3
"""Generate and validate the backend-profile documentation.

The JSON manifest is the machine-readable source of truth for execution-profile
roles and known semantic restrictions. The generated Markdown is intentionally
small: detailed implementation notes remain in backend-specific docs/tests.
"""

import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "spec" / "backend-profiles" / "v0alpha1.json"
DOC = ROOT / "docs" / "BACKEND_PROFILES.md"

EXPECTED_IDS = {"bytecode", "jit", "wasm", "wasmfx", "native"}


def load_manifest():
    data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    if data.get("schema") != "nulang.backend-profiles/v0alpha1":
        raise SystemExit("backend profile manifest has an unknown schema")
    profiles = data.get("profiles")
    if not isinstance(profiles, list):
        raise SystemExit("backend profile manifest must contain a profiles array")
    ids = [p.get("id") for p in profiles]
    if len(ids) != len(set(ids)):
        raise SystemExit("backend profile ids must be unique")
    if set(ids) != EXPECTED_IDS:
        raise SystemExit(f"backend profile ids must be exactly {sorted(EXPECTED_IDS)}")
    if data.get("semantic_reference") != "bytecode":
        raise SystemExit("bytecode must remain the semantic reference")
    if data.get("canonical_portable_target") != "wasm":
        raise SystemExit("wasm must remain the canonical portable target")

    by_id = {p["id"]: p for p in profiles}
    if by_id["jit"].get("fallback") != "bytecode":
        raise SystemExit("jit must explicitly fall back to bytecode")
    for profile in profiles:
        restrictions = profile.get("restrictions", [])
        if not isinstance(restrictions, list) or len(restrictions) != len(set(restrictions)):
            raise SystemExit(f"{profile['id']}: restrictions must be a unique array")
    return data


def verify_code_evidence(data):
    by_id = {p["id"]: p for p in data["profiles"]}
    checks = [
        (
            ROOT / "src" / "backends" / "mod.rs",
            "WASM backend restricted profile: user-defined effect handlers and continuation resume are not supported",
            by_id["wasm"]["restrictions"],
            {"user-defined-effect-handlers", "continuation-resume"},
            "plain WASM restriction validator",
        ),
        (
            ROOT / "src" / "wasmfx_backend.rs",
            "WasmFX backend restricted profile: user-defined effect handlers and continuation resume are not supported yet",
            by_id["wasmfx"]["restrictions"],
            {"user-defined-effect-handlers", "continuation-resume"},
            "WasmFX restriction validator",
        ),
        (
            ROOT / "src" / "aot" / "codegen.rs",
            "Resume: effect-continuation resume requires the bytecode backend",
            by_id["native"]["restrictions"],
            {"continuation-resume"},
            "native AOT resume rejection",
        ),
    ]
    for path, needle, restrictions, required, label in checks:
        content = path.read_text(encoding="utf-8")
        if needle not in content:
            raise SystemExit(f"{label} evidence is missing from {path.relative_to(ROOT)}")
        if not required.issubset(set(restrictions)):
            raise SystemExit(f"{label} is not represented in the backend manifest")


def render(data):
    rows = []
    for p in data["profiles"]:
        restrictions = ", ".join(f"`{x}`" for x in p.get("restrictions", [])) or "None"
        fallback = f"`{p['fallback']}`" if p.get("fallback") else "—"
        rows.append(
            f"| `{p['id']}` | {p['maturity']} | {p['role']} | {fallback} | {restrictions} |"
        )
    return (
        "# Backend Profiles\n\n"
        "> Generated from `spec/backend-profiles/v0alpha1.json`. "
        "Do not edit this table by hand.\n\n"
        f"Semantic reference: **{data['semantic_reference']}**.  \n"
        f"Canonical portable/cloud target: **{data['canonical_portable_target']}**.\n\n"
        "| Backend | Maturity | Role | Fallback | Known semantic restrictions |\n"
        "|---|---|---|---|---|\n"
        + "\n".join(rows)
        + "\n\n"
        "A restricted backend must reject unsupported semantics explicitly; it must never "
        "silently reinterpret them. JIT is an optimization profile and may fall back to "
        "the bytecode interpreter for unsupported hot regions.\n"
    )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    data = load_manifest()
    verify_code_evidence(data)
    expected = render(data)
    if args.check:
        if not DOC.exists() or DOC.read_text(encoding="utf-8") != expected:
            raise SystemExit(
                "generated backend profile documentation is stale; "
                "run scripts/generate_backend_profiles.py"
            )
        print("Backend profile manifest and generated docs are consistent.")
        return
    DOC.write_text(expected, encoding="utf-8")
    print(f"Wrote {DOC.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
