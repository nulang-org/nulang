#!/usr/bin/env python3
"""Apply a unified diff by exact hunk content, ignoring stale line offsets.

Every hunk body must match exactly. If an exact old block occurs more than
once, a non-placeholder source line may disambiguate only between those
byte-identical matches, and only within a bounded distance. Missing or still
ambiguous matches are fatal. No fuzzy context is ever accepted.
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass, field
from pathlib import Path
import re
import shlex
import sys

MAX_LINE_DRIFT = 250


@dataclass
class Hunk:
    old_start: int
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
            match = re.match(r"@@ -(\d+)(?:,\d+)? \+\d+(?:,\d+)? @@", line)
            if not match:
                raise ValueError(f"unsupported hunk header: {line.rstrip()}")
            hunk = Hunk(old_start=int(match.group(1)))
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
            raise ValueError(
                f"unexpected line inside hunk for {current.path}: {line.rstrip()}"
            )

    if not patches:
        raise ValueError("no file patches found")
    if any(not p.hunks for p in patches):
        missing = [p.path for p in patches if not p.hunks]
        raise ValueError(f"file patch has no hunks: {missing}")
    return patches


def exact_positions(text: str, needle: str) -> list[int]:
    positions: list[int] = []
    start = 0
    while True:
        pos = text.find(needle, start)
        if pos < 0:
            return positions
        positions.append(pos)
        start = pos + max(1, len(needle))


def hunk_identity(old: str, old_start: int) -> str:
    first = old.splitlines()[0].strip() if old.splitlines() else "<empty>"
    return f"source_hint={old_start}, first={first!r}"


def choose_position(path: str, index: int, current: str, old: str, old_start: int) -> int:
    ident = hunk_identity(old, old_start)
    positions = exact_positions(current, old)
    if len(positions) == 1:
        return positions[0]
    if not positions:
        raise RuntimeError(
            f"{path} hunk {index} ({ident}): exact original block not found"
        )
    if old_start <= 1:
        raise RuntimeError(
            f"{path} hunk {index} ({ident}): {len(positions)} exact matches and no usable line hint"
        )

    candidates = []
    for pos in positions:
        line = current.count("\n", 0, pos) + 1
        candidates.append((abs(line - old_start), line, pos))
    candidates.sort()

    if len(candidates) > 1 and candidates[0][0] == candidates[1][0]:
        raise RuntimeError(
            f"{path} hunk {index} ({ident}): exact matches tie around source line {old_start}"
        )
    drift, line, pos = candidates[0]
    if drift > MAX_LINE_DRIFT:
        raise RuntimeError(
            f"{path} hunk {index} ({ident}): nearest exact match at line {line} drifts {drift} lines"
        )
    print(
        f"strict patch: {path} hunk {index} disambiguated exact duplicate "
        f"at line {line} (hint {old_start})"
    )
    return pos


def apply(root: Path, patches: list[FilePatch], write: bool) -> None:
    contents: dict[str, str] = {}
    existed: dict[str, bool] = {}

    for block_index, patch in enumerate(patches, start=1):
        path = root / patch.path
        if patch.path not in contents:
            existed[patch.path] = path.exists()
            contents[patch.path] = path.read_text() if path.exists() else ""

        current = contents[patch.path]
        for index, hunk in enumerate(patch.hunks, start=1):
            old = "".join(hunk.old)
            new = "".join(hunk.new)
            try:
                if not old:
                    if current != "" or existed[patch.path]:
                        raise RuntimeError("empty-old hunk requires a new empty file")
                    current = new
                    continue
                pos = choose_position(patch.path, index, current, old, hunk.old_start)
                current = current[:pos] + new + current[pos + len(old):]
            except RuntimeError as exc:
                raise RuntimeError(
                    f"diff block {block_index}, {patch.path}: {exc}"
                ) from exc

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
