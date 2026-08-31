//! Single source of truth for JIT runtime helper declarations.
//!
//! The `define_helpers!` macro generates:
//! - The `RuntimeHelper` enum (used by `compiler.rs`)
//! - String-name accessors (used by `typed_compiler.rs`)
//! - `ALL` constant slice for iteration
//! - `register_with_builder` for `JitSession` and AOT
//! - `sig()` classification for CLIF import declaration
//!
//! Adding a helper: add one line to the `define_helpers!` invocation.

// Imports used by the `define_helpers!` macro expansion (compiler can't
// see use inside macro bodies).
#[allow(unused_imports)]
use cranelift::codegen::ir::FuncRef;
#[allow(unused_imports)]
use cranelift::prelude::{types, AbiParam, Signature};
use cranelift_frontend::FunctionBuilder;
use cranelift_jit::JITBuilder;
use cranelift_module::{Linkage, Module};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Signature classification
// ---------------------------------------------------------------------------

/// Signature shape of a runtime helper, used to declare CLIF imports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperSig {
    /// `(i64, i64) -> i64`
    Bin,
    /// `(i64) -> i64`
    Unary,
    /// `(*mut u64, u32, u32) -> void` (e.g. `ArrLen`)
    Reg3,
    /// `(*mut u64, u32, u32, u32) -> void` (e.g. `ArrStore`, `FieldL`)
    Reg4,
    /// `(*mut u64, i64, i64, i64) -> i64` — `nulang_jit_direct_call`
    /// `(regs_ptr, func_idx, argc, dst)` returns a nonzero status when the
    /// non-suspending callee raised a runtime error.
    DirectCall,
}

// ---------------------------------------------------------------------------
// Macro
// ---------------------------------------------------------------------------

macro_rules! define_helpers {
    ($($variant:ident => $c_name:ident, $sig:ident,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum RuntimeHelper {
            $($variant,)*
        }

        impl RuntimeHelper {
            /// C symbol name of the helper (e.g. `"nulang_iadd"`).
            pub fn name(&self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($c_name),)*
                }
            }

            /// Signature classification for CLIF import declaration.
            pub fn sig(&self) -> HelperSig {
                match self {
                    $(Self::$variant => HelperSig::$sig,)*
                }
            }

            /// Raw function pointer (for `JITBuilder::symbol`).
            pub fn fn_ptr(&self) -> *const u8 {
                match self {
                    $(Self::$variant => super::runtime::$c_name as *const u8,)*
                }
            }

            /// All helpers in declaration order.
            pub const ALL: &'static [(RuntimeHelper, &'static str)] = &[
                $((RuntimeHelper::$variant, stringify!($c_name)),)*
            ];
        }

        /// Register function pointers with a `JITBuilder` (used by `jit/mod.rs` and `aot/mod.rs`).
        pub fn register_with_builder(builder: &mut JITBuilder) {
            for (_helper, name) in RuntimeHelper::ALL {
                // Resolve function pointer via the enum match (one place, not 4).
                let ptr = match _helper {
                    $(RuntimeHelper::$variant => super::runtime::$c_name as *const u8,)*
                };
                builder.symbol(*name, ptr);
            }
        }

        /// Register CLIF imports with a `Module` + `FunctionBuilder` (used by `compiler.rs`).
        pub fn register_with_module<M: Module>(
            module: &mut M,
            builder: &mut FunctionBuilder,
        ) -> Result<HashMap<RuntimeHelper, FuncRef>, super::compiler::CompileError> {
            use super::compiler::{make_bin_sig, make_direct_call_sig, make_unary_sig, make_void_reg3_sig, make_void_reg4_sig};
            let mut helpers = HashMap::new();

            $(
                let sig = match HelperSig::$sig {
                    HelperSig::Bin => make_bin_sig(module),
                    HelperSig::Unary => make_unary_sig(module),
                    HelperSig::Reg3 => make_void_reg3_sig(module),
                    HelperSig::Reg4 => make_void_reg4_sig(module),
                    HelperSig::DirectCall => make_direct_call_sig(module),
                };
                let name = stringify!($c_name);
                let func_id = module
                    .declare_function(name, Linkage::Import, &sig)
                    .map_err(|e| super::compiler::CompileError::Internal(
                        format!("declare {}: {}", name, e)))?;
                let func_ref = module.declare_func_in_func(func_id, builder.func);
                helpers.insert(RuntimeHelper::$variant, func_ref);
            )*

            Ok(helpers)
        }
    };
}

// ---------------------------------------------------------------------------
// Helper definitions — single source of truth
// ---------------------------------------------------------------------------

define_helpers! {
    // Integer arithmetic
    IAdd   => nulang_iadd,   Bin,
    ISub   => nulang_isub,   Bin,
    IMul   => nulang_imul,   Bin,
    IDiv   => nulang_idiv,   Bin,
    IMod   => nulang_imod,   Bin,
    // Integer comparison
    ICmpEq => nulang_icmp_eq, Bin,
    ICmpLt => nulang_icmp_lt, Bin,
    ICmpGt => nulang_icmp_gt, Bin,
    ICmpLe => nulang_icmp_le, Bin,
    ICmpGe => nulang_icmp_ge, Bin,
    // Float arithmetic
    FAdd   => nulang_fadd,   Bin,
    FSub   => nulang_fsub,   Bin,
    FMul   => nulang_fmul,   Bin,
    FDiv   => nulang_fdiv,   Bin,
    // Float comparison
    FCmpEq => nulang_fcmp_eq, Bin,
    FCmpLt => nulang_fcmp_lt, Bin,
    FCmpGt => nulang_fcmp_gt, Bin,
    // Unary ops
    INeg   => nulang_ineg,   Unary,
    IInc   => nulang_iinc,   Unary,
    IDec   => nulang_idec,   Unary,
    Not    => nulang_not,    Unary,
    IToF   => nulang_itof,   Unary,
    FToI   => nulang_ftoi,   Unary,
    FNeg   => nulang_fneg,   Unary,
    // Logical
    And    => nulang_and,    Bin,
    Or     => nulang_or,     Bin,
    // Bitwise
    Xor    => nulang_xor,    Bin,
    Shl    => nulang_shl,    Bin,
    Shr    => nulang_shr,    Bin,
    BitAnd => nulang_bitand, Bin,
    BitOr  => nulang_bitor,  Bin,
    // Heap operations
    ArrStore => nulang_arr_store, Reg4,
    ArrLen   => nulang_arr_len,   Reg3,
    FieldL   => nulang_field_load, Reg4,
    // AOT value-based helpers (new)
    Pow      => nulang_pow,         Bin,
    StrEq    => nulang_str_eq,      Bin,
    StrConcat => nulang_str_concat,  Bin,
    AllocObj => nulang_alloc_obj,   Bin,  // actually (i64,i32)->i64 but builder only needs ptr
    ObjGet   => nulang_obj_get,     Bin,
    ObjSet   => nulang_obj_set,     Reg4, // actually (i64,i64,i64)->void
    ObjLen   => nulang_obj_len,     Unary,
    RecCopy  => nulang_rec_copy,    Unary,
    SafePoint => nulang_jit_safepoint_check, Unary,
    SetYield => nulang_jit_set_yield_pc, Unary,
    SetBranchExit => nulang_jit_set_branch_exit_pc, Unary,
    // Re-entrant direct call of a provably non-suspending callee from a
    // compiled region (see `nulang_jit_direct_call`).
    DirectCall => nulang_jit_direct_call, DirectCall,
}
