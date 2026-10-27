# Core CLI release qualification (pre-production)

Nulang's public release binary uses `cargo build --locked --release --no-default-features --features release-cli --bin nulang`. The original `conformance/run.py` built a separate **debug** binary, which did not demonstrate the public release CLI's behavior. The runner now accepts an explicit executable path so the same built artifact is checked against the repository's complete language behavior corpus.

## Execute the gate

After building the exact release profile in an isolated checkout:

```sh
cargo build --locked --release --no-default-features --features release-cli --bin nulang
target_dir="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
python3 conformance/run.py --binary "$target_dir/release/nulang"
```

The manually dispatched [Core release CLI conformance qualification](../.github/workflows/core-release-qualification.yml) workflow builds the same profile and runs the complete suite, **failing on any mismatch, missing fixture, missing executable, or zero executed tests**. It is not a replacement for the repository's separate default, no-default, WASM, release, security, and cross-platform gates.

Without `--binary`, the existing runner still builds and tests its debug executable; do not use the implicit mode as evidence about published release artifacts. A passed binary-selection test proves the harness selected the requested executable; it does not demonstrate language conformance.

## Admission policy

- **RED until proven green:** The full behavior corpus currently contains failing fixtures. Never shrink the corpus, silently skip or rewrite expectations, mark this job `continue-on-error`, or label an incomplete run a production qualification.
- **No promotion from this workflow alone:** Even an entirely green release CLI conformance run does not certify the native actor runtime, distributed Fabric, NuDB, security, external durable effects, or backend semantic parity.
- **Exact-artifact provenance:** Retain the commit SHA, release profile, platform, and executable checksum with a successful qualification record. A different build or release tag must rerun qualification on its own artifact.
- **No weakening of branch protections:** This workflow is manual and non-required during remediation. Require an independently reviewed, passing exact-head gate before certifying a production Core release.
- **Failure triage:** Record each failing fixture and classify compiler crash, expected/actual semantic mismatch, unsupported experimental feature, runtime fault, or timeout. Preserve its original expectation until a reviewed correction demonstrates the intended semantics.

This qualification path is deliberately separate from the draft stabilization/security integration. It is not authorization to merge that integration or deploy Nulang into critical workloads.
