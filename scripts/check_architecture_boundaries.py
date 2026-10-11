#!/usr/bin/env python3
"""Enforce repository architecture boundaries that must not regress.

Nulang owns portable computation semantics. Hosted resource placement and
reconciliation policy belongs to Nulang Cloud, where `nlc-placement` is the
sole authoritative hosted control plane. The historical
`crates/nulang-cloud-control` crate is retained only as a frozen migration
surface; it must not return to Nulang's active workspace or dependency graph.
"""

from __future__ import annotations

import argparse
import pathlib
import sys
import tomllib
from collections.abc import Iterator, Mapping
from typing import Any

FORBIDDEN_CLOUD_CONTROL_PACKAGE = "nulang-cloud-control"
FORBIDDEN_CLOUD_CONTROL_MEMBER = pathlib.PurePosixPath("crates/nulang-cloud-control")
DEPENDENCY_TABLE_NAMES = {"dependencies", "dev-dependencies", "build-dependencies"}


def _load_toml(path: pathlib.Path) -> dict[str, Any]:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def _dependency_tables(
    node: Mapping[str, Any], prefix: tuple[str, ...] = ()
) -> Iterator[tuple[str, Mapping[str, Any]]]:
    """Yield Cargo dependency tables, including workspace/target-specific ones."""
    for key, value in node.items():
        if not isinstance(value, Mapping):
            continue
        next_prefix = (*prefix, key)
        if key in DEPENDENCY_TABLE_NAMES:
            yield ".".join(next_prefix), value
        yield from _dependency_tables(value, next_prefix)


def _dependency_package(alias: str, spec: Any) -> str:
    if isinstance(spec, Mapping):
        package = spec.get("package")
        if isinstance(package, str):
            return package
    return alias


def _normalized_member_path(member: str) -> pathlib.PurePosixPath:
    normalized = pathlib.PurePosixPath(member)
    while normalized.parts and normalized.parts[0] == ".":
        normalized = pathlib.PurePosixPath(*normalized.parts[1:])
    return normalized


def _workspace_manifest_paths(
    root: pathlib.Path, root_manifest: Mapping[str, Any]
) -> tuple[list[pathlib.Path], list[str]]:
    errors: list[str] = []
    workspace = root_manifest.get("workspace", {})
    members = workspace.get("members", []) if isinstance(workspace, Mapping) else []
    if not isinstance(members, list):
        return [], ["Cargo.toml [workspace].members must be an array"]

    manifests: list[pathlib.Path] = []
    for raw_member in members:
        if not isinstance(raw_member, str):
            errors.append("Cargo.toml [workspace].members entries must be strings")
            continue

        member = _normalized_member_path(raw_member)
        if member == FORBIDDEN_CLOUD_CONTROL_MEMBER:
            errors.append(
                f"{FORBIDDEN_CLOUD_CONTROL_MEMBER} must not be an active workspace member; "
                "hosted placement/reconciliation authority belongs to Nulang Cloud nlc-placement"
            )

        # Current Nulang uses explicit members. Support Cargo-style glob members
        # as well so this guard cannot be bypassed by replacing the explicit path
        # with a broader pattern such as crates/*.
        pattern = member.as_posix()
        if any(char in pattern for char in "*?["):
            candidates = sorted(path for path in root.glob(pattern) if path.is_dir())
        else:
            candidates = [root / pathlib.Path(*member.parts)]

        for candidate in candidates:
            try:
                relative = pathlib.PurePosixPath(candidate.relative_to(root).as_posix())
            except ValueError:
                errors.append(f"workspace member escapes repository root: {raw_member}")
                continue

            if relative == FORBIDDEN_CLOUD_CONTROL_MEMBER:
                errors.append(
                    f"{FORBIDDEN_CLOUD_CONTROL_MEMBER} must not be included by workspace member pattern {raw_member!r}"
                )

            manifest = candidate / "Cargo.toml"
            if manifest.exists():
                manifests.append(manifest)
            else:
                errors.append(f"workspace member {raw_member!r} has no Cargo.toml at {manifest}")

    # Preserve deterministic diagnostics and avoid scanning the same manifest
    # twice when overlapping workspace patterns resolve to one crate.
    return sorted(set(manifests)), errors


def _check_manifest_dependencies(
    manifest_path: pathlib.Path, manifest: Mapping[str, Any], root: pathlib.Path
) -> list[str]:
    errors: list[str] = []
    try:
        display_path = manifest_path.relative_to(root).as_posix()
    except ValueError:
        display_path = str(manifest_path)

    for table_name, dependencies in _dependency_tables(manifest):
        for alias, spec in dependencies.items():
            package = _dependency_package(alias, spec)
            if package == FORBIDDEN_CLOUD_CONTROL_PACKAGE:
                errors.append(
                    f"{display_path} [{table_name}] depends on forbidden package "
                    f"{FORBIDDEN_CLOUD_CONTROL_PACKAGE!s} via {alias!r}; "
                    "hosted control-plane policy must remain in Nulang Cloud"
                )
    return errors


def validate_repository(root: pathlib.Path) -> list[str]:
    root = root.resolve()
    root_cargo = root / "Cargo.toml"
    if not root_cargo.exists():
        return [f"repository root has no Cargo.toml: {root_cargo}"]

    try:
        root_manifest = _load_toml(root_cargo)
    except (OSError, tomllib.TOMLDecodeError) as exc:
        return [f"cannot parse {root_cargo}: {exc}"]

    member_manifests, errors = _workspace_manifest_paths(root, root_manifest)

    # The root package may itself have dependencies, and [workspace.dependencies]
    # is also authoritative for member crates using `workspace = true`.
    errors.extend(_check_manifest_dependencies(root_cargo, root_manifest, root))

    for manifest_path in member_manifests:
        try:
            manifest = _load_toml(manifest_path)
        except (OSError, tomllib.TOMLDecodeError) as exc:
            errors.append(f"cannot parse {manifest_path}: {exc}")
            continue
        errors.extend(_check_manifest_dependencies(manifest_path, manifest, root))

    return errors


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify Nulang architecture ownership boundaries.")
    parser.add_argument(
        "--root",
        type=pathlib.Path,
        default=pathlib.Path.cwd(),
        help="Repository root containing Cargo.toml (default: current directory).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    errors = validate_repository(args.root)
    if errors:
        print("Architecture boundary validation failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1

    print("Success: architecture ownership boundaries are intact.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
