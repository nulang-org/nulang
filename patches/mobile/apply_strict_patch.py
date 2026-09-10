#!/usr/bin/env python3
"""Apply a generated unified diff using exact original content only.

Stale line numbers are ignored for matching. Repeated byte-identical blocks may
use a bounded non-placeholder line hint for disambiguation. A placeholder hunk
(`@@ -1 ...`) is skipped only when a later, larger hunk for the same file
contains all lines actually removed and added by that placeholder. No fuzzy
context is accepted.
"""
from __future__ import annotations

import argparse
from collections import Counter
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

    @property
    def old_text(self) -> str:
        return "".join(self.old)

    @property
    def new_text(self) -> str:
        return "".join(self.new)

    def changed_lines(self) -> tuple[Counter[str], Counter[str]]:
        old = Counter(self.old)
        new = Counter(self.new)
        return old - new, new - old


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
            hunk.old.append(line[1:]); hunk.new.append(line[1:])
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
        raise ValueError(
            "file patch has no hunks: " + ", ".join(p.path for p in patches if not p.hunks)
        )
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


def ident(hunk: Hunk) -> str:
    lines = hunk.old_text.splitlines()
    first = lines[0].strip() if lines else "<empty>"
    return f"source_hint={hunk.old_start}, first={first!r}"


def counter_contains(haystack: Counter[str], needle: Counter[str]) -> bool:
    return all(haystack[line] >= count for line, count in needle.items())


def is_subsumed_placeholder(patches: list[FilePatch], block_index: int, hunk: Hunk) -> bool:
    if hunk.old_start > 1 or not hunk.old_text:
        return False
    removed, added = hunk.changed_lines()
    if not removed and not added:
        return False
    path = patches[block_index].path
    for later in patches[block_index + 1:]:
        if later.path != path:
            continue
        for later_hunk in later.hunks:
            if len(later_hunk.old_text) <= len(hunk.old_text):
                continue
            later_old = Counter(later_hunk.old)
            later_new = Counter(later_hunk.new)
            if counter_contains(later_old, removed) and counter_contains(later_new, added):
                return True
    return False


def choose_position(path: str, hunk_index: int, current: str, hunk: Hunk) -> int:
    old = hunk.old_text
    positions = exact_positions(current, old)
    if len(positions) == 1:
        return positions[0]
    if not positions:
        raise RuntimeError(
            f"{path} hunk {hunk_index} ({ident(hunk)}): exact original block not found"
        )
    if hunk.old_start <= 1:
        raise RuntimeError(
            f"{path} hunk {hunk_index} ({ident(hunk)}): {len(positions)} exact matches and no usable line hint"
        )
    candidates = []
    for pos in positions:
        line = current.count("\n", 0, pos) + 1
        candidates.append((abs(line - hunk.old_start), line, pos))
    candidates.sort()
    if len(candidates) > 1 and candidates[0][0] == candidates[1][0]:
        raise RuntimeError(
            f"{path} hunk {hunk_index} ({ident(hunk)}): exact matches tie around source line {hunk.old_start}"
        )
    drift, line, pos = candidates[0]
    if drift > MAX_LINE_DRIFT:
        raise RuntimeError(
            f"{path} hunk {hunk_index} ({ident(hunk)}): nearest exact match at line {line} drifts {drift} lines"
        )
    print(
        f"strict patch: {path} hunk {hunk_index} disambiguated exact duplicate "
        f"at line {line} (hint {hunk.old_start})"
    )
    return pos


def apply(root: Path, patches: list[FilePatch], write: bool) -> None:
    contents: dict[str, str] = {}
    existed: dict[str, bool] = {}
    for zero_block, patch in enumerate(patches):
        block_index = zero_block + 1
        path = root / patch.path
        if patch.path not in contents:
            existed[patch.path] = path.exists()
            contents[patch.path] = path.read_text() if path.exists() else ""
        current = contents[patch.path]
        for hunk_index, hunk in enumerate(patch.hunks, start=1):
            try:
                if is_subsumed_placeholder(patches, zero_block, hunk):
                    print(
                        f"strict patch: skip subsumed placeholder block {block_index} "
                        f"for {patch.path} ({ident(hunk)})"
                    )
                    continue
                old = hunk.old_text
                new = hunk.new_text
                if not old:
                    if current != "" or existed[patch.path]:
                        raise RuntimeError("empty-old hunk requires a new empty file")
                    current = new
                    continue
                pos = choose_position(patch.path, hunk_index, current, hunk)
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
    try:
        patches = parse_patch(args.patch.read_text())
        apply(Path.cwd(), patches, write=not args.check)
    except (OSError, ValueError, RuntimeError) as exc:
        print(f"strict patch error: {exc}", file=sys.stderr)
        return 1
    print(
        f"strict patch {'preflight' if args.check else 'apply'}: OK "
        f"({len(patches)} file diff blocks)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
