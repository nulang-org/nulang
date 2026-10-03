//! Database-engine building blocks.
//!
//! These primitives intentionally keep database storage/execution concerns
//! separate from actor semantics. Actors may own and coordinate tablets, but
//! inner storage and query loops remain ordinary local computation.

pub mod checkpoint;
pub mod lsm;
pub mod managed_lsm;
pub(crate) mod sstable;
pub mod store;
pub mod tablet;
pub mod wal;

#[cfg(test)]
mod interruption;
