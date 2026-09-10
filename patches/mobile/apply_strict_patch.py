#!/usr/bin/env python3
"""Apply a unified diff by exact hunk content, ignoring stale line offsets.

Every non-new-file hunk must match its original text exactly once at the point
it is applied. Missing or ambiguous matches are fatal. This is intentionally
stricter than fuzzy `patch` and is used only to rebase generated patches whose
line-number metadata is stale.
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass, field
from pathlib import Path
import shlex
import sys


@dataclass
class Hunk:
    old: list[str] = field(default_factory=list)
    new: list[str] = field(default_factory=list)


@dataclass
class FilePatch:
    path: str
    new_file: bool = False
    hunks: list[Hunk] = field(default_factory=list)


def parse_patch(text: str) -> list[FilePatch]:
    lines = text.splitlines(keepends=True)
    patches: list[FilePatch] = []
    current: FilePatch | None = None
    hunk: Hunk | None = None

    for line in lines:
        if line.startswith("diff --git "):
            parts = shlex.split(line.rstrip("\n"))
            if len(parts) != 4 or not parts[3].startswith("b/"):
                raise ValueError(f"unsupported diff header: {line.rstrip()}")
            current = FilePatch(path=parts[3][2:])
            patches.append(current)
            hunk = None
            continue

        if current is None:
            continue

        if line.startswith("new file mode "):
            current.new_file = True
            continue

        if line.startswith("@@"):
            hunk = Hunk()
            current.hunks.append(hunk)
            continue

        if hunk is None:
            continue

        if line.startswith("\\ No newline at end of file"):
            continue
        if line.startswith(" "):
            hunk.old.append(line[1:])
            hunk.new.append(line[1:])
        elif line.startswith("-"):
            hunk.old.append(line[1:])
        elif line.startswith("+"):
            hunk.new.append(line[1:])
        else:
            # A new metadata block ends the current hunk; malformed content is
            # rejected rather than guessed.
            raise ValueError(
                f"unexpected line inside hunk for {current.path}: {line.rstrip()}"
            )

    if not patches:
        raise ValueError("no file patches found")
    if any(not p.hunks for p in patches):
        missing = [p.path for p in patches if not p.hunks]
        raise ValueError(f"file patch has no hunks: {missing}")
    return patches


def apply(root: Path, patches: list[FilePatch], write: bool) -> None:
    contents: dict[str, str] = {}
    existed: dict[str, bool] = {}

    for patch in patches:
        path = root / patch.path
        if patch.path not in contents:
            existed[patch.path] = path.exists()
            contents[patch.path] = path.read_text() if path.exists() else ""

        current = contents[patch.path]
        for index, hunk in enumerate(patch.hunks, start=1):
            old = "".join(hunk.old)
            new = "".join(hunk.new)

            if not old:
                if current != "" or existed[patch.path]:
                    raise RuntimeError(
                        f"{patch.path} hunk {index}: empty-old hunk requires a new empty file"
                    )
                current = new
                continue

            count = current.count(old)
            if count != 1:
                raise RuntimeError(
                    f"{patch.path} hunk {index}: expected exactly one exact match, found {count}"
                )
            current = current.replace(old, new, 1)

        contents[patch.path] = current

    if write:
        for rel, content in contents.items():
            path = root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("patch", type=Path)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    root = Path.cwd()
    try:
        patches = parse_patch(args.patch.read_text())
        apply(root, patches, write=not args.check)
    except (OSError, ValueError, RuntimeError) as exc:
        print(f"strict patch error: {exc}", file=sys.stderr)
        return 1

    action = "preflight" if args.check else "apply"
    print(f"strict patch {action}: OK ({len(patches)} file diff blocks)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
