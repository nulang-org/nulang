# RISC-V 64 support

Nulang supports 64-bit RISC-V Linux as an explicitly tested portability target.

## Supported target

The compatibility target is Rust's `riscv64gc-unknown-linux-gnu` target (RV64 with the standard `G` extension set plus compressed instructions, using glibc).

The supported baseline covers:

- the Nulang frontend and bytecode VM;
- actor/runtime code that is included in a no-default-features build;
- Cranelift-backed native code generation and JIT execution;
- the `nulang-capacity` crate;
- execution under a real riscv64 Linux userspace or QEMU user-mode emulation.

The repository's RISC-V workflow cross-compiles this configuration on every pull request and runs both bytecode and native-codegen smoke tests under QEMU.

## Build from a RISC-V Linux machine

```bash
cargo build --no-default-features --features native-codegen --bin nulang
```

For the complete default feature set, install the native development libraries required by the enabled integrations (for example Python development files for the `python` feature).

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

`cranelift-codegen` is compiled with its `riscv64` backend enabled in `Cargo.toml`. Nulang's JIT/AOT implementation asks `cranelift-native` for the host ISA, so a Nulang process running on riscv64 generates riscv64 code rather than assuming x86-64 or AArch64.

Nulang's hand-selected SIMD fast path is currently enabled only where its architecture-specific capability check succeeds. On riscv64 it falls back to scalar codegen; RISC-V Vector Extension (RVV) optimization is not part of the compatibility baseline.

## Scope and non-goals

This target is 64-bit Linux. It does not imply support for RV32, bare-metal RISC-V, RISC-V Android, or every optional integration when cross-compiling. Those should be added as separate targets with their own CI coverage instead of being inferred from the riscv64 Linux build.

A target is considered supported only while its CI cross-build and QEMU smoke tests pass. This prevents architecture compatibility from being documentation-only.
