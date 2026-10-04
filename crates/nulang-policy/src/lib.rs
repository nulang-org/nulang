//! Pure application authorization policy kernel.
//!
//! This crate is intentionally independent of the Nulang compiler/runtime.
//! It consumes shared principals from `nulang-security` and evaluates trusted
//! application facts with deterministic deny-by-default semantics.
