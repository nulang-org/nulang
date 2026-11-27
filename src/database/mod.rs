//! Database-engine building blocks.
//!
//! These primitives intentionally keep database storage/execution concerns
//! separate from actor semantics. Actors may own and coordinate tablets, but
//! inner storage and query loops remain ordinary local computation.

pub mod checkpoint;
pub mod indexed_sst;
pub mod optimistic;
#[cfg(unix)]
pub mod sst_manifest;
pub mod store;
pub mod tablet;
pub mod wal;
#[cfg(unix)]
mod wal_sst_shadow;

#[cfg(test)]
mod interruption;
#[cfg(test)]
mod model_tests;

#[cfg(test)]
mod receipt_tests;
