#!/usr/bin/env bash
# Verify all ```nulang code blocks in documentation.
# Runs each block through --check (parse + type + effect checking) or
# a full run when the block is a standalone program.
#
# Sources scanned:
#   1. Markdown fences in the Astro docs site
#      (docs/src/content/docs/**/*.{md,mdx}) and the repo-root docs
#      (SPEC2.md, README.md, docs/GETTING_STARTED.md, docs/TUTORIAL.md).
#   2. `///` doc comments in .nula source files (PLAN Phase 1 bullet 6).
#
# Blocks that are intentionally NOT standalone programs are skipped via
# explicit markers, never silently:
#   - a `// fragment` comment line (illustrative snippet that references
#     surrounding prose — SPEC2 narrative examples),
#   - a REPL session (`nulang>` prompt),
#   - a section whose own heading carries "— Planned" (aspirational
#     syntax for a feature that does not exist yet; handles #, ## and
#     ### headings).
set -uo pipefail
NULANG="${NULANG_BIN:-cargo run --quiet --}"

DOCS_DIR="docs/src/content/docs"
ROOT_DOCS=(SPEC2.md README.md docs/GETTING_STARTED.md docs/TUTORIAL.md)
TMPDIR=$(mktemp -d)
PASS=0
FAIL=0
SKIP=0
TOTAL=0

cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

# Check if a block looks like a REPL session (starts with nulang>)
is_repl() { echo "$1" | head -1 | grep -q '^nulang>'; }

# Check if block looks runnable (not a declaration fragment)
is_runnable() {
    local first
    first=$(echo "$1" | head -1)
    # Declaration keywords → fragment, not standalone
    echo "$first" | grep -qE '^(actor |behavior |fn |effect |type |workflow|agent |state |import |use |receive )' && return 1
    return 0
}

echo "=== Verifying documentation code examples ==="
echo ""

verify_file() {
    local file="$1" rel="$2"

    # Extract blocks: lines between ```nulang and ```
    in_block=0
    block=""
    block_num=0
    # Track whether the CURRENT markdown section (chapter '# ' or
    # subsection '## ') is explicitly marked '— Planned' in its own
    # heading text. Both chapter- and subsection-level markers exist in
    # SPEC2.md (e.g. '# Chapter 14: Standard Library — Planned' and
    # '## 5.3 Authority Capabilities — Planned'); a block under either
    # is aspirational syntax for a feature that doesn't exist yet, not a
    # doc-accuracy bug, so it's skipped like a REPL session rather than
    # expected to compile.
    chapter_planned=0
    section_planned=0

    while IFS= read -r line || [[ -n "$line" ]]; do
        if [[ $in_block -eq 0 ]]; then
            if [[ "$line" == "# "* ]]; then
                chapter_planned=0
                section_planned=0
                [[ "$line" == *"Planned"* ]] && chapter_planned=1
            elif [[ "$line" == "## "* || "$line" == "### "* ]]; then
                section_planned=0
                [[ "$line" == *"Planned"* ]] && section_planned=1
            fi
        fi
        if [[ "$line" == '```nulang' ]]; then
            in_block=1
            block=""
            block_num=$((block_num + 1))
            continue
        fi
        if [[ "$line" == '```' ]] && [[ $in_block -eq 1 ]]; then
            in_block=0
            # Process the completed block
            [[ -z "$(echo "$block" | tr -d '[:space:]')" ]] && continue
            TOTAL=$((TOTAL + 1))
            label="${rel}#${block_num}"

            if is_repl "$block"; then
                SKIP=$((SKIP + 1))
                echo "  SKIP  $label (REPL session)"
                continue
            fi

            if echo "$block" | grep -q '^// fragment'; then
                SKIP=$((SKIP + 1))
                echo "  SKIP  $label (illustrative fragment)"
                continue
            fi

            if [[ $chapter_planned -eq 1 || $section_planned -eq 1 ]]; then
                SKIP=$((SKIP + 1))
                echo "  SKIP  $label (— Planned section, aspirational syntax)"
                continue
            fi

            if is_runnable "$block"; then
                echo "$block" > "$TMPDIR/test.nula"
                if $NULANG "$TMPDIR/test.nula" >/dev/null 2>&1; then
                    PASS=$((PASS + 1))
                    echo "  PASS  $label (run)"
                elif $NULANG --check "$TMPDIR/test.nula" >/dev/null 2>&1; then
                    PASS=$((PASS + 1))
                    echo "  PASS  $label (check only)"
                else
                    FAIL=$((FAIL + 1))
                    echo "  FAIL  $label"
                    $NULANG --check "$TMPDIR/test.nula" 2>&1 | head -3 | sed 's/^/        /'
                fi
            else
                echo "$block" > "$TMPDIR/test.nula"
                if $NULANG --check "$TMPDIR/test.nula" >/dev/null 2>&1; then
                    PASS=$((PASS + 1))
                    echo "  OK    $label (check)"
                else
                    FAIL=$((FAIL + 1))
                    echo "  FAIL  $label"
                    $NULANG --check "$TMPDIR/test.nula" 2>&1 | head -3 | sed 's/^/        /'
                fi
            fi
            continue
        fi
        if [[ $in_block -eq 1 ]]; then
            block="${block}${line}"$'\n'
        fi
    done < "$file"
}

# Verify ```nulang blocks inside `///` doc comments of one .nula source
# file. Consecutive `///` lines form the doc comment; fences inside it
# are extracted and verified with the same machinery as markdown blocks
# (run first, fall back to --check; declaration-first blocks check only).
# `////` lines are regular comments, not doc comments (matches docgen).
verify_nula_source() {
    local file="$1" rel="$2"
    local in_block=0 block="" block_num=0

    while IFS= read -r line || [[ -n "$line" ]]; do
        local trimmed="${line#"${line%%[![:space:]]*}"}"
        [[ "$trimmed" == "////"* ]] && continue   # regular comment
        [[ "$trimmed" != "///"* ]] && continue    # not a doc line
        local content="${trimmed#///}"
        content="${content#"${content%%[![:space:]]*}"}"

        if [[ $in_block -eq 0 && "$content" == '```nulang' ]]; then
            in_block=1
            block=""
            block_num=$((block_num + 1))
            continue
        fi
        if [[ $in_block -eq 1 && "$content" == '```' ]]; then
            in_block=0
            [[ -z "$(echo "$block" | tr -d '[:space:]')" ]] && continue
            TOTAL=$((TOTAL + 1))
            label="${rel}#${block_num}"

            if is_repl "$block"; then
                SKIP=$((SKIP + 1))
                echo "  SKIP  $label (REPL session)"
                continue
            fi

            if echo "$block" | grep -q '^// fragment'; then
                SKIP=$((SKIP + 1))
                echo "  SKIP  $label (illustrative fragment)"
                continue
            fi

            if is_runnable "$block"; then
                echo "$block" > "$TMPDIR/test.nula"
                if $NULANG "$TMPDIR/test.nula" >/dev/null 2>&1; then
                    PASS=$((PASS + 1))
                    echo "  PASS  $label (run)"
                elif $NULANG --check "$TMPDIR/test.nula" >/dev/null 2>&1; then
                    PASS=$((PASS + 1))
                    echo "  PASS  $label (check only)"
                else
                    FAIL=$((FAIL + 1))
                    echo "  FAIL  $label"
                    $NULANG --check "$TMPDIR/test.nula" 2>&1 | head -3 | sed 's/^/        /'
                fi
            else
                echo "$block" > "$TMPDIR/test.nula"
                if $NULANG --check "$TMPDIR/test.nula" >/dev/null 2>&1; then
                    PASS=$((PASS + 1))
                    echo "  OK    $label (check)"
                else
                    FAIL=$((FAIL + 1))
                    echo "  FAIL  $label"
                    $NULANG --check "$TMPDIR/test.nula" 2>&1 | head -3 | sed 's/^/        /'
                fi
            fi
            continue
        fi
        if [[ $in_block -eq 1 ]]; then
            block="${block}${content}"$'\n'
        fi
    done < "$file"
}

while IFS= read -r -d '' file; do
    verify_file "$file" "${file#$DOCS_DIR/}"
done < <(find "$DOCS_DIR" -type f \( -name '*.md' -o -name '*.mdx' \) -print0)

for file in "${ROOT_DOCS[@]}"; do
    [[ -f "$file" ]] || continue
    verify_file "$file" "$file"
done

# .nula sources: every ```nulang block inside a /// doc comment must
# compile+run (PLAN Phase 1 bullet 6). Repo-controlled content, always on.
while IFS= read -r -d '' file; do
    verify_nula_source "$file" "$file"
done < <(find src -name '*.nula' -print0)

echo ""
echo "=== Results: $PASS passed, $FAIL failed, $SKIP skipped ($TOTAL total) ==="
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
