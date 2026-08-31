//! Nulang Cloud adapter — the default deployment target.
//!
//! The cloud adapter does not generate extra files in `dist/`; the tarball sent
//! to Nulang Cloud includes `dist/nulang-app.ir.json` and the static assets
//! directly, and the cloud runtime interprets the IR.

pub const NAME: &str = "nulang_cloud";
