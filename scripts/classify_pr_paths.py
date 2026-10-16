#!/usr/bin/env python3
"""Audit a PR diff for docs-only eligibility; do not bypass required CI jobs.

This tool is deliberately advisory until required-check migration and exact-head
trial validation are complete (see issue #1566). Any uncertainty fails closed:
nonzero on broken diff/refs, full on unknown or mixed change paths.
"""

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys


class ClassificationError(Exception):
    """Cannot safely establish the complete PR change set."""


def git(repo, *args):
    try:
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout
    except (subprocess.CalledProcessError, OSError) as exc:
        raise ClassificationError(f"git operation failed: {args[0]}") from exc


def parse_name_status(raw):
    """Parse Git's NUL-delimited diff, preserving both sides of renames/copies."""
    if raw and not raw.endswith(b"\0"):
        raise ClassificationError("incomplete NUL-delimited diff")
    fields = iter(raw[:-1].split(b"\0") if raw else ())
    changes = []
    for token in fields:
        try:
            status = token.decode("ascii")
        except UnicodeDecodeError as exc:
            raise ClassificationError("non-ASCII Git change status") from exc
        if not status or status[0] not in "ACDMRT":
            raise ClassificationError(f"unknown Git change status: {status!r}")
        if status[0] in "RC":
            if not status[1:].isdigit():
                raise ClassificationError(f"malformed rename/copy status: {status!r}")
            count = 2
        else:
            if len(status) != 1:
                raise ClassificationError(f"unexpected change status: {status!r}")
            count = 1
        paths = []
        for _ in range(count):
            try:
                path = os.fsdecode(next(fields))
            except StopIteration as exc:
                raise ClassificationError("missing path in NUL-delimited diff") from exc
            if not path or path.startswith("/") or "\\" in path or any(
                part in ("", ".", "..") for part in path.split("/")
            ):
                raise ClassificationError("unsafe or empty changed path")
            paths.append(path)
        changes.extend(paths)
    return changes


ROOT_DOC_FILES = frozenset({"README.md", "CONTRIBUTING.md", "CHANGELOG.md"})
DOC_EXTENSIONS = frozenset({".md", ".mdx"})


def is_docs_only_path(path):
    """Conservative allowlist; executable docs/Astro, workflows and scripts stay full."""
    if path in ROOT_DOC_FILES:
        return True
    file = PurePosixPath(path)
    if path.startswith("docs/") or path.startswith("RFC/"):
        return file.suffix.lower() in DOC_EXTENSIONS
    if path.startswith(".github/ISSUE_TEMPLATE/"):
        return file.suffix.lower() in (".yml", ".yaml")
    return False


def is_regular_document_at_revisions(repo, path, revisions):
    """Reject symlinks, submodules, executable blobs, and unexpected tree modes.

    A path's name alone cannot prove it is documentation: Git can track a
    symlink, gitlink, or executable file with a Markdown extension. Check both
    commit trees, including the deleted/renamed-away side of the change.
    """
    expected_path = os.fsencode(path)
    for revision in revisions:
        raw = git(repo, "ls-tree", "-rz", revision, "--", path)
        if not raw:
            continue  # file added or deleted in this revision
        if not raw.endswith(b"\0"):
            raise ClassificationError("incomplete Git tree listing")
        entries = raw[:-1].split(b"\0")
        if len(entries) != 1:
            raise ClassificationError("ambiguous Git tree entry for changed path")
        metadata, delimiter, found_path = entries[0].partition(b"\t")
        parts = metadata.split(b" ")
        if (not delimiter or found_path != expected_path or len(parts) != 3):
            raise ClassificationError("malformed Git tree entry")
        mode, kind, _sha = parts
        if mode != b"100644" or kind != b"blob":
            return False
    return True


def classify(repo, base, head):
    """Compare exact PR head against merge-base of the explicitly supplied base."""
    base_sha = git(repo, "rev-parse", "--verify", f"{base}^{{commit}}").decode().strip()
    head_sha = git(repo, "rev-parse", "--verify", f"{head}^{{commit}}").decode().strip()
    merge_base = git(repo, "merge-base", base_sha, head_sha).decode().strip()
    if not merge_base:
        raise ClassificationError("unable to establish PR merge base")
    raw = git(repo, "diff", "--name-status", "--find-renames", "-z", merge_base, head_sha, "--")
    paths = sorted(set(parse_name_status(raw)))
    unsafe_paths = [
        path for path in paths
        if not is_docs_only_path(path)
        or not is_regular_document_at_revisions(repo, path, (merge_base, head_sha))
    ]
    eligible = bool(paths) and not unsafe_paths
    return {
        "schema": 1,
        "mode": "docs-only" if eligible else "full",
        "reason": (
            "all changed paths match the documentation allowlist"
            if eligible
            else "no changed paths; full checks are required"
            if not paths
            else "one or more changed paths require full checks"
        ),
        "base": base_sha,
        "head": head_sha,
        "merge_base": merge_base,
        "paths": paths,
        "unsafe_paths": unsafe_paths,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    parser.add_argument("--base", required=True, help="Exact PR base SHA or ref")
    parser.add_argument("--head", required=True, help="Exact PR head SHA or ref")
    args = parser.parse_args(argv)
    try:
        result = classify(args.repo, args.base, args.head)
    except ClassificationError as exc:
        print(f"cannot safely classify PR: {exc}; run full CI", file=sys.stderr)
        return 2
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
