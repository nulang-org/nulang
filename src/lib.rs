#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::missing_transmute_annotations
)]
// Test fixtures intentionally exercise source-level decimal spellings such as 3.14;
// replacing those values with mathematical constants would change test semantics.
#![cfg_attr(test, allow(clippy::approx_constant))]

mod actor_protocol;
pub mod agent;
#[cfg(feature = "native-codegen")]
pub mod aot;
pub mod artifact_identity;
pub mod ast;
pub mod authority;
pub mod authority_host;
mod authority_runtime;
pub use authority_runtime::RuntimeAuthorityError;
pub mod backends;
pub mod behavior_identity;
#[cfg(test)]
pub mod benchmarks;
pub mod bytecode;
#[cfg(feature = "wasmfx-backend")]
pub mod cir;
#[cfg(feature = "wasmfx-backend")]
pub mod cir_analysis;
#[cfg(feature = "wasmfx-backend")]
pub mod cir_lower;
pub mod content_identity;
pub mod core_vm;
#[cfg(feature = "native-codegen")]
pub mod cranelift_utils;
pub mod dap;
pub mod diagnostic;
#[cfg(feature = "native-codegen")]
pub mod difffuzz;
pub mod docgen;
pub mod dst;
pub mod durable_effect;
pub mod durable_effect_persistence;
pub mod effect_checker;
pub mod ffi;
pub mod fmt;
pub mod format;
#[cfg(feature = "native-codegen")]
pub mod fuzz;
pub mod hir;
#[path = "hir_lower_nominal.rs"]
pub mod hir_lower;
pub mod integration_tests;
pub mod iso_arena;
#[cfg(feature = "native-codegen")]
pub mod jit;
pub mod json_diagnostics;
pub mod lexer;
#[cfg(feature = "lsp")]
pub mod lsp;
pub mod mir;
pub mod mir_codegen;
pub mod mir_inline;
pub mod mir_lower;
#[cfg(feature = "wasm-backend")]
pub mod mir_wasm;
#[cfg(feature = "wasm-backend")]
pub mod mir_wasm_simd;
#[cfg(feature = "otel")]
pub mod observability;
pub mod package;
pub mod parser;
pub mod prelude_source;
pub mod primitives;
pub mod protocol;
pub mod protocol_wire;
#[cfg(feature = "python")]
pub mod python;
pub mod registry;
pub mod repl;
pub mod resolver;
pub mod runtime;
pub mod stdlib;
#[cfg(test)]
pub mod stress_tests;
pub mod tool_schema;
pub mod type_ir;
pub mod type_metadata;
#[path = "typechecker_facade.rs"]
pub mod typechecker;
#[path = "typechecker.rs"]
pub(crate) mod typechecker_base;
pub mod types;
pub mod value_layout;
pub mod vm;
#[cfg(feature = "wasm-backend")]
pub mod wasm_component_runtime;
#[cfg(feature = "wasm-backend")]
pub mod wasm_runtime;
#[cfg(feature = "wasm-backend")]
pub mod wasm_types;
#[cfg(feature = "wasmfx-backend")]
pub mod wasmfx_backend;
#[cfg(feature = "wasmfx-backend")]
pub mod wasmfx_runtime;
pub mod web;
pub mod witgen;
