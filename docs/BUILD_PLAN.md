# Read-only package build inspection

`nulang nula plan --json` prints a versioned, machine-readable inspection of
the current package and its **existing** dependency lockfile. It does not
resolve dependencies, fetch source, compile, modify `Nulang.lock`, or
publish an artifact. The `--json` flag is optional; output is always JSON.

## What the result means

- `package.source_id`: canonical `Nulang.toml` and `.nula` source identity
  using the existing compiler-owned `SourceId` implementation.
- `dependencies[].verification = local-source-verified`: the path
  dependency exists and its current canonical source identity matches the
  saved strong identity in `Nulang.lock`.
- `dependencies[].verification = commit-pinned-content-unverified`:
  the Git dependency has a complete commit pin; the checkout's contents
  have **not** been independently verified by this command.
- `artifact.semantic_id`, `artifact.artifact_id`, `artifact.digest`:
  always `null` because no compiler-generated semantics or executable
  artifact was produced.
- `cloud_admissible`: always `false`.

The inspection fails closed for an absent lockfile when dependencies are
declared, missing/mismatched path identities, missing dependency pins,
unsupported lockfile sources, and Git entries without complete commit pins.

## Security and reproducibility limits

This is an **inspection document**, not a hermetic-build attestation, a
complete transitive dependency proof, or an RFC 0020 Behavior Manifest.
`SourceId` currently covers canonical source files and the package manifest,
not arbitrary linked assets, environment inputs, compiler flags, or host tools.
An existing lockfile may be stale relative to indirect dependency resolution.

Nulang Cloud MUST NOT admit or deploy this JSON as a substitute for the
compiler-produced versioned Behavior Manifest, exact executable bytes,
artifact digest, and runtime ABI/WIT compatibility checks.

Future incremental compilation may consume these observations only as
diagnostic hints. Authoritative cache keys and deployment claims require
complete input closure and compiler-owned semantic/artifact identities.

## Developer checks

Run the focused unit tests with:

```sh
cargo test --lib package::build_plan::tests
```

Inspect a package without changing its files:

```sh
nulang nula plan --json
```

After any compiler or source-cache integration, additionally compare
clean versus cached builds and verify both executable digests.
