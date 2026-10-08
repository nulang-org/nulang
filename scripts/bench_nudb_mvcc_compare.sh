#!/usr/bin/env bash
# Compare the same NuDB MVCC benchmark harness against immutable source heads.
# Worktrees and Cargo output stay in an isolated temporary directory.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
baseline_ref="850446ead383caeadd91951f2962677180b77047"
candidate_ref="HEAD"
if (( $# >= 1 )); then baseline_ref="$1"; fi
if (( $# >= 2 )); then candidate_ref="$2"; fi

baseline_sha="$(git -C "$repo" rev-parse --verify "$baseline_ref^{commit}")"
candidate_sha="$(git -C "$repo" rev-parse --verify "$candidate_ref^{commit}")"
if [[ "$baseline_sha" == "$candidate_sha" ]]; then
  echo "Refusing to benchmark the same commit twice" >&2
  exit 2
fi
if ! command -v cargo >/dev/null || ! command -v rustup >/dev/null; then
  echo "Rust/Cargo with rustup is required (project toolchain: 1.95.0)" >&2
  exit 2
fi

# Guard against silently replacing a baseline that has a different registry.
stripped_registry="$(sed -e '/^mod nudb_mvcc_bench;$/d' -e '/^[[:space:]]*nudb_mvcc_bench::benches,$/d' "$repo/benches/bench_main.rs")"
baseline_registry="$(git -C "$repo" show "$baseline_sha:benches/bench_main.rs")"
if [[ "$stripped_registry" != "$baseline_registry" ]]; then
  echo "Baseline benchmark registry changed; review the delta first" >&2
  exit 2
fi

work="$(mktemp -d)"
clean() {
  git -C "$repo" worktree remove --force "$work/baseline" 2>/dev/null || true
  git -C "$repo" worktree remove --force "$work/candidate" 2>/dev/null || true
  rm -rf -- "$work"
}
trap clean EXIT

git -C "$repo" worktree add --detach "$work/baseline" "$baseline_sha"
git -C "$repo" worktree add --detach "$work/candidate" "$candidate_sha"
for tree in "$work/baseline" "$work/candidate"; do
  cp "$repo/benches/nudb_mvcc_bench.rs" "$tree/benches/nudb_mvcc_bench.rs"
  cp "$repo/benches/bench_main.rs" "$tree/benches/bench_main.rs"
done

export CARGO_TARGET_DIR="$work/target"
export CARGO_INCREMENTAL=0

echo "NuDB MVCC: reverse=$baseline_sha binary=$candidate_sha"
(
  cd "$work/baseline"
  cargo +1.95.0 bench --locked --no-default-features --bench bench_main \
    -- nudb/mvcc --save-baseline nudb_reverse
)
(
  cd "$work/candidate"
  cargo +1.95.0 bench --locked --no-default-features --bench bench_main \
    -- nudb/mvcc --baseline nudb_reverse
)
echo "Review the Criterion per-case differences before promotion."
