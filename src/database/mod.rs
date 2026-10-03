//! Database-engine building blocks.
//!
//! These primitives intentionally keep database storage/execution concerns
//! separate from actor semantics. Actors may own and coordinate tablets, but
//! inner storage and query loops remain ordinary local computation.

pub mod checkpoint;
pub mod manifest;
pub mod owner;
pub mod sstable;
pub(crate) mod sstable_indexed;
pub(crate) mod sstable_v2;
pub mod store;
pub mod tablet;
pub mod wal;
pub mod wal_batch;

#[cfg(test)]
mod fallible_read_tests;
#[cfg(test)]
mod interruption;
#[cfg(test)]
mod manifest_v2_tests;
#[cfg(test)]
mod sstable_compaction_tests;
#[cfg(test)]
mod sstable_indexed_tests;
#[cfg(test)]
mod sstable_mmap_tests;
#[cfg(test)]
mod sstable_v2_serving_tests;
#[cfg(test)]
mod sstable_v2_tests;
