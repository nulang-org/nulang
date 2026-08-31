#![allow(
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::missing_transmute_annotations
)]

pub mod agent;
pub mod aot;
pub mod ast;
pub mod backends;
#[cfg(test)]
pub mod benchmarks;
pub mod bytecode;
#[cfg(feature = "wasmfx-backend")]
pub mod cir;
#[cfg(feature = "wasmfx-backend")]
pub mod cir_analysis;
#[cfg(feature = "wasmfx-backend")]
pub mod cir_lower;
pub mod core_vm;
pub mod cranelift_utils;
pub mod dap;
pub mod diagnostic;
pub mod difffuzz;
pub mod docgen;
pub mod dst;
pub mod effect_checker;
pub mod ffi;
pub mod fmt;
pub mod format;
pub mod fuzz;
pub mod hir;
pub mod hir_lower;
pub mod integration_tests;
pub mod iso_arena;
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
pub mod mir_wasm_simd;
#[cfg(feature = "otel")]
pub mod observability;
pub mod package;
pub mod parser;
pub mod prelude_source;
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
pub mod typechecker;
pub mod types;
pub mod value_layout;
pub mod vm;
#[cfg(feature = "wasm-backend")]
pub mod wasm_component_runtime;
#[cfg(feature = "wasm-backend")]
pub mod wasm_runtime;
pub mod wasm_types;
#[cfg(feature = "wasmfx-backend")]
pub mod wasmfx_backend;
#[cfg(feature = "wasmfx-backend")]
pub mod wasmfx_runtime;
pub mod web;
pub mod witgen;
