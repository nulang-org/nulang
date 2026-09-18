# RISC-V 64 support

Nulang supports 64-bit RISC-V Linux as an explicitly tested portability target.

## Supported target

The compatibility target is Rust's `riscv64gc-unknown-linux-gnu` target (RV64 with the standard `G` extension set plus compressed instructions, using glibc).

The supported baseline covers:

- the Nulang frontend and bytecode VM;
- actor/runtime code that is included in a no-default-features build;
- Cranelift-backed tiered JIT code generation and execution;
- the Cranelift AOT/native backend (`--backend native`);
- the `nulang-capacity` crate, including `Architecture::Riscv64` placement constraints;
- execution under a real riscv64 Linux userspace or QEMU user-mode emulation.

The repository's RISC-V workflow cross-compiles this configuration on every pull request. Under QEMU it runs the bytecode VM, forces a hot bytecode loop to tier up into the JIT and verifies that compilation occurred, and separately executes an AOT/native program whose result must equal `42`.

## Build from a RISC-V Linux machine

```bash
cargo build --no-default-features --features native-codegen --bin nulang
```

For the complete default feature set, install the native development libraries required by the enabled integrations (for example Python development files for the `python` feature).

The compatibility CI intentionally uses `--no-default-features --features native-codegen`. It therefore validates the language runtime and native-codegen stack without claiming that every optional native integration has been cross-tested. `build.rs` includes GNU multiarch discovery for native RISC-V Python installations (`riscv64-linux-gnu`), but Python is not part of this minimal CI contract.

## Cross-compile from Debian/Ubuntu x86-64

Install the GNU RISC-V toolchain and Rust target:

```bash
sudo apt-get install gcc-riscv64-linux-gnu binutils-riscv64-linux-gnu libc6-dev-riscv64-cross qemu-user
rustup target add riscv64gc-unknown-linux-gnu
```

Then build:

```bash
CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \
CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc \
AR_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-ar \
TARGET_CC=riscv64-linux-gnu-gcc \
TARGET_AR=riscv64-linux-gnu-ar \
cargo build \
  --target riscv64gc-unknown-linux-gnu \
  --no-default-features \
  --features native-codegen \
  --bin nulang
```

Run it under QEMU with:

```bash
qemu-riscv64 -L /usr/riscv64-linux-gnu \
  target/riscv64gc-unknown-linux-gnu/debug/nulang examples/01_hello.nula
```

## Native code generation

`cranelift-codegen` is compiled with its `riscv64` backend enabled in `Cargo.toml`. Both Nulang's tiered JIT and its AOT/native backend ask `cranelift-native` for the host ISA when targeting `native`, so a Nulang process running on riscv64 generates riscv64 machine code rather than assuming x86-64 or AArch64.

The two native-codegen paths are tested separately. The tiered-JIT gate runs a bytecode program long enough to cross `HOT_THRESHOLD`, verifies that at least one region was JIT-compiled, and checks its result against interpreter semantics. The AOT gate invokes `--backend native` and requires the generated program to return the expected result.

Nulang's hand-selected SIMD fast path is currently enabled only where its architecture-specific capability check succeeds. On riscv64 it falls back to scalar codegen; RISC-V Vector Extension (RVV) optimization is not part of the compatibility baseline.

## Capacity placement

`nulang-capacity::Architecture` includes `Riscv64`, serialized as `riscv64`. Jobs can therefore require RISC-V explicitly and the broker treats architecture as a hard eligibility constraint rather than a ranking preference. A RISC-V workload cannot be silently placed on an x86-64 or Arm64 offer.

## Portability notes

The native tests deliberately execute generated machine code rather than only cross-compiling the Rust binary. The workflow also checks the produced executable's ELF header reports `Machine: RISC-V`. Together these catch target-ISA regressions that a compile-only check would miss.

The JIT callback bridge still stores a Rust trait-object pointer across a short thread-local execution boundary. That code is exercised by native-codegen paths, but Rust does not specify trait-object representation as a stable ABI. Removing that representation assumption is tracked in #328 as portability hardening rather than as a prerequisite for the RV64 Linux baseline.

Nulang's current `Value::ptr` representation carries a 48-bit address payload. RISC-V CI checks representative bump, chained-block, and large-object-space allocations fit that representation in the tested RV64 Linux environment. Making heap references safe on arbitrary wider-VA hosts is tracked separately in #330; the compatibility baseline must not be read as a claim about every possible RISC-V virtual-address mode.

## Scope and non-goals

This target is 64-bit Linux. It does not imply support for RV32, bare-metal RISC-V, RISC-V Android, RVV acceleration, or every optional integration when cross-compiling. Those should be added as separate targets with their own CI coverage instead of being inferred from the riscv64 Linux build.

A target is considered supported only while its CI cross-build and QEMU execution tests pass. This prevents architecture compatibility from being documentation-only.
