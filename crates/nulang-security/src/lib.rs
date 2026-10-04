//! Shared security primitives for Nulang runtime and agent components.
//!
//! The crate intentionally contains only dependency-light identity, time, and
//! delegation metadata. Runtime capability semantics remain in the runtime
//! crate, while application policy engines can depend on these primitives
//! without pulling in the compiler or VM.
