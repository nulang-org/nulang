# Imported-source lexer token cache

This change caches **lexer tokens only** for imported source files in a long-lived
Nulang process. It does not cache parsed ASTs, type-check results, imports,
MIR, bytecode, native code, or complete builds.

## Correctness boundary

Each import continues to read the current source bytes from disk. The cache
entry is addressed by the resolved canonical file path and is reusable only
when its BLAKE3 fingerprint matches those bytes. Cache hits restore the lexer
diagnostic SourceMap. Source edits — including same-length edits — miss.

Import resolution continues to clear its resolved-declaration cache for every
top-level compilation. The parser and type checker execute freshly, including
parser-time lookups of imported type declarations and new type-variable IDs.
These properties are essential: caching ASTs based only on their source bytes
would be unsafe while `Parser::try_import_type` reads dependencies and
`TypeVar::fresh` produces global identities.

The token cache is thread-local and bounded to 64 files with at most
128 KiB of source and 8,192 tokens per entry. A larger file is always lexed
fresh. The deterministic eviction policy removes the lexicographically first
path, not the least recently used path; this is intentionally simple and may
underperform for large import sets.

## Verification

```sh
cargo test --no-default-features --lib resolver::tests
cargo fmt --all -- --check
cargo clippy --no-default-features --lib -- -D warnings
```

Focused regressions cover reuse of identical tokens, same-length source
edits, cache-hit SourceMap restoration, and the visibility of transitive
dependency edits while the parent's tokens are cached.

## Performance decision

Do **not** assume this cache is faster. It avoids lexing on a hit, but still
pays for disk reads, BLAKE3 hashing, and cloning the token vector. Run a
paired before/after measurement with the same warmed process, imported
source tree, hardware, release profile, and repeated compiles. Include a
large dependency graph and a small-source control; measure p50/p95 compilation
wall time and peak RSS. If token-clone + hash overhead dominates, revert or
limit caching to modules where benchmark evidence shows a net win.

The separate `nulang-compile-bench` harness currently measures self-contained
modules without imports, so its numbers cannot be used to claim a benefit
from this change.
