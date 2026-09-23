//! Nulang browser playground — compiler front-end + CoreVM compiled to WASM.
//!
//! This crate re-uses the compiler's own sources via `#[path]` includes so
//! the playground can never drift from the real language: the lexer, parser,
//! typechecker, HIR/MIR lowering, bytecode codegen, and the CoreVM
//! interpreter below are *the same files* the native `nulang` binary uses.
//!
//! Only the portable, pure-Rust subset of the compiler is included. Native
//! subsystems (Cranelift JIT, WASM backend, FFI loading, actors/networking,
//! Python interop, LSP) are deliberately out of scope for the browser.
//!
//! Build:
//! ```sh
//! cargo build --release --target wasm32-unknown-unknown \
//!   --manifest-path crates/nulang-playground/Cargo.toml
//! ```
//! The resulting `nulang_playground.wasm` has a tiny C-style ABI (see the
//! `nulang_*` exports at the bottom) and needs no JS glue generator.

// -- Compiler front-end (shared sources) ------------------------------------

#[path = "../../../src/types.rs"]
pub mod types;
#[path = "../../../src/type_ir.rs"]
pub mod type_ir;
#[path = "../../../src/diagnostic.rs"]
pub mod diagnostic;
#[path = "../../../src/lexer.rs"]
pub mod lexer;
#[path = "../../../src/ast.rs"]
pub mod ast;
#[path = "../../../src/authority.rs"]
pub mod authority;
#[path = "../../../src/actor_protocol.rs"]
pub mod actor_protocol;
#[path = "../../../src/effect_checker.rs"]
pub mod effect_checker;
#[path = "../../../src/prelude_source.rs"]
pub mod prelude_source;
#[path = "../../../src/parser.rs"]
pub mod parser;
#[path = "../../../src/stdlib.rs"]
pub mod stdlib;
#[path = "../../../src/typechecker.rs"]
pub mod typechecker;
#[path = "../../../src/tool_schema.rs"]
pub mod tool_schema;
#[path = "../../../src/hir.rs"]
pub mod hir;
#[path = "../../../src/hir_lower.rs"]
pub mod hir_lower;
#[path = "../../../src/type_metadata.rs"]
pub mod type_metadata;
#[path = "../../../src/bytecode.rs"]
pub mod bytecode;
#[path = "../../../src/format/mod.rs"]
pub mod format;
#[path = "../../../src/mir.rs"]
pub mod mir;
#[path = "../../../src/mir_inline.rs"]
pub mod mir_inline;
#[path = "../../../src/mir_lower.rs"]
pub mod mir_lower;
#[path = "../../../src/mir_codegen.rs"]
pub mod mir_codegen;
#[path = "../../../src/value_layout.rs"]
pub mod value_layout;
#[path = "../../../src/core_vm/mod.rs"]
pub mod core_vm;

// -- Native-only shims -------------------------------------------------------
//
// `mir_codegen` maps Nulang types onto FFI types for `extern` declarations.
// The real mapping lives in `src/ffi/marshal.rs` alongside libloading-based
// native calls, which cannot exist in a browser. The pure type mapping is
// reproduced here (same logic, same types) so `extern` declarations still
// type-check and compile; actually *calling* a native function is rejected
// by the CoreVM, which has no FFI.

pub mod ffi {
    pub mod marshal {
        /// Map a Nulang type to its FFI representation.
        /// Mirrors `src/ffi/marshal.rs::nulang_type_to_ffi_type`.
        pub(crate) fn nulang_type_to_ffi_type(
            ty: &crate::types::Type,
        ) -> Option<crate::bytecode::FfiType> {
            use crate::bytecode::FfiType;
            use crate::types::{PrimitiveType, Type};
            match ty {
                Type::Primitive(p) => match p {
                    PrimitiveType::Int => Some(FfiType::Int),
                    PrimitiveType::Float => Some(FfiType::Float),
                    PrimitiveType::Bool => Some(FfiType::Bool),
                    PrimitiveType::String => Some(FfiType::String),
                    PrimitiveType::Unit => Some(FfiType::Unit),
                    _ => None,
                },
                _ => None,
            }
        }
    }
}

// -- Compile + run pipeline --------------------------------------------------

use std::cell::RefCell;
use std::rc::Rc;

/// The result of one playground run, serialized to JSON for the JS side.
#[derive(serde::Serialize)]
pub struct RunResult {
    /// Whether compilation and execution both succeeded.
    pub ok: bool,
    /// Everything the program printed via `IO.print`, plus the final value
    /// of `main` when it is a non-unit value.
    pub output: String,
    /// Compiler or VM diagnostics; empty on success.
    pub error: String,
}

/// Compile `source` and run its `main` through the CoreVM, capturing all
/// printed output. This is the same pipeline as
/// `nulang run --backend core-vm` (lexer → parser → typechecker → HIR → MIR
/// → bytecode → CoreVM).
pub fn compile_and_run(source: &str) -> RunResult {
    match compile_and_run_inner(source) {
        Ok(output) => RunResult {
            ok: true,
            output,
            error: String::new(),
        },
        Err(error) => RunResult {
            ok: false,
            output: String::new(),
            error,
        },
    }
}

fn compile_and_run_inner(source: &str) -> Result<String, String> {
    // Lexer / parser / typechecker errors are `NuError`; render with Display.
    let mut lexer = lexer::Lexer::new(source);
    let tokens = lexer.lex().map_err(|e| e.to_string())?;
    let mut parser = parser::Parser::new(tokens);
    let ast = parser.parse_module().map_err(|e| e.to_string())?;
    let mut type_checker = typechecker::TypeChecker::new();
    type_checker
        .check_module(&ast)
        .map_err(|e| e.to_string())?;

    let hir = hir_lower::lower_module(&ast, &type_checker.inferred_decl_types);
    let mut mir = mir_lower::lower_module(&hir).map_err(|e| e.to_string())?;
    let module = mir_codegen::compile_mir(&mut mir, "main").map_err(|e| e.to_string())?;

    let mut vm = core_vm::CoreVM::new();
    let sink: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    vm.output_sink = Some(sink.clone());
    let module_idx = vm
        .load_module_from_code(&module)
        .map_err(|e| format!("VM load: {e}"))?;
    let entry = module.entry_point.unwrap_or(0);
    let value = vm.run(module_idx, entry).map_err(|e| format!("VM: {e}"))?;

    let mut output = sink.borrow().clone();
    // Mirror the CLI: display a non-unit return value of `main`.
    let result_str = if let Some(s) = vm.resolve_display_string(value) {
        s
    } else {
        // Reuse the same tag decoding as the CLI's core-vm path.
        display_raw_value(value)
    };
    if !result_str.is_empty() && result_str != "unit" && result_str != "()" {
        output.push_str(&result_str);
        output.push('\n');
    }
    Ok(output)
}

/// Minimal raw-value display for the top-level result (the full `Value`
/// type lives in the native-only `src/vm.rs`).
fn display_raw_value(value: u64) -> String {
    if value_layout::is_int_raw(value) {
        value_layout::as_int_raw(value).to_string()
    } else if value == value_layout::TAG_NIL {
        "nil".to_string()
    } else if (value & value_layout::TAG_MASK) == value_layout::TAG_BOOL {
        if (value & 1) != 0 { "true" } else { "false" }.to_string()
    } else if value == value_layout::TAG_UNIT {
        "unit".to_string()
    } else {
        String::new()
    }
}

// -- C-style WASM ABI --------------------------------------------------------
//
// The JS side copies the source text into wasm memory with `nulang_alloc`,
// calls `nulang_run`, and reads back a length-prefixed UTF-8 JSON buffer:
//
//   [0..4]   u32 little-endian byte length of the JSON payload
//   [4..]    JSON payload (RunResult)
//
// No wasm-bindgen, no externref, no JS glue generator required.

/// Allocate `len` bytes of wasm memory; returns the pointer. The caller
/// (JS) writes source text here and passes it to `nulang_run`.
#[no_mangle]
pub extern "C" fn nulang_alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Free a buffer previously returned by `nulang_alloc`.
#[no_mangle]
pub unsafe extern "C" fn nulang_free(ptr: *mut u8, len: usize) {
    drop(Vec::from_raw_parts(ptr, 0, len));
}

/// Reusable result buffer backing `nulang_run`'s return value.
static mut RESULT_BUF: Vec<u8> = Vec::new();

/// Compile and run the Nulang source at `ptr..ptr+len`.
/// Returns a pointer to a length-prefixed JSON buffer (see above). The
/// buffer is intentionally leaked; it is reused across calls, so the JS
/// side must copy the payload out before the next `nulang_run`.
#[no_mangle]
pub unsafe extern "C" fn nulang_run(ptr: *const u8, len: usize) -> *const u8 {
    let source = match std::str::from_utf8(std::slice::from_raw_parts(ptr, len)) {
        Ok(s) => s.to_owned(),
        Err(e) => return result_ptr(format!(r#"{{"ok":false,"output":"","error":"invalid UTF-8 source: {e}"}}"#)),
    };
    let json = serde_json::to_string(&compile_and_run(&source))
        .unwrap_or_else(|_| r#"{"ok":false,"output":"","error":"serialization failed"}"#.into());
    result_ptr(json)
}

#[allow(static_mut_refs)]
fn result_ptr(json: String) -> *const u8 {
    unsafe {
        let buf = &mut *std::ptr::addr_of_mut!(RESULT_BUF);
        buf.clear();
        buf.extend_from_slice(&(json.len() as u32).to_le_bytes());
        buf.extend_from_slice(json.as_bytes());
        buf.as_ptr()
    }
}
