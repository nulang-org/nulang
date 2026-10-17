# Experimental native executable slice

This document describes the first independently linked native executable path.
It is **not** the interpreter/JIT-backed `--backend native` path and it is
**not** full-language AOT parity.

## Linux smoke

With Rust, a native C linker (`cc`) and the opt-in `native-executable` Cargo feature:

```sh
cat > hello.nula <<'EOF'
perform IO.print("Hello from Nulang")
EOF
cargo run --locked --release --features native-executable -- --emit-exe --out hello hello.nula
./hello
file hello
ldd hello
stat -c '%s bytes' hello
```

The output is an ordinary host-native executable linked to system libc
(`puts`). It does **not** embed the Nulang compiler, VM, JIT, actor scheduler,
durability stack, or package manager. The emitted artifact is a single
executable file, but is **dynamically linked**: libc and the Linux dynamic
loader must be available on its target machine.

Supported in this prototype:

- A synthetic `__main` (or `main`) entry with no parameters, a single basic
  block, no handlers, and a Unit result.
- UTF-8 string constants, integer constants, local copies, negation, and
  integer addition/subtraction/multiplication. Straight-line integer expressions
  are folded at compile time with the VM's signed **48-bit wraparound** semantics.
- `perform IO.print(string)` and `IO.println(string)`, implemented through
  host `puts`, including their terminating newline.

Not supported: function calls, branching/loops, integer result printing,
dynamic strings, arbitrary effects, actors, FFI, suspension, GC-managed
objects, durability, distribution, or native-library independence.
Unsupported MIR returns an error **before writing or installing an executable**.
NUL-containing string literals are rejected rather than truncated.

## Focused validation

```sh
cargo test --locked --no-default-features --features native-executable --test native_tiny_executable
cargo check --locked --no-default-features
bash scripts/check_mobile_runtime_profile.sh
```

For a repeatable Linux size and stdout-parity report:

```sh
cargo build --locked --release --no-default-features --features native-executable --bin nulang
cargo_target="$(cargo metadata --locked --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
bash scripts/measure_native_binaries.sh "$cargo_target/release/nulang" "$cargo_target/native-size-report"
cat "$cargo_target/native-size-report/report.tsv"
```

The script always compares Nulang against C, and adds Go, TinyGo, and Rust
baselines only when the corresponding compiler is available. It fails if
binary output differs from the same expected stdout fixture. Artifacts and
the tab-separated report are retained in a fresh output directory. All
executables use size-directed build settings and symbol stripping where
supported; compiler versions, runtime/linker defaults, and library dependencies
still differ. This is a smoke/measurement harness, not a statistically
controlled performance study.

Compare **identical programs and linking requirements**, not a
dynamically linked Nulang hello-world against a fully static Go binary.
The size and linkage report does not establish startup or memory efficiency.

## Process overhead probe (Linux)

The opt-in native CI lane also produces `native-runtime-profile.json` for the
same emitted Nulang, C, and available Rust/Go/TinyGo executables:

```sh
python3 scripts/measure_native_runtime.py --runs 21 --warmup 3 \
  --json-out native-runtime-profile.json \
  "nulang=/absolute/path/hello-nulang" \
  "c=/absolute/path/hello-c"
```

The harness compiles a small C probe that uses `fork`, `exec`, `wait4`
and a monotonic clock to capture process launch-to-exit wall time, child CPU
time and child peak RSS (Linux KiB). The C probe avoids attributing Python's
large memory footprint to tiny child processes, which can happen when Python
`posix_spawn` uses `CLONE_VM`. Executables must exit successfully. Stdout and
stderr are redirected to `/dev/null`; functional output parity is checked by
the separate size harness first.

These are **warm-cache process lifecycle samples**, not true cold starts or
isolated application initialization measurements. Wall time includes fork/exec
and kernel scheduling. CI shared-host timing variance makes the samples
informational, **not a blocking relative-performance gate**. For trustworthy
comparisons, rerun multiple times on stable, identical hardware, with the
same linkage model and program behavior. Each artifact has a **30-second
overall sampling timeout** by default (adjustable using `--timeout-seconds`).
A timeout terminates the entire measurement process group and fails the
smoke run rather than leaving CI blocked by a hung executable.

## Promotion gates

1. Exact-head `cargo test --locked` and native executable smoke pass on Linux.
2. Output parity with bytecode on supported fixtures; unsupported constructs
   fail closed and leave no executable artifact.
3. The `mobile-runtime` feature gate continues excluding Cranelift and
   dynamic FFI.
4. Security review of the linker input, temporary files and artifact replacement.
5. Dedicated binary-size benchmark against Go, TinyGo and a native C baseline
   with comparable libc/static-linking settings.

Do not ship this backend as a production deployment or advertise a binary-size
advantage until these gates and reproducible measurements are satisfied.
