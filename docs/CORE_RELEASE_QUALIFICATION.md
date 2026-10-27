# Core CLI release qualification (pre-production)

Nulang's public release binary uses `cargo build --locked --release --no-default-features --features release-cli --bin nulang`. The original `conformance/run.py` built a separate **debug** binary, which did not demonstrate the public release CLI's behavior. The runner now accepts an explicit executable path so the same built artifact is checked against the repository's complete language behavior corpus.

## Execute the gate

After building the exact release profile in an isolated checkout:

```sh
cargo build --locked --release --no-default-features --features release-cli --bin nulang
target_dir="$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
python3 conformance/run.py --binary "$target_dir/release/nulang"
```

The [Core release CLI conformance qualification](../.github/workflows/core-release-qualification.yml) workflow is triggered by qualifying pull-request paths, so its **first review can run before the workflow exists on `main`**. Once installed on the default branch, it can also be dispatched manually. It builds the same release profile and runs the complete suite, **failing on any mismatch, missing fixture, missing executable, or zero executed tests**. It is not a replacement for the repository's separate default, no-default, WASM, release, security, and cross-platform gates.

Without `--binary`, the existing runner still builds and tests its debug executable; do not use the implicit mode as evidence about published release artifacts. A passed binary-selection test proves the harness selected the requested executable; it does not demonstrate language conformance.

## Explicit resource-capability fixtures

A behavior expectation may opt into a minimal resource grant for **that one case** with `"capabilities": ["fs"]`, `"capabilities": ["net"]`, or `"capabilities": ["os"]`. The conformance runner validates a nonempty, unique subset of those three exact names before execution and adds `--with fs` (or the corresponding comma-separated list) to that executable invocation only. All fixtures without the field remain **ungranted**; unknown capabilities, raw flags, empty sets, duplicates and invalid types are not accepted as grants. The existing expected exit code, stdout, and stderr remain binding.

The FS and Process tests now explicitly declare their required resource permission rather than weakening the CLI's default-deny admission policy. The closed-body/open-effect-row fixture likewise declares `fs` because its public function signature includes FS even when its body only prints. This **does not** authorize network listeners or broader system access globally. Do not auto-grant missing permissions merely to obtain a green suite. There is a separate `System.arg(1)`/CLI-flag argv semantic mismatch to investigate; it is intentionally not hidden by this change.

On unexpected exit codes or stdout, the runner prints a bounded excerpt of actual stderr so resource denials can be distinguished from compiler/runtime defects.

## Admission policy

- **RED until proven green:** The full behavior corpus currently contains failing fixtures. Never shrink the corpus, silently skip or rewrite expectations, mark this job `continue-on-error`, or label an incomplete run a production qualification.
- **No promotion from this workflow alone:** Even an entirely green release CLI conformance run does not certify the native actor runtime, distributed Fabric, NuDB, security, external durable effects, or backend semantic parity.
- **Exact-artifact provenance:** Retain the commit SHA, release profile, platform, and executable checksum with a successful qualification record. A different build or release tag must rerun qualification on its own artifact.
- **No weakening of branch protections:** This workflow is manual and non-required during remediation. Require an independently reviewed, passing exact-head gate before certifying a production Core release.
- **Failure triage:** Record each failing fixture and classify compiler crash, expected/actual semantic mismatch, unsupported experimental feature, runtime fault, or timeout. Preserve its original expectation until a reviewed correction demonstrates the intended semantics.

This qualification path is deliberately separate from the draft stabilization/security integration. It is not authorization to merge that integration or deploy Nulang into critical workloads.
