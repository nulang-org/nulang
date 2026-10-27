# Nulang profile entropy measurements

Status: **measurement-only**. This supplements RFC 0033 (draft) and the semantic-stabilization contract; it does not change the default `full` feature set, release CLI, Cargo dependencies, or required CI gates.

## Why this exists

The smallest build must not acquire unused subsystems by accident. Beyond the qualitative dependency guard, we need reproducible evidence of how the minimal, public release CLI, and full profiles differ. Making the dependency graph visible also identifies the next *real* extraction opportunity rather than guessing from source-file count.

## Profiles and CI evidence

| Metric | Collection method | Interpretation |
| --- | --- | --- |
| Resolved dependency packages | `cargo metadata --locked --no-default-features --filter-platform <host>` with explicit named features | Unique package **IDs** reachable over activated normal/build edges from `nulang`; excludes dev-only and unrelated workspace packages |
| Direct dependency packages | Same graph, first-level edges only | Directly required normal/build packages; not all items from Cargo.toml |
| Existing binary size | `stat().st_size` on executable already built by existing CI | Raw disk bytes of unstripped/stripped binary as produced by that job; not installed or compressed footprint |
| Cached CI build time | Elapsed wall seconds around existing `cargo build` | **Not** a cold build benchmark or an inter-machine language ranking |

- `minimal`: `--no-default-features`.
- `release-cli`: `--no-default-features --features release-cli`.
- `full`: `--no-default-features --features full`, equivalent to the current default feature aggregate.

The CI workflow records profile dependency graphs and existing build measurements as short-lived artifacts from the **same exact head** of the pull request. Metadata for all three profiles is collected in the minimal-build job on the same machine/target for fair dependency-closure comparison. The separate full and release jobs size binaries **already built** there; this adds no extra compilation.

## Local usage

From the repository root with the pinned Rust toolchain installed:

```bash
python3 -m unittest scripts/tests/test_profile_entropy.py
python3 scripts/profile_entropy.py \
  --profile minimal --profile release-cli --profile full \
  --output /tmp/nulang-profile-dependencies.json
```

For an existing compiled binary, optionally add `--binary path/to/nulang`; pass `--build-seconds N` only when `N` was measured during a build. A missing binary produces `null`, **never 0 bytes**. Metadata failures return nonzero and do not produce a misleading empty baseline.

## Important limits

- Package *resolution* is not proof that all packages were compiled or linked. Target filtering still counts Cargo's activated normal/build graph, not conditional compiler paths that Cargo does not expose precisely.
- `--filter-platform` counts are **not** directly comparable to unfiltered metadata counts. The 151-package unfiltered minimal closure observed on the initial RFC 0033 PR is not a Linux target-filtered baseline.
- Counts and sizes are only comparable for the same target triple, feature set, release/debug profile, Rust/Cargo versions, compiler flags, and build context.
- CI build durations include cache/runner variance. Before creating numerical regression gates, run controlled repeated **cold** build measurements using isolated target directories on a dedicated runner.
- A Rust dependency upgrade can reduce risk even if package count increases. Dependency count is not a substitute for advisory scanning or audit.
- These artifacts do not certify security, recovery, WASM parity, or release readiness.

## After the first exact-head artifacts

1. Record actual minimal / release-cli / full counts and package deltas from the artifacts.
2. Identify optional packages leaked into the minimal closure; require fixes before widening the core. Coordinate with PR #2287.
3. Assess which *required* dependencies create high maintenance risk (churn, security, portability, custom transitive graphs); recommend extraction only where evidence supports it.
4. Establish a stable same-runner cold-build experiment before setting timing/size budgets.
5. Keep all runtime, durability, authority, format and advisory gates intact. Do not merge a feature-profile change merely because metric collection succeeds.
