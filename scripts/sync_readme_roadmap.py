#!/usr/bin/env python3
"""Refresh the README's issue-backed roadmap without touching prose outside its markers."""

import json
import os
import re
from pathlib import Path
from urllib.request import Request, urlopen

REPO = "nulang-org/nulang"
TRACKER = 331
START = "<!-- roadmap:start -->"
END = "<!-- roadmap:end -->"
TRACKS = (
    ("P0 — semantic correctness stack", "Semantic correctness (P0)"),
    ("Protocol and delivery evolution", "Protocols and delivery"),
    ("Durable external effects", "Durable external effects"),
    ("Remaining P1 identity gap", "Durable identity (P1)"),
    ("P1/P2 authority completion", "Host and WASM authority"),
    ("P2/P3 tooling", "Tooling"),
)


def tracked_issues(body):
    """Read references only from list entries in explicitly tracked issue sections."""
    by_heading = {heading: [] for heading, _ in TRACKS}
    heading = None
    for line in body.splitlines():
        if line.startswith("## "):
            heading = line[3:].strip()
        elif heading in by_heading and re.match(r"^\s*[-*] ", line):
            for issue in re.findall(r"(?<!\w)#(\d+)\b", line):
                number = int(issue)
                if number not in by_heading[heading]:
                    by_heading[heading].append(number)
    if not any(by_heading.values()):
        raise ValueError("Roadmap tracker has no recognized issue references; refusing to clear README")
    return by_heading


def render(tracks, states):
    """Open means open GitHub issue/PR, not unfinished work; closed isn't proof of delivery."""
    rows = [
        "_Generated from the [maintained roadmap tracker](https://github.com/nulang-org/nulang/issues/331). "
        "Statuses reflect linked GitHub issues and PRs, **not verified feature completion**._",
        "",
        "| Focus | Open / tracked | Open references |",
        "| --- | ---: | --- |",
    ]
    for heading, label in TRACKS:
        ids = tracks[heading]
        if not ids:
            continue
        pending = [n for n in ids if states[n] == "open"]
        sample = ", ".join(f"[#{n}](https://github.com/{REPO}/issues/{n})" for n in pending[:4]) or "—"
        if len(pending) > 4:
            sample += f" (+{len(pending) - 4} more in tracker)"
        rows.append(f"| {label} | {len(pending)} / {len(ids)} | {sample} |")
    rows.extend([
        "",
        "For the full dependency ordering, rationale, and change proposals, see "
        "[roadmap issue #331](https://github.com/nulang-org/nulang/issues/331). "
        "For already implemented behavior and limitations, see "
        "[implementation status](docs/IMPLEMENTATION_STATUS.md) and [changelog](CHANGELOG.md).",
    ])
    return "\n".join(rows)


def replace_block(readme, generated):
    if readme.count(START) != 1 or readme.count(END) != 1:
        raise ValueError("README must contain exactly one start and end roadmap marker")
    start = readme.index(START) + len(START)
    end = readme.index(END)
    if end < start:
        raise ValueError("README roadmap markers are in reverse order")
    return readme[:start] + "\n" + generated.rstrip() + "\n" + readme[end:]


def github_issue(number):
    url = f"https://api.github.com/repos/{REPO}/issues/{number}"
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if not token:
        raise RuntimeError("GH_TOKEN or GITHUB_TOKEN is required")
    request = Request(url, headers={
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "User-Agent": "nulang-readme-roadmap-sync",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    with urlopen(request, timeout=20) as response:
        return json.load(response)


def main():
    tracker = github_issue(TRACKER)
    tracks = tracked_issues(tracker.get("body") or "")
    states = {}
    for number in sorted({n for refs in tracks.values() for n in refs}):
        state = github_issue(number).get("state")
        if state not in ("open", "closed"):
            raise ValueError(f"Unexpected issue state for #{number}: {state!r}")
        states[number] = state
    path = Path("README.md")
    old = path.read_text(encoding="utf-8")
    updated = replace_block(old, render(tracks, states))
    if old != updated:
        path.write_text(updated, encoding="utf-8")
        print("README roadmap refreshed")
    else:
        print("README roadmap already current")


if __name__ == "__main__":
    main()
