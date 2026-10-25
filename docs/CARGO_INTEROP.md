# Cargo interoperability: native adapter MVP

Nulang remains a separate language and package manager. Cargo is used only
for explicitly opted-in Rust dependencies, through a C-compatible native
adapter. No claim is made that Nulang can compile arbitrary Rust crate APIs,
Rust types, traits, async futures, or procedural macros.

## Configure a package

Add to your Nulang.toml:

    [rust]
    manifest = "native/Cargo.toml"

In native/Cargo.toml, define exactly one cdylib target and any Rust
dependencies supported by Cargo. Generate and commit native/Cargo.lock.
Example: examples/cargo-native/ uses the crc32fast crate.

Use a platform-independent alias from Nulang:

    extern "nula-native:crc_bridge" {
        fn crc32_i64(value: Int) -> Int
    }

The alias must match Cargo's cdylib target name. Only C-compatible exports
from an explicit Rust adapter are callable. Nulang's existing FFI type checker
checks the declared signature; it cannot prove a Rust export actually matches
the declared ABI. Keep native pointer and allocation ownership explicit.

## Build and run

Run from the package root:

    nulang nula build
    nulang nula run
    nulang nula test

The package manager invokes cargo metadata and cargo build --locked --release.
It uses Cargo's compiler-artifact JSON output to find the built cdylib,
independently of CARGO_TARGET_DIR and .cargo/config.toml, then stages the
platform-specific library under .nula/native. nula clean removes staged native
libraries as well as Nulang build artifacts. The adapter is rechecked by Cargo
on each package preparation; incremental Cargo compilation avoids redundant
work. The adapter's Cargo.lock is separate from Nulang.lock.

Native adapters require Cargo at build time and a runtime with native FFI
enabled. A prebuilt Nulang executable can run compiled applications without
Cargo, provided the native library has been staged and its operating-system
dependencies are available.

## Boundaries and limitations

- The Rust adapter must be a package-local Cargo.toml path. Absolute paths,
  parent traversal and symlink escapes are rejected.
- --locked requires a committed Cargo.lock; dependencies can require network
  access if absent from Cargo's local cache.
- Rust crate build scripts and proc macros execute at build time; native shared
  libraries execute with the application's privileges. Neither is sandboxed.
- Dynamic FFI is unsuitable for untrusted code and may be prohibited by the
  target platform. Use the existing FFI sandbox/allowlist for tighter control.
- nula build-wasm rejects packages declaring a native Cargo adapter instead
  of silently generating an artifact with unsupported native dependencies.
- Rust ABI types are intentionally not directly mapped. Advanced typed
  bindings and WASM Component Model support require separate implementations.

Self-hosting Nulang's full compiler is independent of this bridge and remains
a separate bootstrap milestone.

Deployment: nula deploy (including dry-run and WASM modes) rejects native
Cargo adapters until deployment archives and target runtimes explicitly
support shipping and loading native shared libraries. Local build/run remain supported.
