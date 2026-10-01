//! Portable structured-concurrency IR marker vocabulary.
//!
//! Kept separate from runtime-specific primitives so the browser playground
//! can reuse HIR/MIR lowering without pulling in the native actor runtime.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParallelRegionMarker {
    Begin { branches: u32 },
    Branch { index: u32 },
    End,
}
