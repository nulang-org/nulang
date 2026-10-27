#!/usr/bin/env python3
"""Collect Nulang's feature-profile dependency closure and existing binary size.

This is diagnostic-only: never runs a build, upgrades a dependency, or changes
Cargo.lock. Build durations are provided by an already executed CI build step.
"""

import argparse
from collections import deque
import json
from pathlib import Path
import subprocess
import sys

PROFILES = {
    "minimal": (),
    "release-cli": ("release-cli",),
    "full": ("full",),
}


def summarize(data):
    """Count normal/build packages reachable from the root Nulang package."""
    packages = data.get("packages")
    resolved = data.get("resolve")
    if not isinstance(packages, list) or not isinstance(resolved, dict):
        raise ValueError("Cargo metadata missing packages/resolve")
    nodes = resolved.get("nodes")
    if not isinstance(nodes, list):
        raise ValueError("Cargo metadata missing resolve.nodes")
    by_id = {item["id"]: item for item in packages}
    by_node = {item["id"]: item for item in nodes}
    roots = [pid for pid, p in by_id.items() if p.get("name") == "nulang"]
    if len(roots) != 1:
        raise ValueError(f"Expected one nulang root package, got {len(roots)}")
    root = roots[0]
    seen = {root}
    queue = deque([root])
    direct = set()
    while queue:
        current = queue.popleft()
        if current not in by_id or current not in by_node:
            raise ValueError(f"Incomplete resolved graph at {current}")
        deps = by_node[current].get("deps")
        if not isinstance(deps, list):
            raise ValueError(f"Missing resolved dependency list at {current}")
        for dep in deps:
            kinds = dep.get("dep_kinds")
            target = dep.get("pkg")
            if not target or not isinstance(kinds, list) or not kinds:
                raise ValueError(f"Incomplete resolved edge at {current}")
            if not all(isinstance(kind, dict) and kind.get("kind") in (None, "build", "dev") for kind in kinds):
                raise ValueError(f"Unknown Cargo dependency kind at {current}")
            if not any(kind["kind"] in (None, "build") for kind in kinds):
                continue
            if current == root:
                direct.add(target)
            if target not in by_id or target not in by_node:
                raise ValueError(f"Incomplete resolved graph at {target}")
            if target not in seen:
                seen.add(target)
                queue.append(target)
    dependency_packages = [by_id[pid] for pid in seen]
    return {
        "resolved_packages": len(seen),
        "direct_packages": len(direct),
        "package_names": sorted(p["name"] for p in dependency_packages),
        "packages": sorted(
            ({"name": p["name"], "version": p["version"]} for p in dependency_packages),
            key=lambda p: (p["name"], p["version"]),
        ),
    }


def metadata(root, profile, target):
    if profile not in PROFILES:
        raise ValueError(f"Unknown profile {profile!r}")
    command = [
        "cargo", "metadata", "--locked", "--format-version", "1",
        "--no-default-features", "--filter-platform", target,
    ]
    if PROFILES[profile]:
        command.extend(["--features", ",".join(PROFILES[profile])])
    result = subprocess.run(command, cwd=root, text=True, capture_output=True, check=True)
    return json.loads(result.stdout)


def rust_target():
    result = subprocess.run(["rustc", "-vV"], text=True, capture_output=True, check=True)
    for line in result.stdout.splitlines():
        if line.startswith("host: "):
            return line.partition(": ")[2]
    raise ValueError("rustc did not report a host triple")


def binary_size(path):
    try:
        return path.stat().st_size
    except FileNotFoundError:
        return None


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", action="append", required=True, choices=sorted(PROFILES))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--target", help="Rust target triple (defaults to rustc host)")
    parser.add_argument("--binary", type=Path, help="Existing compiled executable to size")
    parser.add_argument("--build-seconds", type=float, help="Observed elapsed CI build seconds")
    args = parser.parse_args(argv)
    if args.build_seconds is not None and args.build_seconds < 0:
        parser.error("--build-seconds must be nonnegative")
    try:
        target = args.target or rust_target()
        report = {
            "schema_version": 1,
            "target_triple": target,
            "measurement": "target-filtered activated normal/build Cargo dependency graph",
            "caveat": "Not a compiled-link or cold-build benchmark; cached CI time is descriptive only",
            "profiles": {},
        }
        for profile in dict.fromkeys(args.profile):
            summary = summarize(metadata(args.root, profile, target))
            summary["features"] = list(PROFILES[profile])
            report["profiles"][profile] = summary
            print(f"{profile}: {summary['resolved_packages']} resolved packages; {summary['direct_packages']} direct packages")
        if args.binary is not None:
            report["binary_bytes"] = binary_size(args.binary)
            if report["binary_bytes"] is None:
                print(f"WARN: binary not found: {args.binary}; size unknown", file=sys.stderr)
        if args.build_seconds is not None:
            report["cached_ci_build_seconds"] = args.build_seconds
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(f"Profile report: {args.output}")
        return 0
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError, json.JSONDecodeError) as exc:
        details = exc.stderr.strip() if isinstance(exc, subprocess.CalledProcessError) and exc.stderr else str(exc)
        print(f"ERROR: profile measurement failed closed: {details}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
