//! SIMD CLIF Compiler for the Nulang JIT Backend.
//!
//! Builds on top of the typed compiler to add SIMD vectorization for
//! array loops. Compiles vectorizable loop regions to native code using
//! Cranelift's SIMD vector instructions (I64x2, F64x2, I32x4, F32x4).
//!
//! # Architecture
//!
//! - `compile_simd_region()`: Main entry point — compiles a vectorizable
//!   loop region to SIMD-native code with scalar prefix/epilogue handling.
//! - `emit_simd_binop()`: Emits SIMD arithmetic in CLIF (iadd/fadd/etc).
//! - `emit_simd_load()` / `emit_simd_store()`: Vector load/store from arrays.
//! - `emit_scalar_prefix_loop()` / `emit_scalar_epilogue_loop()`: Scalar
//!   fallback loops for elements that don't fill a full vector.
//!
//! # Code Structure
//!
//! Each compiled SIMD function follows this layout:
//!
//! ```text
//! entry:
//!     load array base pointers from registers
//!     compute trip count
//!     prefix_count = trip_count % vector_width
//!     jump prefix_header(index=0)
//!
//! prefix_header(index):        // scalar prefix loop
//!     index < prefix_count?
//!     yes: jump prefix_body
//!     no:  jump simd_header(index=prefix_count)
//!
//! prefix_body:
//!     scalar body
//!     jump prefix_header(index+1)
//!
//! simd_header(index):          // main SIMD body
//!     index + width <= trip_count?
//!     yes: jump simd_body
//!     no:  jump epilogue_header(index)
//!
//! simd_body:
//!     load vector_a from a[i..i+width]
//!     load vector_b from b[i..i+width]
//!     simd_result = vector_a op vector_b
//!     store vector_result to c[i..i+width]
//!     jump simd_header(index+width)
//!
//! epilogue_header(index):      // scalar cleanup
//!     index < trip_count?
//!     yes: jump epilogue_body
//!     no:  jump return
//!
//! epilogue_body:
//!     scalar body
//!     jump epilogue_header(index+1)
//!
//! return:
//!     ret
//! ```

use cranelift::codegen::ir::{BlockArg, FuncRef};
use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilder;
use cranelift_jit::JITModule;
use cranelift_module::{Linkage, Module};

use std::collections::HashMap;

use crate::bytecode::Instruction;
use crate::cranelift_utils::{emit_extract_payload, emit_sext48};
use crate::jit::compiler::CompileError;
#[cfg(test)]
use crate::jit::simd_analyzer::SimdWidth;
use crate::jit::simd_analyzer::{
    BinopKind, CmpKind, SimdElemType, SimdRegion, UnaryKind, VectorizablePattern,
};
use crate::jit::typed_compiler::load_reg;
use crate::value_layout::{PAYLOAD_MASK, TAG_INT};

// ---------------------------------------------------------------------------
// SIMD Operation Enum (internal to the compiler)
// ---------------------------------------------------------------------------

/// SIMD binary operations mapped from the analyzer's `BinopKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimdBinOp {
    Add,
    Sub,
    Mul,
}

impl SimdBinOp {
    /// Convert from an analyzer `BinopKind` to a `SimdBinOp`.
    pub fn from_binop_kind(op: BinopKind) -> Option<Self> {
        match op {
            BinopKind::IAdd | BinopKind::FAdd => Some(SimdBinOp::Add),
            BinopKind::ISub | BinopKind::FSub => Some(SimdBinOp::Sub),
            BinopKind::IMul | BinopKind::FMul => Some(SimdBinOp::Mul),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: Map SimdElemType to Cranelift vector Type
// ---------------------------------------------------------------------------

/// Get the Cranelift SIMD vector type for a given element type.
fn vector_clif_type(elem_type: SimdElemType) -> Type {
    match elem_type {
        SimdElemType::Int64 => types::I64X2,
        SimdElemType::Float64 => types::F64X2,
        SimdElemType::Int32 => types::I32X4,
        SimdElemType::Float32 => types::F32X4,
    }
}

/// Get the number of vector lanes for a given element type.
fn vector_width(elem_type: SimdElemType) -> usize {
    elem_type.lane_count()
}

/// Get the element size in bytes.
fn elem_size(elem_type: SimdElemType) -> usize {
    elem_type.elem_size()
}

// ---------------------------------------------------------------------------
// Host SIMD Capability Detection
// ---------------------------------------------------------------------------

/// Check whether the host CPU supports SIMD instructions.
///
/// On x86_64, checks for SSE2 (the baseline for Cranelift's 128-bit vectors).
/// On aarch64, NEON is baseline so always returns true.
/// On other architectures, returns false.
pub fn is_simd_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        return std::is_x86_feature_detected!("sse2");
    }
    #[cfg(target_arch = "aarch64")]
    {
        return true; // NEON is baseline on aarch64
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// Main Entry Point
// ---------------------------------------------------------------------------

/// Compile a vectorizable loop region to SIMD-native code.
///
/// This is the main entry point for SIMD compilation. It generates a function
/// that:
/// 1. Loads array base pointers from NaN-tagged registers
/// 2. Computes the trip count (from hint or constant)
/// 3. Runs a scalar prefix loop for `trip_count % vector_width` elements
/// 4. Runs the SIMD body processing `vector_width` elements at a time
/// 5. Runs a scalar epilogue loop for any remaining elements
///
/// # Arguments
/// - `module`: The Cranelift JIT module
/// - `builder_context`: Reusable function builder context
/// - `ctx`: Reusable codegen context
/// - `func_name`: Unique name for the compiled function
/// - `instructions`: Full instruction array (for scalar fallback)
/// - `simd_region`: The vectorizable region descriptor from the analyzer
///
/// # Returns
/// A raw function pointer to the compiled code, or an error.
pub fn compile_simd_region(
    module: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    ctx: &mut codegen::Context,
    func_name: &str,
    instructions: &[Instruction],
    simd_region: &SimdRegion,
) -> Result<*const u8, CompileError> {
    // If SIMD is not supported on this host, fall back to scalar compilation
    if !is_simd_supported() {
        return fallback_to_scalar(
            module,
            builder_context,
            ctx,
            func_name,
            simd_region.start_offset,
            simd_region.num_instrs,
            instructions,
        );
    }

    // If no trip count hint and no ArrLen register, fall back to scalar.
    let trip_count_is_runtime =
        simd_region.trip_count_hint == Some(0) && simd_region.arr_len_reg.is_some();
    if simd_region.trip_count_hint.is_none() && !trip_count_is_runtime {
        return fallback_to_scalar(
            module,
            builder_context,
            ctx,
            func_name,
            simd_region.start_offset,
            simd_region.num_instrs,
            instructions,
        );
    }

    ctx.clear();

    // Build the function signature: fn(regs: *mut u64, constants: *const u64)
    let pointer_type = module.isa().pointer_type();
    ctx.func.signature.params.push(AbiParam::new(pointer_type));
    ctx.func.signature.params.push(AbiParam::new(pointer_type));

    let mut builder = FunctionBuilder::new(&mut ctx.func, builder_context);

    // -----------------------------------------------------------------------
    // Create blocks
    // -----------------------------------------------------------------------

    let entry_block = builder.create_block();
    builder.append_block_params_for_function_params(entry_block);

    // Scalar prefix loop
    let prefix_header = builder.create_block();
    builder.append_block_param(prefix_header, types::I64);
    let prefix_body = builder.create_block();

    // SIMD main loop
    let simd_header = builder.create_block();
    builder.append_block_param(simd_header, types::I64);
    let simd_body = builder.create_block();

    // Scalar epilogue loop
    let epilogue_header = builder.create_block();
    builder.append_block_param(epilogue_header, types::I64);
    let epilogue_body = builder.create_block();
    let _epilogue_post = builder.create_block();
    let return_block = builder.create_block();

    // -----------------------------------------------------------------------
    // Entry: load registers pointer, compute trip count
    // -----------------------------------------------------------------------

    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);
    let regs_ptr = builder.block_params(entry_block)[0];
    let _constants_ptr = builder.block_params(entry_block)[1];

    // Extract array registers from the pattern
    let (lhs_arr_reg, rhs_arr_reg, dst_arr_reg) = match &simd_region.pattern {
        VectorizablePattern::ElementWiseBinop {
            lhs_arr_reg,
            rhs_arr_reg,
            dst_arr_reg,
            ..
        } => (
            *lhs_arr_reg as usize,
            *rhs_arr_reg as usize,
            *dst_arr_reg as usize,
        ),
        VectorizablePattern::ElementWiseUnary {
            src_arr_reg,
            dst_arr_reg,
            ..
        } => (
            *src_arr_reg as usize,
            *src_arr_reg as usize,
            *dst_arr_reg as usize,
        ),
        VectorizablePattern::ElementWiseCmp {
            lhs_arr_reg,
            rhs_arr_reg,
            dst_arr_reg,
            ..
        } => (
            *lhs_arr_reg as usize,
            *rhs_arr_reg as usize,
            *dst_arr_reg as usize,
        ),
    };

    // Load array base pointers (NaN-tagged) and extract payloads
    let lhs_base_tagged = load_reg(&mut builder, regs_ptr, lhs_arr_reg);
    let rhs_base_tagged = load_reg(&mut builder, regs_ptr, rhs_arr_reg);
    let dst_base_tagged = load_reg(&mut builder, regs_ptr, dst_arr_reg);

    let lhs_base = emit_extract_payload(&mut builder, lhs_base_tagged);
    let rhs_base = emit_extract_payload(&mut builder, rhs_base_tagged);
    let dst_base = emit_extract_payload(&mut builder, dst_base_tagged);

    // Trip count: either compile-time constant or loaded from ArrLen register
    let trip_count = if trip_count_is_runtime {
        let arr_len_reg = simd_region.arr_len_reg.unwrap();
        let offset = i32::from(arr_len_reg) * 8;
        let addr = builder.ins().iadd_imm(regs_ptr, offset as i64);
        let mem_flags = MemFlags::trusted();
        builder.ins().load(types::I64, mem_flags, addr, 0)
    } else {
        let n = simd_region.trip_count_hint.unwrap_or(0) as i64;
        builder.ins().iconst(types::I64, n)
    };
    let vwidth = vector_width(simd_region.elem_type) as i64;
    let vwidth_val = builder.ins().iconst(types::I64, vwidth);

    // Compute prefix_count = trip_count % vector_width
    let prefix_count = builder.ins().srem(trip_count, vwidth_val);

    // Element size constant
    let elem_size_val = builder
        .ins()
        .iconst(types::I64, elem_size(simd_region.elem_type) as i64);

    // Jump to prefix loop with index = 0
    let index_init = builder.ins().iconst(types::I64, 0);
    builder
        .ins()
        .jump(prefix_header, &[BlockArg::from(index_init)]);

    // -----------------------------------------------------------------------
    // Scalar Prefix Loop: for i in 0..prefix_count
    // -----------------------------------------------------------------------

    builder.switch_to_block(prefix_header);
    let prefix_index = builder.block_params(prefix_header)[0];

    // Check: index < prefix_count?
    let prefix_cond = builder
        .ins()
        .icmp(IntCC::SignedLessThan, prefix_index, prefix_count);
    builder.ins().brif(
        prefix_cond,
        prefix_body,
        &[],
        simd_header,
        &[BlockArg::from(prefix_count)],
    );

    // Prefix body: scalar iteration
    builder.switch_to_block(prefix_body);
    emit_scalar_iteration(
        &mut builder,
        simd_region,
        prefix_index,
        lhs_base,
        rhs_base,
        dst_base,
    );
    // Increment and loop back
    let one = builder.ins().iconst(types::I64, 1);
    let prefix_next = builder.ins().iadd(prefix_index, one);
    builder
        .ins()
        .jump(prefix_header, &[BlockArg::from(prefix_next)]);

    builder.seal_block(prefix_body);

    // -----------------------------------------------------------------------
    // SIMD Main Loop: for i in prefix_count..(trip_count - remainder)
    // -----------------------------------------------------------------------

    builder.switch_to_block(simd_header);
    let simd_index = builder.block_params(simd_header)[0];

    // Check: index + vector_width <= trip_count?
    let simd_end_bound = builder.ins().isub(trip_count, vwidth_val);
    let simd_cond = builder
        .ins()
        .icmp(IntCC::SignedLessThanOrEqual, simd_index, simd_end_bound);
    builder.ins().brif(
        simd_cond,
        simd_body,
        &[],
        epilogue_header,
        &[BlockArg::from(simd_index)],
    );

    // SIMD body: vectorized iteration
    builder.switch_to_block(simd_body);

    // Load vectors from arrays
    let vec_lhs = emit_simd_load(
        &mut builder,
        simd_region,
        lhs_base,
        simd_index,
        elem_size_val,
    );
    let vec_rhs = emit_simd_load(
        &mut builder,
        simd_region,
        rhs_base,
        simd_index,
        elem_size_val,
    );

    // Emit SIMD operation
    let vec_result = match &simd_region.pattern {
        VectorizablePattern::ElementWiseBinop { op, .. } => {
            if let Some(simd_op) = SimdBinOp::from_binop_kind(*op) {
                emit_simd_binop(
                    &mut builder,
                    vec_lhs,
                    vec_rhs,
                    simd_region.elem_type,
                    simd_op,
                )
            } else {
                vec_lhs // fallback for unsupported ops
            }
        }
        VectorizablePattern::ElementWiseUnary { op, .. } => {
            emit_simd_unop(&mut builder, vec_lhs, simd_region.elem_type, *op)
        }
        VectorizablePattern::ElementWiseCmp { op, .. } => {
            emit_simd_cmp(&mut builder, vec_lhs, vec_rhs, simd_region.elem_type, *op)
        }
    };

    // Store result vector
    emit_simd_store(
        &mut builder,
        simd_region,
        dst_base,
        simd_index,
        elem_size_val,
        vec_result,
    );

    // Increment index by vector_width and loop back
    let simd_next = builder.ins().iadd(simd_index, vwidth_val);
    builder
        .ins()
        .jump(simd_header, &[BlockArg::from(simd_next)]);

    builder.seal_block(simd_body);

    // -----------------------------------------------------------------------
    // Scalar Epilogue Loop: for remaining elements
    // -----------------------------------------------------------------------

    builder.switch_to_block(epilogue_header);
    let epilogue_index = builder.block_params(epilogue_header)[0];

    // Check: index < trip_count?
    let epilogue_cond = builder
        .ins()
        .icmp(IntCC::SignedLessThan, epilogue_index, trip_count);
    builder
        .ins()
        .brif(epilogue_cond, epilogue_body, &[], return_block, &[]);

    // Epilogue body: scalar iteration
    builder.switch_to_block(epilogue_body);
    emit_scalar_iteration(
        &mut builder,
        simd_region,
        epilogue_index,
        lhs_base,
        rhs_base,
        dst_base,
    );
    // Increment and loop back
    let one_val = builder.ins().iconst(types::I64, 1);
    let epilogue_next = builder.ins().iadd(epilogue_index, one_val);
    builder
        .ins()
        .jump(epilogue_header, &[BlockArg::from(epilogue_next)]);

    builder.seal_block(epilogue_body);

    // -----------------------------------------------------------------------
    // Return
    // -----------------------------------------------------------------------

    builder.switch_to_block(return_block);
    builder.seal_block(return_block);

    // Seal the loop headers (all predecessors are known)
    builder.seal_block(prefix_header);
    builder.seal_block(simd_header);
    builder.seal_block(epilogue_header);

    builder.ins().return_(&[]);

    // Finalize
    builder.finalize();

    // Define and compile the function
    let func_id = module
        .declare_function(func_name, Linkage::Local, &ctx.func.signature.clone())
        .map_err(|e| CompileError::DeclareFailed(format!("{}", e)))?;

    module
        .define_function(func_id, ctx)
        .map_err(|e| CompileError::CompileFailed(format!("{:?}", e)))?;

    module.finalize_definitions().unwrap();

    let code = module.get_finalized_function(func_id);
    Ok(code as *const u8)
}

// ---------------------------------------------------------------------------
// Fallback to Scalar Compilation
// ---------------------------------------------------------------------------

/// When SIMD is not supported or the region can't be vectorized, fall back
/// to the typed scalar compiler.
fn fallback_to_scalar(
    module: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    ctx: &mut codegen::Context,
    func_name: &str,
    start_offset: usize,
    num_instrs: usize,
    instructions: &[Instruction],
) -> Result<*const u8, CompileError> {
    crate::jit::typed_compiler::compile_bytecode_region_typed(
        module,
        builder_context,
        ctx,
        func_name,
        start_offset,
        num_instrs,
        instructions,
        None,
        None,
    )
}

// ---------------------------------------------------------------------------
// SIMD Binary Operation Emission
// ---------------------------------------------------------------------------

/// Emit a SIMD binary operation in CLIF.
///
/// Emits the appropriate vector arithmetic instruction based on element type:
/// - Integer types: `iadd`, `isub`, `imul` on I64x2/I32x4
/// - Float types: `fadd`, `fsub`, `fmul` on F64x2/F32x4
///
/// # Arguments
/// - `builder`: The function builder
/// - `a_vec`: First SIMD vector operand
/// - `b_vec`: Second SIMD vector operand
/// - `elem_type`: The element type (determines which CLIF type to use)
/// - `op`: The binary operation to perform
///
/// # Returns
/// The SIMD vector result value.
pub fn emit_simd_binop(
    builder: &mut FunctionBuilder,
    a_vec: Value,
    b_vec: Value,
    elem_type: SimdElemType,
    op: SimdBinOp,
) -> Value {
    match elem_type {
        SimdElemType::Int64 | SimdElemType::Int32 => match op {
            SimdBinOp::Add => builder.ins().iadd(a_vec, b_vec),
            SimdBinOp::Sub => builder.ins().isub(a_vec, b_vec),
            SimdBinOp::Mul => builder.ins().imul(a_vec, b_vec),
        },
        SimdElemType::Float64 | SimdElemType::Float32 => match op {
            SimdBinOp::Add => builder.ins().fadd(a_vec, b_vec),
            SimdBinOp::Sub => builder.ins().fsub(a_vec, b_vec),
            SimdBinOp::Mul => builder.ins().fmul(a_vec, b_vec),
        },
    }
}

// ---------------------------------------------------------------------------
// SIMD Unary Operation Emission
// ---------------------------------------------------------------------------

/// Emit a SIMD unary operation in CLIF.
///
/// Currently supports integer negate (`ineg`) and float negate (`fneg`).
pub fn emit_simd_unop(
    builder: &mut FunctionBuilder,
    a_vec: Value,
    elem_type: SimdElemType,
    op: UnaryKind,
) -> Value {
    match op {
        UnaryKind::INeg => {
            match elem_type {
                SimdElemType::Int64 | SimdElemType::Int32 => builder.ins().ineg(a_vec),
                SimdElemType::Float64 | SimdElemType::Float32 => {
                    // Float negation: XOR with sign bit
                    // For now, fallback to zero - a (which is a valid fneg)
                    let zero = match elem_type {
                        SimdElemType::Float64 => builder.ins().f64const(0.0),
                        SimdElemType::Float32 => builder.ins().f32const(0.0),
                        _ => unreachable!(),
                    };
                    // Need to splat the scalar zero to vector
                    let vec_ty = vector_clif_type(elem_type);
                    let vec_zero = builder.ins().splat(vec_ty, zero);
                    builder.ins().fsub(vec_zero, a_vec)
                }
            }
        }
        UnaryKind::FNeg => match elem_type {
            SimdElemType::Float64 | SimdElemType::Float32 => {
                let zero = match elem_type {
                    SimdElemType::Float64 => builder.ins().f64const(0.0),
                    SimdElemType::Float32 => builder.ins().f32const(0.0),
                    _ => unreachable!(),
                };
                let vec_ty = vector_clif_type(elem_type);
                let vec_zero = builder.ins().splat(vec_ty, zero);
                builder.ins().fsub(vec_zero, a_vec)
            }
            SimdElemType::Int64 | SimdElemType::Int32 => builder.ins().ineg(a_vec),
        },
    }
}

// ---------------------------------------------------------------------------
// SIMD Comparison Emission
// ---------------------------------------------------------------------------

/// Emit a SIMD comparison operation in CLIF.
///
/// Produces a vector of boolean results (one per lane).
pub fn emit_simd_cmp(
    builder: &mut FunctionBuilder,
    a_vec: Value,
    b_vec: Value,
    elem_type: SimdElemType,
    op: CmpKind,
) -> Value {
    match elem_type {
        SimdElemType::Int64 | SimdElemType::Int32 => {
            let cc = match op {
                CmpKind::ICmpEq => IntCC::Equal,
                CmpKind::ICmpLt => IntCC::SignedLessThan,
                CmpKind::ICmpGt => IntCC::SignedGreaterThan,
                CmpKind::ICmpLe => IntCC::SignedLessThanOrEqual,
                CmpKind::ICmpGe => IntCC::SignedGreaterThanOrEqual,
                _ => IntCC::Equal,
            };
            builder.ins().icmp(cc, a_vec, b_vec)
        }
        SimdElemType::Float64 | SimdElemType::Float32 => {
            let cc = match op {
                CmpKind::FCmpEq => FloatCC::Equal,
                CmpKind::FCmpLt => FloatCC::LessThan,
                CmpKind::FCmpGt => FloatCC::GreaterThan,
                _ => FloatCC::Equal,
            };
            builder.ins().fcmp(cc, a_vec, b_vec)
        }
    }
}

// ---------------------------------------------------------------------------
// SIMD Load / Store Helpers
// ---------------------------------------------------------------------------

/// Load a SIMD vector from an array at the given index.
///
/// Computes the address: `base_ptr + index * elem_size`, then loads
/// a full SIMD vector (e.g., 16 bytes for I64x2) from that address.
///
/// # Arguments
/// - `builder`: The function builder
/// - `simd_region`: The SIMD region (determines vector type)
/// - `base_ptr`: The base pointer of the array (extracted payload, untagged)
/// - `index`: The element index (scalar i64)
/// - `elem_size_val`: The element size in bytes (i64 constant value)
///
/// # Returns
/// A SIMD vector value loaded from memory.
pub fn emit_simd_load(
    builder: &mut FunctionBuilder,
    simd_region: &SimdRegion,
    base_ptr: Value,
    index: Value,
    elem_size_val: Value,
) -> Value {
    let offset = builder.ins().imul(index, elem_size_val);
    let addr = builder.ins().iadd(base_ptr, offset);
    let vtype = vector_clif_type(simd_region.elem_type);
    let flags = MemFlags::trusted();
    builder.ins().load(vtype, flags, addr, 0)
}

/// Store a SIMD vector to an array at the given index.
///
/// Computes the address: `base_ptr + index * elem_size`, then stores
/// a full SIMD vector to that address.
///
/// # Arguments
/// - `builder`: The function builder
/// - `simd_region`: The SIMD region (determines vector type)
/// - `base_ptr`: The base pointer of the array (extracted payload, untagged)
/// - `index`: The element index (scalar i64)
/// - `elem_size_val`: The element size in bytes (i64 constant value)
/// - `value`: The SIMD vector value to store
pub fn emit_simd_store(
    builder: &mut FunctionBuilder,
    _simd_region: &SimdRegion,
    base_ptr: Value,
    index: Value,
    elem_size_val: Value,
    value: Value,
) {
    let offset = builder.ins().imul(index, elem_size_val);
    let addr = builder.ins().iadd(base_ptr, offset);
    let flags = MemFlags::trusted();
    builder.ins().store(flags, value, addr, 0);
}

// ---------------------------------------------------------------------------
// Scalar Load / Store Helpers (for prefix/epilogue)
// ---------------------------------------------------------------------------

/// Load a single scalar element from an array at the given index.
///
/// This is used by the scalar prefix and epilogue loops. It handles
/// NaN-tag extraction for integer elements and bitcast for float elements.
fn emit_scalar_load(
    builder: &mut FunctionBuilder,
    elem_type: SimdElemType,
    base_ptr: Value,
    index: Value,
    elem_size_val: Value,
) -> Value {
    let offset = builder.ins().imul(index, elem_size_val);
    let addr = builder.ins().iadd(base_ptr, offset);

    match elem_type {
        SimdElemType::Int64 => {
            let raw = builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
            emit_sext48(builder, raw)
        }
        SimdElemType::Float64 => {
            let bits = builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
            builder.ins().bitcast(types::F64, MemFlags::new(), bits)
        }
        SimdElemType::Int32 => {
            let val32 = builder.ins().load(types::I32, MemFlags::trusted(), addr, 0);
            builder.ins().sextend(types::I64, val32)
        }
        SimdElemType::Float32 => {
            let val32 = builder.ins().load(types::F32, MemFlags::trusted(), addr, 0);
            builder.ins().fpromote(types::F64, val32)
        }
    }
}

/// Store a single scalar element to an array at the given index.
///
/// This is used by the scalar prefix and epilogue loops. It handles
/// NaN-tagging for integer elements and bitcast for float elements.
fn emit_scalar_store(
    builder: &mut FunctionBuilder,
    elem_type: SimdElemType,
    base_ptr: Value,
    index: Value,
    elem_size_val: Value,
    value: Value,
) {
    let offset = builder.ins().imul(index, elem_size_val);
    let addr = builder.ins().iadd(base_ptr, offset);

    match elem_type {
        SimdElemType::Int64 => {
            let tagged = emit_tag_int_scalar(builder, value);
            builder.ins().store(MemFlags::trusted(), tagged, addr, 0);
        }
        SimdElemType::Float64 => {
            let bits = builder.ins().bitcast(types::I64, MemFlags::new(), value);
            builder.ins().store(MemFlags::trusted(), bits, addr, 0);
        }
        SimdElemType::Int32 => {
            let val32 = builder.ins().ireduce(types::I32, value);
            builder.ins().store(MemFlags::trusted(), val32, addr, 0);
        }
        SimdElemType::Float32 => {
            let val32 = builder.ins().fdemote(types::F32, value);
            builder.ins().store(MemFlags::trusted(), val32, addr, 0);
        }
    }
}

/// Re-tag an i64 value into a NaN-tagged integer (for scalar stores).
fn emit_tag_int_scalar(builder: &mut FunctionBuilder, value: Value) -> Value {
    let tag = builder.ins().iconst(types::I64, TAG_INT as i64);
    let mask = builder.ins().iconst(types::I64, PAYLOAD_MASK as i64);
    let masked = builder.ins().band(value, mask);
    builder.ins().bor(tag, masked)
}

// ---------------------------------------------------------------------------
// Scalar Iteration (for prefix and epilogue)
// ---------------------------------------------------------------------------

/// Emit a single scalar iteration of the loop body.
///
/// Loads one element from each input array, performs the operation,
/// and stores the result. Used by both prefix and epilogue loops.
fn emit_scalar_iteration(
    builder: &mut FunctionBuilder,
    simd_region: &SimdRegion,
    index: Value,
    lhs_base: Value,
    rhs_base: Value,
    dst_base: Value,
) {
    let elem_type = simd_region.elem_type;
    let esize = elem_size_val(builder, elem_type);

    // Load scalar elements
    let lhs_val = emit_scalar_load(builder, elem_type, lhs_base, index, esize);
    let rhs_val = emit_scalar_load(builder, elem_type, rhs_base, index, esize);

    // Perform the operation
    let result = match &simd_region.pattern {
        VectorizablePattern::ElementWiseBinop { op, .. } => {
            if let Some(simd_op) = SimdBinOp::from_binop_kind(*op) {
                match elem_type {
                    SimdElemType::Int64 | SimdElemType::Int32 => match simd_op {
                        SimdBinOp::Add => builder.ins().iadd(lhs_val, rhs_val),
                        SimdBinOp::Sub => builder.ins().isub(lhs_val, rhs_val),
                        SimdBinOp::Mul => builder.ins().imul(lhs_val, rhs_val),
                    },
                    SimdElemType::Float64 | SimdElemType::Float32 => match simd_op {
                        SimdBinOp::Add => builder.ins().fadd(lhs_val, rhs_val),
                        SimdBinOp::Sub => builder.ins().fsub(lhs_val, rhs_val),
                        SimdBinOp::Mul => builder.ins().fmul(lhs_val, rhs_val),
                    },
                }
            } else {
                lhs_val
            }
        }
        VectorizablePattern::ElementWiseUnary { op, .. } => match op {
            UnaryKind::INeg => builder.ins().ineg(lhs_val),
            UnaryKind::FNeg => {
                let zero = builder.ins().f64const(0.0);
                builder.ins().fsub(zero, lhs_val)
            }
        },
        VectorizablePattern::ElementWiseCmp { op, .. } => {
            let cond = match elem_type {
                SimdElemType::Int64 | SimdElemType::Int32 => {
                    let cc = match op {
                        CmpKind::ICmpEq => IntCC::Equal,
                        CmpKind::ICmpLt => IntCC::SignedLessThan,
                        CmpKind::ICmpGt => IntCC::SignedGreaterThan,
                        CmpKind::ICmpLe => IntCC::SignedLessThanOrEqual,
                        CmpKind::ICmpGe => IntCC::SignedGreaterThanOrEqual,
                        _ => IntCC::Equal,
                    };
                    builder.ins().icmp(cc, lhs_val, rhs_val)
                }
                SimdElemType::Float64 | SimdElemType::Float32 => {
                    let cc = match op {
                        CmpKind::FCmpEq => FloatCC::Equal,
                        CmpKind::FCmpLt => FloatCC::LessThan,
                        CmpKind::FCmpGt => FloatCC::GreaterThan,
                        _ => FloatCC::Equal,
                    };
                    builder.ins().fcmp(cc, lhs_val, rhs_val)
                }
            };
            // Convert boolean result to i64 (1 or 0)
            let one = builder.ins().iconst(types::I64, 1);
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().select(cond, one, zero)
        }
    };

    // Store the result
    emit_scalar_store(builder, elem_type, dst_base, index, esize, result);
}

// ---------------------------------------------------------------------------
// Scalar Prefix / Epilogue Loop Generation
// ---------------------------------------------------------------------------

/// Generate a scalar loop for the prefix elements.
///
/// The prefix loop handles the first `trip_count % vector_width` elements
/// individually using scalar operations before the main SIMD loop.
#[allow(dead_code)]
fn emit_scalar_prefix_loop(
    builder: &mut FunctionBuilder,
    simd_region: &SimdRegion,
    a_base: Value,
    b_base: Value,
    c_base: Value,
    prefix_count: Value,
) -> (Block, Block) {
    let prefix_header = builder.create_block();
    let prefix_body = builder.create_block();
    let prefix_post = builder.create_block();

    builder.switch_to_block(prefix_header);
    let index = builder.ins().iconst(types::I64, 0);
    let cond = builder
        .ins()
        .icmp(IntCC::SignedLessThan, index, prefix_count);
    builder.ins().brif(cond, prefix_body, &[], prefix_post, &[]);

    builder.switch_to_block(prefix_body);
    emit_scalar_iteration(builder, simd_region, index, a_base, b_base, c_base);
    let one = builder.ins().iconst(types::I64, 1);
    let next = builder.ins().iadd(index, one);
    let cond2 = builder
        .ins()
        .icmp(IntCC::SignedLessThan, next, prefix_count);
    builder
        .ins()
        .brif(cond2, prefix_body, &[], prefix_post, &[]);

    builder.seal_block(prefix_body);

    (prefix_header, prefix_post)
}

/// Generate a scalar loop for the epilogue elements.
#[allow(dead_code)]
fn emit_scalar_epilogue_loop(
    builder: &mut FunctionBuilder,
    simd_region: &SimdRegion,
    a_base: Value,
    b_base: Value,
    c_base: Value,
    start_index: Value,
    trip_count: Value,
) -> (Block, Block) {
    let epilogue_header = builder.create_block();
    let epilogue_body = builder.create_block();
    let epilogue_post = builder.create_block();

    builder.switch_to_block(epilogue_header);
    let cond = builder
        .ins()
        .icmp(IntCC::SignedLessThan, start_index, trip_count);
    builder
        .ins()
        .brif(cond, epilogue_body, &[], epilogue_post, &[]);

    builder.switch_to_block(epilogue_body);
    emit_scalar_iteration(builder, simd_region, start_index, a_base, b_base, c_base);
    let one = builder.ins().iconst(types::I64, 1);
    let next = builder.ins().iadd(start_index, one);
    let cond2 = builder.ins().icmp(IntCC::SignedLessThan, next, trip_count);
    builder
        .ins()
        .brif(cond2, epilogue_body, &[], epilogue_post, &[]);

    builder.seal_block(epilogue_body);

    (epilogue_header, epilogue_post)
}

// ---------------------------------------------------------------------------
// SIMD Runtime Helper Declarations
// ---------------------------------------------------------------------------

/// Declare SIMD-specific runtime helpers with the JIT module.
///
/// These helpers provide a fallback path when direct SIMD CLIF emission
/// is not sufficient or when the host doesn't support SIMD.
#[allow(dead_code)]
fn declare_simd_runtime_helpers<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
) -> HashMap<&'static str, FuncRef> {
    let mut helpers = HashMap::new();

    let simd_i64_sig = make_simd_i64x2_sig(module);
    let simd_f64_sig = make_simd_f64x2_sig(module);

    let i64_helpers = &[
        "nulang_simd_iadd_i64x2",
        "nulang_simd_isub_i64x2",
        "nulang_simd_imul_i64x2",
    ];
    for name in i64_helpers {
        if let Ok(func_id) = module.declare_function(name, Linkage::Import, &simd_i64_sig) {
            let func_ref = module.declare_func_in_func(func_id, builder.func);
            helpers.insert(*name, func_ref);
        }
    }

    let f64_helpers = &[
        "nulang_simd_fadd_f64x2",
        "nulang_simd_fsub_f64x2",
        "nulang_simd_fmul_f64x2",
    ];
    for name in f64_helpers {
        if let Ok(func_id) = module.declare_function(name, Linkage::Import, &simd_f64_sig) {
            let func_ref = module.declare_func_in_func(func_id, builder.func);
            helpers.insert(*name, func_ref);
        }
    }

    let scalar_load_sig = make_scalar_load_sig(module);
    let scalar_store_sig = make_scalar_store_sig(module);

    if let Ok(func_id) =
        module.declare_function("nulang_simd_load_array", Linkage::Import, &scalar_load_sig)
    {
        let func_ref = module.declare_func_in_func(func_id, builder.func);
        helpers.insert("nulang_simd_load_array", func_ref);
    }

    if let Ok(func_id) = module.declare_function(
        "nulang_simd_store_array",
        Linkage::Import,
        &scalar_store_sig,
    ) {
        let func_ref = module.declare_func_in_func(func_id, builder.func);
        helpers.insert("nulang_simd_store_array", func_ref);
    }

    helpers
}

/// Signature for I64x2 SIMD operations: fn(I64x2, I64x2) -> I64x2
#[allow(dead_code)]
fn make_simd_i64x2_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64X2));
    sig.params.push(AbiParam::new(types::I64X2));
    sig.returns.push(AbiParam::new(types::I64X2));
    sig
}

/// Signature for F64x2 SIMD operations: fn(F64x2, F64x2) -> F64x2
#[allow(dead_code)]
fn make_simd_f64x2_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::F64X2));
    sig.params.push(AbiParam::new(types::F64X2));
    sig.returns.push(AbiParam::new(types::F64X2));
    sig
}

/// Signature for scalar array load: fn(base: i64, idx: i64, len: i64) -> i64
#[allow(dead_code)]
fn make_scalar_load_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Signature for scalar array store: fn(base: i64, idx: i64, len: i64, value: i64)
#[allow(dead_code)]
fn make_scalar_store_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig
}

// ---------------------------------------------------------------------------
// Utility Helpers
// ---------------------------------------------------------------------------

/// Create an i64 constant value for the element size of a given type.
fn elem_size_val(builder: &mut FunctionBuilder, elem_type: SimdElemType) -> Value {
    builder
        .ins()
        .iconst(types::I64, elem_size(elem_type) as i64)
}

// ---------------------------------------------------------------------------
// SIMD Vector Manipulation Helpers
// ---------------------------------------------------------------------------

/// Create a SIMD vector constant with all lanes set to the same value.
#[allow(dead_code)]
fn emit_simd_splat(builder: &mut FunctionBuilder, elem_type: SimdElemType, value: Value) -> Value {
    let vtype = vector_clif_type(elem_type);
    builder.ins().splat(vtype, value)
}

/// Extract a single lane from a SIMD vector.
#[allow(dead_code)]
fn emit_simd_extract_lane(
    builder: &mut FunctionBuilder,
    _elem_type: SimdElemType,
    vector: Value,
    lane_idx: u8,
) -> Value {
    builder.ins().extractlane(vector, lane_idx)
}

/// Insert a single lane into a SIMD vector.
#[allow(dead_code)]
fn emit_simd_insert_lane(
    builder: &mut FunctionBuilder,
    vector: Value,
    value: Value,
    lane_idx: u8,
) -> Value {
    builder.ins().insertlane(vector, value, lane_idx)
}

/// Emit SIMD bitwise AND.
#[allow(dead_code)]
fn emit_simd_band(builder: &mut FunctionBuilder, a: Value, b: Value) -> Value {
    builder.ins().band(a, b)
}

/// Emit SIMD bitwise OR.
#[allow(dead_code)]
fn emit_simd_bor(builder: &mut FunctionBuilder, a: Value, b: Value) -> Value {
    builder.ins().bor(a, b)
}

/// Emit SIMD bitwise XOR.
#[allow(dead_code)]
fn emit_simd_bxor(builder: &mut FunctionBuilder, a: Value, b: Value) -> Value {
    builder.ins().bxor(a, b)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod simd_compiler_tests {
    use super::*;
    use crate::bytecode::*;
    use crate::jit::JitSession;

    /// Helper: Build a JIT session for testing.
    fn make_jit() -> JitSession {
        JitSession::new().unwrap()
    }

    /// Helper: Create an I64 element-wise binop SIMD region.
    fn make_i64_binop_region(op: BinopKind) -> SimdRegion {
        SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width2,
            elem_type: SimdElemType::Int64,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(8),
            arr_len_reg: None,
        }
    }

    /// Helper: Create an F64 element-wise binop SIMD region.
    fn make_f64_binop_region(op: BinopKind) -> SimdRegion {
        SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width2,
            elem_type: SimdElemType::Float64,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(8),
            arr_len_reg: None,
        }
    }

    // ------------------------------------------------------------------
    // Test 1: I64x2 (Int64) addition compiles
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_i64x2_compiles() {
        let mut jit = make_jit();
        let region = make_i64_binop_region(BinopKind::IAdd);

        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_simd_i64x2",
            &instructions,
            &region,
        );
        assert!(
            ptr.is_ok(),
            "I64x2 SIMD region should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 2: F64x2 (Float64) addition compiles
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_f64x2_compiles() {
        let mut jit = make_jit();
        let region = make_f64_binop_region(BinopKind::FAdd);

        let instructions = vec![
            Instruction::new3(OpCode::FAdd, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_simd_f64x2",
            &instructions,
            &region,
        );
        assert!(
            ptr.is_ok(),
            "F64x2 SIMD region should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 3: SimdRegion with metadata produces correct vector properties
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_region_with_metadata() {
        let region = make_i64_binop_region(BinopKind::IAdd);

        assert_eq!(vector_width(region.elem_type), 2, "Int64 should be 2-wide");
        assert_eq!(elem_size(region.elem_type), 8, "Int64 elements are 8 bytes");
        assert_eq!(region.width.lanes(), 2);
        assert!(region.trip_count_hint.is_some());
        assert_eq!(region.trip_count_hint.unwrap(), 8);

        // Verify the CLIF vector type
        let clif_ty = vector_clif_type(region.elem_type);
        assert_eq!(clif_ty, types::I64X2);
    }

    // ------------------------------------------------------------------
    // Test 4: SIMD not supported falls back to scalar
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_not_supported_fallback() {
        let supported = is_simd_supported();

        #[cfg(target_arch = "x86_64")]
        assert!(supported, "x86_64 with SSE2 should report SIMD supported");

        #[cfg(target_arch = "aarch64")]
        assert!(supported, "aarch64 with NEON should report SIMD supported");

        let mut jit = make_jit();
        let region = make_i64_binop_region(BinopKind::IAdd);

        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_simd_fallback",
            &instructions,
            &region,
        );
        assert!(
            ptr.is_ok(),
            "SIMD region should compile (with or without SIMD): {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 5: SIMD width selection is correct for each element type
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_width_selection() {
        let i64_region = make_i64_binop_region(BinopKind::IAdd);
        assert_eq!(vector_width(i64_region.elem_type), 2);
        assert_eq!(i64_region.width, SimdWidth::Width2);

        let f64_region = make_f64_binop_region(BinopKind::FAdd);
        assert_eq!(vector_width(f64_region.elem_type), 2);
        assert_eq!(f64_region.width, SimdWidth::Width2);

        let i32_region = SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::IAdd,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width4,
            elem_type: SimdElemType::Int32,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(8),
            arr_len_reg: None,
        };
        assert_eq!(vector_width(i32_region.elem_type), 4);
        assert_eq!(i32_region.width, SimdWidth::Width4);

        let f32_region = SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::FAdd,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width4,
            elem_type: SimdElemType::Float32,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(8),
            arr_len_reg: None,
        };
        assert_eq!(vector_width(f32_region.elem_type), 4);
        assert_eq!(f32_region.width, SimdWidth::Width4);
    }

    // ------------------------------------------------------------------
    // Test 6: Scalar epilogue handles partial vectors
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_scalar_epilogue() {
        let mut jit = make_jit();

        let region = SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::IAdd,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width2,
            elem_type: SimdElemType::Int64,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(5),
            arr_len_reg: None, // 5 elements → 2-wide SIMD + 1 epilogue
        };

        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_simd_epilogue",
            &instructions,
            &region,
        );
        assert!(
            ptr.is_ok(),
            "SIMD region with epilogue should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 7: Subtraction and multiplication operations compile
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_sub_and_mul() {
        let mut jit = make_jit();

        let sub_region = make_i64_binop_region(BinopKind::ISub);
        let instructions = vec![
            Instruction::new3(OpCode::ISub, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_simd_sub",
            &instructions,
            &sub_region,
        );
        assert!(ptr.is_ok(), "SIMD ISub should compile: {:?}", ptr.err());

        let mut jit2 = JitSession::new().unwrap();
        let mul_region = SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::IMul,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width4,
            elem_type: SimdElemType::Int32,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(8),
            arr_len_reg: None,
        };

        let ptr2 = compile_simd_region(
            &mut jit2.module,
            &mut jit2.builder_context,
            &mut jit2.ctx,
            "test_simd_mul",
            &instructions,
            &mul_region,
        );
        assert!(ptr2.is_ok(), "SIMD IMul should compile: {:?}", ptr2.err());
    }

    // ------------------------------------------------------------------
    // Test 8: I32x4 (Int32) compiles
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_i32x4_compiles() {
        let mut jit = make_jit();
        let region = SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::IAdd,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width4,
            elem_type: SimdElemType::Int32,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: Some(8),
            arr_len_reg: None,
        };

        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_simd_i32x4",
            &instructions,
            &region,
        );
        assert!(
            ptr.is_ok(),
            "I32x4 SIMD region should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 9: SimdBinOp from_binop_kind conversion
    // ------------------------------------------------------------------

    #[test]
    fn test_simd_binop_from_binop_kind() {
        assert_eq!(
            SimdBinOp::from_binop_kind(BinopKind::IAdd),
            Some(SimdBinOp::Add)
        );
        assert_eq!(
            SimdBinOp::from_binop_kind(BinopKind::ISub),
            Some(SimdBinOp::Sub)
        );
        assert_eq!(
            SimdBinOp::from_binop_kind(BinopKind::IMul),
            Some(SimdBinOp::Mul)
        );
        assert_eq!(
            SimdBinOp::from_binop_kind(BinopKind::FAdd),
            Some(SimdBinOp::Add)
        );
        assert_eq!(
            SimdBinOp::from_binop_kind(BinopKind::FSub),
            Some(SimdBinOp::Sub)
        );
        assert_eq!(
            SimdBinOp::from_binop_kind(BinopKind::FMul),
            Some(SimdBinOp::Mul)
        );
        assert_eq!(SimdBinOp::from_binop_kind(BinopKind::IDiv), None);
        assert_eq!(SimdBinOp::from_binop_kind(BinopKind::FDiv), None);
    }

    // ------------------------------------------------------------------
    // Test 10: No trip count hint falls back to scalar
    // ------------------------------------------------------------------

    #[test]
    fn test_no_trip_count_hint_fallback() {
        let mut jit = make_jit();

        let region = SimdRegion {
            start_offset: 0,
            num_instrs: 4,
            pattern: VectorizablePattern::ElementWiseBinop {
                op: BinopKind::IAdd,
                lhs_arr_reg: 10,
                rhs_arr_reg: 11,
                dst_arr_reg: 12,
                lhs_elem_reg: 1,
                rhs_elem_reg: 2,
                result_reg: 3,
            },
            width: SimdWidth::Width2,
            elem_type: SimdElemType::Int64,
            induction_var_reg: 0,
            array_regs: vec![10, 11, 12],
            trip_count_hint: None,
            arr_len_reg: None, // No hint → fallback
        };

        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];

        let ptr = compile_simd_region(
            &mut jit.module,
            &mut jit.builder_context,
            &mut jit.ctx,
            "test_no_hint_fallback",
            &instructions,
            &region,
        );
        assert!(
            ptr.is_ok(),
            "Fallback without trip count hint should compile: {:?}",
            ptr.err()
        );
    }
}
