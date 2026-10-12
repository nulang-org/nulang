//! Compatibility adapters for external runtimes and protocols.
//!
//! Compatibility code translates foreign protocol concepts into Nulang's
//! canonical execution, durability, and effect primitives. Protocol-specific
//! types must not leak into `runtime`, persistence backends, or the language
//! surface.

pub mod temporal;
