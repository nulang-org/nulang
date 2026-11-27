//! Portable parser dependencies only; no native Web host is linked.

#[path = "../../../src/web/accessibility.rs"]
pub(crate) mod accessibility;
#[allow(dead_code)] // The parser uses the shared void-element predicate only.
#[path = "../../../src/web/html_safety.rs"]
pub(crate) mod html_safety;
