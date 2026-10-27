#!/usr/bin/env python3
"""Enforce Nulang's minimal-profile dependency boundary using resolved Cargo metadata.

Only activated normal/build dependencies reachable from the nulang package
count. Unrelated workspace crates and dev-only fixtures are not runtime deps.
"""

import argparse
from collections import deque
import json
from pathlib import Path
import subprocess
import sys

# These optional integration families must not enter the minimal Rust build.
# Changes require a reviewed architecture decision, not a silent exemption.
FORBIDDEN_EXACT = frozenset({
    "libsql", "rocksdb", "postgres", "pyo3", "tokio", "tower-lsp",
    "reqwest", "ureq", "mio", "libffi", "libloading", "wasmtime",
    "wasm-encoder", "wat", "opentelemetry", "opentelemetry-otlp",
    "opentelemetry_sdk", "tracing-opentelemetry", "borsh",
})
FORBIDDEN_PREFIXES = ("cranelift", "nulang-ai")


def is_forbidden(name):
    return name in FORBIDDEN_EXACT or name.startswith(FORBIDDEN_PREFIXES)


def load_metadata(root):
    command = [
        "cargo", "metadata", "--locked", "--format-version", "1",
        "--package", "nulang", "--no-default-features",
    ]
    result = subprocess.run(
        command, cwd=root, text=True, capture_output=True, check=True,
    )
    return json.loads(result.stdout)


def runtime_edges(node):
    """Include normal/build graph edges; omit dev-only edges."""
    deps = node.get("deps")
    if not isinstance(deps, list):
        raise ValueError("Cargo metadata node lacks deps (format-version 1 required)")
    for dep in deps:
        package_id = dep.get("pkg")
        kinds = dep.get("dep_kinds")
        if not package_id or not isinstance(kinds, list):
            raise ValueError("Cargo metadata dependency missing pkg/dep_kinds")
        if any(item.get("kind") in (None, "build") for item in kinds):
            yield package_id


def inspect_core(metadata):
    """Return (reachable_package_names, violations_with_paths); error if incomplete."""
    packages = metadata.get("packages")
    resolve = metadata.get("resolve")
    if not isinstance(packages, list) or not isinstance(resolve, dict):
        raise ValueError("Cargo metadata lacks packages/resolve")
    nodes = resolve.get("nodes")
    if not isinstance(nodes, list):
        raise ValueError("Cargo metadata lacks resolve.nodes")
    names = {package["id"]: package["name"] for package in packages}
    roots = [pid for pid, name in names.items() if name == "nulang"]
    if len(roots) != 1:
        raise ValueError(f"Expected exactly one nulang package, found {len(roots)}")
    by_id = {node["id"]: node for node in nodes}
    root = roots[0]
    parent = {root: None}
    pending = deque([root])
    while pending:
        current = pending.popleft()
        if current not in by_id or current not in names:
            raise ValueError(f"Incomplete resolved graph at {current}")
        for dependency in runtime_edges(by_id[current]):
            if dependency not in names or dependency not in by_id:
                raise ValueError(f"Missing resolved dependency {dependency}")
            if dependency not in parent:
                parent[dependency] = current
                pending.append(dependency)
    violations = []
    for pid in parent:
        if is_forbidden(names[pid]):
            path = []
            position = pid
            while position is not None:
                path.append(names[position])
                position = parent[position]
            violations.append(" -> ".join(reversed(path)))
    return sorted(names[pid] for pid in parent), sorted(violations)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--metadata-file", type=Path, help="Read captured Cargo metadata")
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args(argv)
    try:
        metadata = (
            json.loads(args.metadata_file.read_text(encoding="utf-8"))
            if args.metadata_file else load_metadata(args.root)
        )
        package_names, violations = inspect_core(metadata)
        print(f"minimal core dependency closure: {len(package_names)} packages")
        if violations:
            print("ERROR: optional subsystems reached from minimal core:", file=sys.stderr)
            for path in violations:
                print(f"  {path}", file=sys.stderr)
            return 1
        print("core boundary: PASS (no forbidden optional subsystems)")
        return 0
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as exc:
        print(f"ERROR: could not verify minimal core dependency boundary: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
