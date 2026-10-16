#!/usr/bin/env python3
"""Collect revision-pinned change-amplification evidence for Nulang/Go/Rust.

This is an observational maintenance study, not a language ranking. It does not
run arbitrary commands from manifests or claim semantic/task equivalence.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

SCHEMA_VERSION = 1
LANGUAGES = frozenset({"nulang", "rust", "go"})
DEPENDENCY_MANIFESTS = frozenset({
    "Cargo.toml", "Cargo.lock", "go.mod", "go.sum", "go.work", "go.work.sum",
    "Nulang.toml", "Nulang.lock",
})
SHA_RE = re.compile(r"[0-9a-fA-F]{40}\Z")


def _git(repo: Path, *args: str) -> bytes:
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    if proc.returncode:
        detail = proc.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(f"git {args[0]} failed in {repo}: {detail}")
    return proc.stdout


def _check_commit(repo: Path, sha: str) -> None:
    if not isinstance(sha, str) or not SHA_RE.fullmatch(sha):
        raise ValueError("revisions must use full 40-character commit SHAs")
    try:
        kind = _git(repo, "cat-file", "-t", sha).decode("ascii").strip()
    except ValueError as exc:
        raise ValueError(f"revision {sha} is not a commit in repository {repo}") from exc
    if kind != "commit":
        raise ValueError(f"revision {sha} is not a commit in repository {repo}")


def _numstat_records(data: bytes):
    """Parse git diff --numstat -z, including NUL-prefixed rename path pairs."""
    fields = data.split(b"\0")
    if fields[-1:] == [b""]:
        fields.pop()
    i = 0
    while i < len(fields):
        entry = fields[i]
        i += 1
        parts = entry.split(b"\t", 2)
        if len(parts) != 3:
            raise ValueError("malformed git diff numstat record")
        added, deleted, path = parts
        if not path:  # --find-renames encodes old and new paths after the header
            if i + 1 >= len(fields):
                raise ValueError("incomplete git diff rename record")
            old, path = fields[i], fields[i + 1]
            i += 2
            if not old or not path:
                raise ValueError("empty git diff rename path")
        yield (
            added.decode("ascii"), deleted.decode("ascii"),
            path.decode("utf-8", errors="surrogateescape"),
        )


def measure_change(repo: Path, base_sha: str, head_sha: str) -> dict[str, Any]:
    repo = Path(repo).resolve()
    if not repo.is_dir():
        raise ValueError(f"repository does not exist: {repo}")
    try:
        root = Path(_git(repo, "rev-parse", "--show-toplevel").decode().strip()).resolve()
    except ValueError as exc:
        raise ValueError(f"not a git repository: {repo}") from exc
    if root != repo:
        raise ValueError(f"repository must name git root, got {repo} instead of {root}")
    _check_commit(repo, base_sha)
    _check_commit(repo, head_sha)
    data = _git(
        repo, "diff", "--numstat", "-z", "--find-renames",
        "--no-ext-diff", "--no-textconv", base_sha, head_sha, "--",
    )
    lines_added = 0
    lines_deleted = 0
    changed_paths: list[str] = []
    binary_files: list[str] = []
    for added, deleted, path in _numstat_records(data):
        changed_paths.append(path)
        if added == deleted == "-":
            binary_files.append(path)
        else:
            if not added.isdecimal() or not deleted.isdecimal():
                raise ValueError(f"malformed git numstat counts for {path!r}")
            lines_added += int(added)
            lines_deleted += int(deleted)
    manifests_changed = [
        p for p in changed_paths if Path(p).name in DEPENDENCY_MANIFESTS
    ]
    return {
        "files_changed": len(changed_paths),
        "lines_added": lines_added,
        "lines_deleted": lines_deleted,
        "binary_files": sorted(binary_files),
        "dependency_manifests_changed": sorted(manifests_changed),
    }


def _required_string(obj: dict[str, Any], name: str) -> str:
    value = obj.get(name)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{name} must be a nonempty string")
    return value


def evaluate(manifest: Path) -> dict[str, Any]:
    manifest = Path(manifest).resolve()
    data = json.loads(manifest.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or data.get("schema_version") != SCHEMA_VERSION:
        raise ValueError(f"schema_version must equal {SCHEMA_VERSION}")
    scenario = _required_string(data, "scenario")
    task_id = _required_string(data, "task_id")
    implementations = data.get("implementations")
    if not isinstance(implementations, list) or not implementations:
        raise ValueError("implementations must be a nonempty list")
    seen: set[str] = set()
    results = []
    for item in implementations:
        if not isinstance(item, dict):
            raise ValueError("implementation entries must be objects")
        language = _required_string(item, "language")
        if language not in LANGUAGES:
            raise ValueError(f"unsupported language: {language}")
        if language in seen:
            raise ValueError(f"duplicate language: {language}")
        seen.add(language)
        repo_name = _required_string(item, "repo")
        base_sha = _required_string(item, "base_sha")
        head_sha = _required_string(item, "head_sha")
        verification = item.get("verification")
        if not isinstance(verification, dict):
            raise ValueError(f"{language} verification must be an object")
        status = verification.get("status")
        if status not in ("passed", "failed", "not_run"):
            raise ValueError(f"{language} verification.status must be passed, failed or not_run")
        if status == "passed":
            _required_string(verification, "command")
            _required_string(verification, "evidence")
        minutes = item.get("change_minutes")
        if minutes is not None and (
            isinstance(minutes, bool) or not isinstance(minutes, (int, float))
            or not (0 <= minutes < float("inf"))
        ):
            raise ValueError("change_minutes must be a nonnegative finite number or null")
        repo = (manifest.parent / repo_name).resolve()
        metrics = measure_change(repo, base_sha, head_sha)
        results.append({
            "language": language,
            "repo": repo_name,
            "base_sha": base_sha,
            "head_sha": head_sha,
            "verification": verification,
            "change_minutes": minutes,
            "metrics": metrics,
        })
    results.sort(key=lambda row: row["language"])
    all_passed = all(item["verification"]["status"] == "passed" for item in results)
    return {
        "schema_version": SCHEMA_VERSION,
        "scenario": scenario,
        "task_id": task_id,
        "comparison_status": (
            "ready_for_human_parity_review"
            if seen == LANGUAGES and all_passed else "incomplete"
        ),
        "interpretation": (
            "Change amplification only. Passing recorded checks does not prove equivalent "
            "behavior, maintenance effort, correctness, or lower entropy. "
            "Independent human parity review is required before any comparison."
        ),
        "implementations": results,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path, help="one task shared across Nulang, Go, Rust")
    parser.add_argument("--output", type=Path, help="write JSON report to this path")
    args = parser.parse_args(argv)
    try:
        result = evaluate(args.manifest)
    except (OSError, ValueError, json.JSONDecodeError) as exc:
        print(f"maintenance benchmark error: {exc}", file=sys.stderr)
        return 2
    serialized = json.dumps(result, indent=2, ensure_ascii=True) + "\n"
    if args.output:
        args.output.write_text(serialized, encoding="utf-8")
    else:
        print(serialized, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
