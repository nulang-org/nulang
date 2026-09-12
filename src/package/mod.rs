//! `nula` — the Nulang package manager (MVP).
//!
//! A package is a directory with a `Nulang.toml` manifest ([`manifest`]) and
//! an entry point (`src/main.nula` by default). Resolving a package's
//! dependencies ([`resolver`]) produces a `Nulang.lock` lockfile
//! ([`lockfile`]). Only local-path and git dependencies are supported; there
//! is no network registry yet.
//!
//! The CLI subcommands (`nula new|init|build|build-wasm|test|run|list|clean`)
//! live in [`commands`] and are dispatched from `main.rs` when the first
//! argument is `nula`.

pub mod commands;
pub mod identity;
pub mod lockfile;
pub mod manifest;
pub mod resolver;