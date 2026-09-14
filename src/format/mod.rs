//! Durable-format infrastructure for Nulang.
//!
//! This module owns the canonical, **frozen** format-version constants and
//! the `.nbc` (Nulang Bytecode) artifact codec. It is the layer that lets a
//! program compiled in one era run in another: every durable artifact carries
//! a magic + version header, and a runtime that does not understand a
//! version rejects it rather than reinterpreting it.
//!
//! See `SPEC2.md` §"Format Stability" for the contract, and the submodules
//! for the concrete codecs:
//!
//! - [`constants`] — magic bytes, version numbers, [`constants::FormatError`].
//! - [`nbc`] — `CodeModule::to_nbc` / `from_nbc` and [`nbc::NbcArtifact`].
//! - [`mobile_nbc`] — additive mobile-action authorization metadata.
//! - [`migrate`] — the only legal place format-version upgrades live.

pub mod constants;
pub mod migrate;
pub mod mobile_nbc;
pub mod nbc;
