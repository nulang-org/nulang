# Cargo-built Rust adapter

Demonstrates an opt-in Rust dependency called by a Nulang program.

- Nulang.toml selects a Cargo manifest under the package root.
- Cargo builds the adapter as a C-compatible cdylib using its Cargo.lock.
- nula copies the reported Cargo artifact into .nula/native/.
- The nula-native:crc_bridge FFI alias resolves the platform-specific filename.
- Nulang types the declaration, while only explicit C ABI exports are callable.

Requirements: Rust/Cargo, a native Nulang executable built with the ffi feature.
From this directory run:

    nulang nula run

or:

    nulang nula build

The first build requires access to the locked crc32fast crate if it is not
already cached. Commit native/Cargo.lock alongside the source. Cargo's
lockfile is independent of Nulang.lock. Untrusted Rust crates or build.rs
scripts execute with the build user's privileges; this is not sandboxed.

Native dynamic libraries are not supported by nula build-wasm. Use a WASM
Component Model adapter for portable/sandboxed integrations when available.
