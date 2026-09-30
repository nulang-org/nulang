from pathlib import Path

path = Path("CHANGELOG.md")
text = path.read_text()
marker = "### Added since 1.0.0-frozen — 2026-09-14 (web contract + capacity broker hardening)\n"
entry = """### Actor dispatch correctness — 2026-09-17
- **Fail-closed unknown actor behavior dispatch** (`src/runtime/mod.rs`,
  `src/runtime/distributed.rs`): unresolved behavior names no longer alias
  behavior slot 0. Local sends reject unknown names, invalid synchronous asks
  reject nonexistent numeric handlers, and distributed delivery resolves by
  name before assigning a numeric slot. Content-addressed fetch/hot reload
  preserves fetch-on-demand without sentinel behavior ids and re-verifies the
  requested content hash before delivery.

"""

if entry in text:
    raise SystemExit("changelog entry already present")
if text.count(marker) != 1:
    raise SystemExit(f"changelog insertion marker count changed: {text.count(marker)}")
path.write_text(text.replace(marker, entry + marker, 1))
