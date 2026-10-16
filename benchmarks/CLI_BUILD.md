# Package CLI build A/B benchmark

Purpose: measure the developer's full \`nula build\` subprocess latency, not isolated
compiler-stage speed or cold OS page-cache behavior. Use a **single host** and
two prebuilt compiler binaries from exact Git SHAs.

## Build two binaries independently

In separate Git worktrees for the base and candidate commits, run:

\`\`\`sh
# In base worktree:
CARGO_TARGET_DIR=/tmp/nulang-cli-base cargo build --release --bin nulang
# In candidate worktree:
CARGO_TARGET_DIR=/tmp/nulang-cli-candidate cargo build --release --bin nulang
\`\`\`

Then, from the candidate worktree:

\`\`\`sh
python3 scripts/benchmark_package_build.py \
  --base-binary /tmp/nulang-cli-base/release/nulang \
  --candidate-binary /tmp/nulang-cli-candidate/release/nulang \
  --package benchmarks/fixtures/nula-build-smoke \
  --stdlib src/stdlib \
  --rounds 7 --mode both --out /tmp/nulang-cli-build-ab.json
\`\`\`

The script alternates AB/BA order, uses separate copies of the same fixture,
excludes Cargo compilation and fixture copying from timed sections, and runs
both fresh-project and repeat-in-place builds. It checks that bytecode and
behavior-sidecar bytes are identical across both binaries and across rebuilds;
mismatches or compiler failures **invalidate the measurement**.

Fresh-project means a new package directory, **not** a cold kernel filesystem
cache. The output is a median of round-paired speedup ratios, plus raw per-round
wall times. Run on an otherwise idle host and archive the exact binaries,
SHAs, CPU/OS configuration, and output together. The minimal smoke fixture
measures process and frontend overhead; repeat using a representative
self-contained project to estimate production-workflow impact.

## Harness tests

\`\`\`sh
python3 -m unittest discover -s scripts/tests -p 'test_benchmark_package_build.py' -v
\`\`\`

Do not treat passing harness tests as evidence that Nulang compilation is
correct or faster. Run exact-head Rust integration tests and CI separately.
