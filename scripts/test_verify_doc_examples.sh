#!/usr/bin/env bash
# Regression: the documentation verifier must discover both .md and .mdx pages.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

mkdir -p "$fixture/docs/src/content/docs" "$fixture/src"

cat > "$fixture/docs/src/content/docs/markdown.md" <<'DOC'
# Markdown page

```nulang
perform IO.print("from markdown")
```
DOC

cat > "$fixture/docs/src/content/docs/markdown.mdx" <<'DOC'
# MDX page

```nulang
perform IO.print("from mdx")
```
DOC

# The test checks discovery only: no Rust toolchain or runtime needed.
cat > "$fixture/fake-nulang" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$fixture/fake-nulang"

output="$(cd "$fixture" && NULANG_BIN="$fixture/fake-nulang" bash "$repo_root/scripts/verify_doc_examples.sh")"
if [[ "$output" != *"2 passed, 0 failed, 0 skipped (2 total)"* ]]; then
    printf 'Expected both .md and .mdx to be verified. Actual output:\n%s\n' "$output" >&2
    exit 1
fi
printf 'PASS: documentation verifier discovers .md and .mdx pages\n'
