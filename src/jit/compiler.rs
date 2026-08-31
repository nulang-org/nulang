//! Bytecode to Cranelift IR compiler.
//!
//! Translates a contiguous region of Nulang bytecode into native machine
//! code via Cranelift. Each opcode is mapped to one or more CLIF
//! instructions, with NaN-tag-aware arithmetic delegated to runtime
//! helper functions (see `runtime.rs`).
//!
//! # Supported Opcodes
//!
//! | Category | Opcodes |
//! |----------|---------|
//! | Special | Nop, Halt, Const0-2, ConstM1 |
//! | Register | Load, Store, Move, Swap, Dup |
//! | Integer Arith | IAdd, ISub, IMul, IDiv, IMod, INeg, IInc, IDec |
//! | Bitwise | Xor, Shl, Shr, BitAnd, BitOr |
//! | Float Arith | FAdd, FSub, FMul, FDiv, FNeg |
//! | Compare | ICmp{Eq,Lt,Gt,Le,Ge}, FCmp{Eq,Lt,Gt} |
//! | Logic | Not, And, Or |
//! | Control | Jmp, JmpT, JmpF |
//! | Effects  | PerformDirect (yields to interpreter) |
//! | Convert | IToF, FToI |
//! | Debug | DbgPrint |

use std::collections::HashMap;

use cranelift::codegen::ir::FuncRef;
use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilder;
use cranelift_jit::JITModule;
use cranelift_module::{Linkage, Module};

use crate::bytecode::{Instruction, OpCode};
use crate::runtime::heap::{ActorHeap, OrcaHeader, TypeTag};
use crate::value_layout::{PAYLOAD_MASK, TAG_INT, TAG_MASK, TAG_NIL, TAG_PTR};

// ---------------------------------------------------------------------------
// Opcode Support Matrix
// ---------------------------------------------------------------------------

/// Check if an opcode can be compiled by the JIT.
pub fn is_opcode_compilable(op: OpCode) -> bool {
    matches!(
        op,
        OpCode::Nop
            | OpCode::Halt
            | OpCode::Const0
            | OpCode::Const1
            | OpCode::Const2
            | OpCode::ConstM1
            | OpCode::ConstU
            | OpCode::Load
            | OpCode::Store
            | OpCode::Move
            | OpCode::Swap
            | OpCode::Dup
            | OpCode::IAdd
            | OpCode::ISub
            | OpCode::IMul
            | OpCode::IDiv
            | OpCode::IMod
            | OpCode::INeg
            | OpCode::IInc
            | OpCode::IDec
            | OpCode::IPow
            | OpCode::FPow
            | OpCode::Xor
            | OpCode::Shl
            | OpCode::Shr
            | OpCode::BitAnd
            | OpCode::BitOr
            | OpCode::FAdd
            | OpCode::FSub
            | OpCode::FMul
            | OpCode::FDiv
            | OpCode::FNeg
            | OpCode::ICmpEq
            | OpCode::ICmpLt
            | OpCode::ICmpGt
            | OpCode::ICmpLe
            | OpCode::ICmpGe
            | OpCode::FCmpEq
            | OpCode::FCmpLt
            | OpCode::FCmpGt
            | OpCode::Not
            | OpCode::And
            | OpCode::Or
            | OpCode::Jmp
            | OpCode::JmpT
            | OpCode::JmpF
            | OpCode::IToF
            | OpCode::FToI
            | OpCode::DbgPrint
            | OpCode::Ret
            | OpCode::RetVal
            | OpCode::ArrLoad
            | OpCode::ArrStore
            | OpCode::ArrLen
            | OpCode::FieldL
            | OpCode::PerformDirect
    )
}

// ---------------------------------------------------------------------------
// Signature Helpers
// ---------------------------------------------------------------------------

pub(crate) fn make_bin_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

pub(crate) fn make_unary_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

pub(crate) fn make_void_reg3_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I32));
    sig.params.push(AbiParam::new(types::I32));
    sig
}

pub(crate) fn make_void_reg4_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I32));
    sig.params.push(AbiParam::new(types::I32));
    sig.params.push(AbiParam::new(types::I32));
    sig
}

/// `(*mut u64 regs, i64 func_idx, i64 argc, i64 dst) -> i64` for
/// `nulang_jit_direct_call` (returns a nonzero status on runtime error).
pub(crate) fn make_direct_call_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // regs ptr
    sig.params.push(AbiParam::new(types::I64)); // func_idx
    sig.params.push(AbiParam::new(types::I64)); // argc
    sig.params.push(AbiParam::new(types::I64)); // dst
    sig.returns.push(AbiParam::new(types::I64)); // status
    sig
}

// ---------------------------------------------------------------------------
// Runtime Helper Registration
// ---------------------------------------------------------------------------

// Re-export from the single source of truth.
pub use crate::jit::helpers::RuntimeHelper;

fn register_runtime_helpers<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
) -> Result<HashMap<RuntimeHelper, FuncRef>, CompileError> {
    crate::jit::helpers::register_with_module(module, builder)
}

// ---------------------------------------------------------------------------
// Compilation
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum CompileError {
    DeclareFailed(String),
    CompileFailed(String),
    /// The region contains an opcode this compiler does not support;
    /// callers should fall back to another compiler.
    UnsupportedOpcode(String),
    /// An internal invariant was violated (missing block, missing helper).
    Internal(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::DeclareFailed(msg) => write!(f, "function declaration failed: {}", msg),
            CompileError::CompileFailed(msg) => write!(f, "compilation failed: {}", msg),
            CompileError::UnsupportedOpcode(msg) => write!(f, "unsupported opcode: {}", msg),
            CompileError::Internal(msg) => write!(f, "internal compiler error: {}", msg),
        }
    }
}

impl std::error::Error for CompileError {}

/// Store the relative target pc (target - region start) to the JIT
/// BRANCH-EXIT slot, so the VM resumes the interpreter there after the region
/// returns. Used when a compiled branch targets an instruction outside the
/// region. A branch exit is NOT a suspension (unlike the JIT_YIELD_PC used by
/// PerformDirect / safepoints), so it must not make the VM halt.
pub(crate) fn emit_yield_pc(
    builder: &mut FunctionBuilder,
    branch_exit_helper: FuncRef,
    start_offset: usize,
    target: usize,
) {
    // Target may precede the region (a backward jump); the VM adds the signed
    // relative offset to the region-start pc, so wrap the subtraction.
    let rel = builder
        .ins()
        .iconst(types::I64, target.wrapping_sub(start_offset) as i64);
    builder.ins().call(branch_exit_helper, &[rel]);
}

/// Compile a bytecode region to a native function.
///
/// `native_calls` maps absolute pcs of `Call` instructions (within the
/// region) to their direct, provably-non-suspending callee's function-table
/// index. Those calls are compiled as `nulang_jit_direct_call` helper
/// invocations (the region stays resident in native code while the callee
/// runs on the interpreter frame stack). Any other `Call` in the region is a
/// compile error — `find_compilable_region_with_calls` only accepts regions
/// whose `Call` sites are all in this map.
pub fn compile_bytecode_region(
    module: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    ctx: &mut codegen::Context,
    func_name: &str,
    start_offset: usize,
    num_instrs: usize,
    instructions: &[Instruction],
    native_calls: &HashMap<usize, usize>,
) -> Result<*const u8, CompileError> {
    ctx.clear();

    let pointer_type = module.isa().pointer_type();
    ctx.func.signature.params.push(AbiParam::new(pointer_type));
    ctx.func.signature.params.push(AbiParam::new(pointer_type));

    let mut builder = FunctionBuilder::new(&mut ctx.func, builder_context);

    let entry_block = builder.create_block();
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);

    let regs_ptr = builder.block_params(entry_block)[0];
    let consts_ptr = builder.block_params(entry_block)[1];

    let helpers = register_runtime_helpers(module, &mut builder)?;

    let end_offset = (start_offset + num_instrs).min(instructions.len());
    let mut blocks: HashMap<usize, Block> = HashMap::new();
    for i in start_offset..end_offset {
        blocks.insert(i, builder.create_block());
    }
    let return_block = builder.create_block();
    // Inject a thread-local JIT safepoint check. A runtime helper is used
    // instead of an embedded process-global pointer so concurrent VMs cannot
    // consume each other's actor reduction counters.
    let zero = builder.ins().iconst(types::I64, 0);
    let safepoint = builder
        .ins()
        .call(helpers[&RuntimeHelper::SafePoint], &[zero]);
    let safepoint_result = builder.inst_results(safepoint)[0];
    let exhausted = builder.ins().icmp(IntCC::NotEqual, safepoint_result, zero);
    let yield_block = builder.create_block();
    if let Some(&first_block) = blocks.get(&start_offset) {
        builder
            .ins()
            .brif(exhausted, yield_block, &[], first_block, &[]);
    } else {
        builder
            .ins()
            .brif(exhausted, yield_block, &[], return_block, &[]);
    }

    // Yield block: mark a relative resume offset in thread-local state.
    builder.switch_to_block(yield_block);
    builder.set_cold_block(yield_block);
    let zero = builder.ins().iconst(types::I64, 0);
    builder
        .ins()
        .call(helpers[&RuntimeHelper::SetYield], &[zero]);
    builder.ins().jump(return_block, &[]);

    // Seal all new blocks.
    builder.seal_block(yield_block);
    for pc in start_offset..end_offset {
        let instr = instructions[pc];
        let block = *blocks
            .get(&pc)
            .ok_or_else(|| CompileError::Internal("missing block in compiled region".into()))?;
        builder.switch_to_block(block);

        // Mark interpreter-fallback blocks as cold so the hot path
        // stays contiguous in the I-cache.
        if matches!(instr.opcode, OpCode::PerformDirect) {
            builder.set_cold_block(block);
        }

        match instr.opcode {
            OpCode::Nop => {}
            OpCode::Halt => {
                builder.ins().jump(return_block, &[]);
            }
            OpCode::Const0 => {
                emit_const(&mut builder, regs_ptr, instr.op1 as usize, 0);
            }
            OpCode::Const1 => {
                emit_const(&mut builder, regs_ptr, instr.op1 as usize, 1);
            }
            OpCode::Const2 => {
                emit_const(&mut builder, regs_ptr, instr.op1 as usize, 2);
            }
            OpCode::ConstM1 => {
                emit_const(&mut builder, regs_ptr, instr.op1 as usize, -1);
            }
            OpCode::ConstU => {
                let idx = instr.imm16() as usize;
                let offset = (idx * 8) as i32;
                let addr = if offset == 0 {
                    consts_ptr
                } else {
                    let off = builder.ins().iconst(types::I64, offset as i64);
                    builder.ins().iadd(consts_ptr, off)
                };
                let val = builder.ins().load(types::I64, MemFlags::new(), addr, 0);
                store_reg(&mut builder, regs_ptr, instr.op3 as usize, val);
            }

            OpCode::Load | OpCode::Store | OpCode::Move | OpCode::Dup => {
                let v = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                store_reg(&mut builder, regs_ptr, instr.op2 as usize, v);
            }
            OpCode::Swap => {
                let v1 = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                let v2 = load_reg(&mut builder, regs_ptr, instr.op2 as usize);
                store_reg(&mut builder, regs_ptr, instr.op1 as usize, v2);
                store_reg(&mut builder, regs_ptr, instr.op2 as usize, v1);
            }

            OpCode::IAdd => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::IAdd,
            ),
            OpCode::ISub => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::ISub,
            ),
            OpCode::IMul => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::IMul,
            ),
            OpCode::IDiv => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::IDiv,
            ),
            OpCode::IMod => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::IMod,
            ),
            OpCode::INeg => emit_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                RuntimeHelper::INeg,
            ),
            // Both IPow and FPow route through nulang_pow, which dispatches on
            // the tagged operands (powf when both floats, wrapping int pow
            // otherwise) — matching the interpreter's step_ipow exactly.
            OpCode::IPow | OpCode::FPow => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::Pow,
            ),
            OpCode::IInc => emit_self_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                RuntimeHelper::IInc,
            ),
            OpCode::IDec => emit_self_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                RuntimeHelper::IDec,
            ),
            OpCode::Xor => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::Xor,
            ),
            OpCode::Shl => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::Shl,
            ),
            OpCode::Shr => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::Shr,
            ),
            OpCode::BitAnd => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::BitAnd,
            ),
            OpCode::BitOr => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::BitOr,
            ),

            OpCode::FAdd => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FAdd,
            ),
            OpCode::FSub => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FSub,
            ),
            OpCode::FMul => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FMul,
            ),
            OpCode::FDiv => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FDiv,
            ),
            // Interpreter reads src from op1 and writes dst to op3 for FNeg.
            OpCode::FNeg => emit_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op3 as usize,
                RuntimeHelper::FNeg,
            ),

            OpCode::ICmpEq => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::ICmpEq,
            ),
            OpCode::ICmpLt => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::ICmpLt,
            ),
            OpCode::ICmpGt => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::ICmpGt,
            ),
            OpCode::ICmpLe => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::ICmpLe,
            ),
            OpCode::ICmpGe => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::ICmpGe,
            ),
            OpCode::FCmpEq => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FCmpEq,
            ),
            OpCode::FCmpLt => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FCmpLt,
            ),
            OpCode::FCmpGt => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::FCmpGt,
            ),

            OpCode::Not => emit_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                RuntimeHelper::Not,
            ),
            OpCode::And => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::And,
            ),
            OpCode::Or => emit_binop(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                instr.op3 as usize,
                RuntimeHelper::Or,
            ),

            OpCode::Jmp => {
                let target = (pc as i64 + instr.simm16() as i64) as usize;
                if let Some(&target_block) = blocks.get(&target) {
                    builder.ins().jump(target_block, &[]);
                } else {
                    // Target outside the region: yield the target pc so the
                    // interpreter resumes there (branch-exit, not a suspension).
                    emit_yield_pc(
                        &mut builder,
                        helpers[&RuntimeHelper::SetBranchExit],
                        start_offset,
                        target,
                    );
                    builder.ins().jump(return_block, &[]);
                }
            }
            OpCode::JmpT => {
                let target = (pc as i64 + instr.offset16() as i64) as usize;
                let cond_val = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                // Branch conditions are NaN-tagged bools; truthiness is the low
                // payload bit (matches `Value::as_bool`), not the whole value.
                let one = builder.ins().iconst(types::I64, 1);
                let cond_bit = builder.ins().band(cond_val, one);
                let zero = builder.ins().iconst(types::I64, 0);
                let is_true = builder.ins().icmp(IntCC::NotEqual, cond_bit, zero);
                let fallthrough = *blocks.get(&(pc + 1)).unwrap_or(&return_block);
                if let Some(&target_block) = blocks.get(&target) {
                    builder
                        .ins()
                        .brif(is_true, target_block, &[], fallthrough, &[]);
                } else {
                    // Target outside the region: yield to it on the taken path.
                    // Fill the current block first (Cranelift requires the
                    // current block to be terminated before switching).
                    let outside = builder.create_block();
                    builder.ins().brif(is_true, outside, &[], fallthrough, &[]);
                    builder.switch_to_block(outside);
                    emit_yield_pc(
                        &mut builder,
                        helpers[&RuntimeHelper::SetBranchExit],
                        start_offset,
                        target,
                    );
                    builder.ins().jump(return_block, &[]);
                    builder.seal_block(outside);
                }
            }
            OpCode::JmpF => {
                let target = (pc as i64 + instr.offset16() as i64) as usize;
                let cond_val = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                let one = builder.ins().iconst(types::I64, 1);
                let cond_bit = builder.ins().band(cond_val, one);
                let zero = builder.ins().iconst(types::I64, 0);
                let is_false = builder.ins().icmp(IntCC::Equal, cond_bit, zero);
                let fallthrough = *blocks.get(&(pc + 1)).unwrap_or(&return_block);
                if let Some(&target_block) = blocks.get(&target) {
                    builder
                        .ins()
                        .brif(is_false, target_block, &[], fallthrough, &[]);
                } else {
                    // Target outside the region: yield to it on the taken path.
                    let outside = builder.create_block();
                    builder.ins().brif(is_false, outside, &[], fallthrough, &[]);
                    builder.switch_to_block(outside);
                    emit_yield_pc(
                        &mut builder,
                        helpers[&RuntimeHelper::SetBranchExit],
                        start_offset,
                        target,
                    );
                    builder.ins().jump(return_block, &[]);
                    builder.seal_block(outside);
                }
            }

            OpCode::IToF => emit_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                RuntimeHelper::IToF,
            ),
            OpCode::FToI => emit_unary(
                &mut builder,
                &helpers,
                regs_ptr,
                instr.op1 as usize,
                instr.op2 as usize,
                RuntimeHelper::FToI,
            ),

            OpCode::Call => {
                // A direct, provably-non-suspending call (recovered by
                // `find_compilable_region_with_calls`): run the callee to
                // completion via the re-entrant `nulang_jit_direct_call`
                // helper while this region stays resident in native code.
                let func_idx = match native_calls.get(&pc) {
                    Some(&idx) => idx as i64,
                    None => {
                        return Err(CompileError::Internal(
                            "Call in compiled region without a native-call entry".into(),
                        ))
                    }
                };
                let fidx = builder.ins().iconst(types::I64, func_idx);
                let argcv = builder.ins().iconst(types::I64, instr.op2 as i64);
                let dstv = builder.ins().iconst(types::I64, instr.op3 as i64);
                let status_inst = builder.ins().call(
                    helpers[&RuntimeHelper::DirectCall],
                    &[regs_ptr, fidx, argcv, dstv],
                );
                let status = builder.inst_results(status_inst)[0];
                // On nonzero status the callee raised (e.g. step-limit); the
                // error is already recorded in the pending-error thread-local,
                // so exit the region and let the VM propagate it.
                let zero = builder.ins().iconst(types::I64, 0);
                let is_err = builder.ins().icmp(IntCC::NotEqual, status, zero);
                let fallthrough = *blocks.get(&(pc + 1)).unwrap_or(&return_block);
                builder
                    .ins()
                    .brif(is_err, return_block, &[], fallthrough, &[]);
            }

            OpCode::Ret | OpCode::RetVal => {
                builder.ins().jump(return_block, &[]);
            }
            OpCode::DbgPrint => {}

            OpCode::ArrLoad => {
                emit_arr_load(
                    &mut builder,
                    regs_ptr,
                    instr.op1 as usize,
                    instr.op2 as usize,
                    instr.op3 as usize,
                );
            }
            OpCode::ArrStore => {
                emit_reg_call4(
                    &mut builder,
                    &helpers,
                    regs_ptr,
                    instr.op1,
                    instr.op2,
                    instr.op3,
                    RuntimeHelper::ArrStore,
                );
            }
            OpCode::ArrLen => {
                emit_reg_call3(
                    &mut builder,
                    &helpers,
                    regs_ptr,
                    instr.op1,
                    instr.op2,
                    RuntimeHelper::ArrLen,
                );
            }
            OpCode::FieldL => {
                emit_reg_call4(
                    &mut builder,
                    &helpers,
                    regs_ptr,
                    instr.op1,
                    instr.op2,
                    instr.op3,
                    RuntimeHelper::FieldL,
                );
            }
            OpCode::PerformDirect => {
                // Yield to interpreter at this exact instruction.
                // The interpreter handles continuation capture and
                // handler dispatch.  Store the relative PC offset
                // so try_jit_execute re-enters the interpreter at
                // the PerformDirect instruction.
                //
                // OPTIMIZATION (future): when the handler binding is
                // single-shot (see `HandlerBinding::single_shot`),
                // the handler body can be compiled inline — the
                // continuation is just the next PC and no heap
                // allocation is needed.  This requires (a) the JIT
                // compiler to access `CodeModule::handler_tables` to
                // look up the binding, and (b) support for compiling
                // non-contiguous handler bodies (different PC offset)
                // as callable subroutines within the JIT region.
                let rel_offset = builder.ins().iconst(types::I64, (pc - start_offset) as i64);
                builder
                    .ins()
                    .call(helpers[&RuntimeHelper::SetYield], &[rel_offset]);
                builder.ins().jump(return_block, &[]);
            }
            _ => {
                builder.ins().jump(return_block, &[]);
            }
        }

        let is_terminator = matches!(
            instr.opcode,
            OpCode::Jmp
                | OpCode::JmpT
                | OpCode::JmpF
                | OpCode::Halt
                | OpCode::Ret
                | OpCode::RetVal
                | OpCode::Call // folded direct call: emits its own brif exit
                | OpCode::PerformDirect
        );

        if !is_terminator {
            if let Some(&next_block) = blocks.get(&(pc + 1)) {
                builder.ins().jump(next_block, &[]);
            } else {
                builder.ins().jump(return_block, &[]);
            }
        }
    }

    for block in blocks.values() {
        builder.seal_block(*block);
    }

    builder.switch_to_block(return_block);
    builder.seal_block(return_block);
    builder.ins().return_(&[]);

    builder.finalize();
    let func_id = module
        .declare_function(func_name, Linkage::Local, &ctx.func.signature.clone())
        .map_err(|e| CompileError::DeclareFailed(format!("{}", e)))?;
    module
        .define_function(func_id, ctx)
        .map_err(|e| CompileError::CompileFailed(format!("{}", e)))?;
    module
        .finalize_definitions()
        .map_err(|e| CompileError::CompileFailed(format!("finalize: {}", e)))?;

    let code = module.get_finalized_function(func_id);
    Ok(code as *const u8)
}

// ---------------------------------------------------------------------------
// CLIF Generation Helpers — shared with typed_compiler.rs
// ---------------------------------------------------------------------------

use crate::jit::typed_compiler::{emit_const, load_reg, store_reg};

pub(crate) fn emit_arr_load(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    arr_reg: usize,
    idx_reg: usize,
    dst: usize,
) {
    // Load NaN-boxed array pointer and index.
    let arr_val = load_reg(builder, regs_ptr, arr_reg);
    let idx_val = load_reg(builder, regs_ptr, idx_reg);

    // Interpreter parity (vm.rs `OpCode::ArrLoad`): the load yields nil
    // unless the array register holds a non-null heap pointer to an
    // Array-typed object AND the index is in bounds — never a raw
    // dereference. The `#[repr(C)]` `OrcaHeader` sits immediately before
    // the payload pointer; field offsets come from `offset_of!` so they
    // track the struct layout.
    let header_size = builder
        .ins()
        .iconst(types::I64, ActorHeap::HEADER_SIZE as i64);
    let nil_bits = builder.ins().iconst(types::I64, TAG_NIL as i64);

    let arr_tag = builder.ins().band_imm(arr_val, TAG_MASK as i64);
    let is_ptr = builder
        .ins()
        .icmp_imm(IntCC::Equal, arr_tag, TAG_PTR as i64);
    // Extract raw pointer (mask off tag bits from NaN-boxed pointer).
    let arr_ptr = builder.ins().band_imm(arr_val, PAYLOAD_MASK as i64);
    let non_null = builder.ins().icmp_imm(IntCC::NotEqual, arr_ptr, 0);
    let can_read_header = builder.ins().band(is_ptr, non_null);

    let header_blk = builder.create_block();
    let bounds_blk = builder.create_block();
    let load_blk = builder.create_block();
    let nil_blk = builder.create_block();
    let merge_blk = builder.create_block();

    builder
        .ins()
        .brif(can_read_header, header_blk, &[], nil_blk, &[]);

    // Header check: the object must carry the Array type tag.
    builder.switch_to_block(header_blk);
    let header = builder.ins().isub(arr_ptr, header_size);
    let type_tag = builder.ins().load(
        types::I8,
        MemFlags::new(),
        header,
        std::mem::offset_of!(OrcaHeader, type_tag) as i32,
    );
    let is_array = builder
        .ins()
        .icmp_imm(IntCC::Equal, type_tag, TypeTag::Array as i64);
    builder.ins().brif(is_array, bounds_blk, &[], nil_blk, &[]);

    // Bounds check: len = (header.size - header_size) / 8. The unsigned
    // compare also rejects negative indices (huge when viewed unsigned),
    // and the int-tag select mirrors `as_int().unwrap_or(0)`.
    builder.switch_to_block(bounds_blk);
    let size = builder.ins().load(
        types::I64,
        MemFlags::new(),
        header,
        std::mem::offset_of!(OrcaHeader, size) as i32,
    );
    let payload = builder.ins().isub(size, header_size);
    let len = builder.ins().ushr_imm(payload, 3);
    let shifted = builder.ins().ishl_imm(idx_val, 16);
    let idx_sext = builder.ins().sshr_imm(shifted, 16);
    let idx_tag = builder.ins().band_imm(idx_val, TAG_MASK as i64);
    let is_int = builder
        .ins()
        .icmp_imm(IntCC::Equal, idx_tag, TAG_INT as i64);
    let zero = builder.ins().iconst(types::I64, 0);
    let idx_raw = builder.ins().select(is_int, idx_sext, zero);
    let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, idx_raw, len);
    builder.ins().brif(in_bounds, load_blk, &[], nil_blk, &[]);

    // In bounds: load the tagged Value at arr_ptr + idx * 8.
    builder.switch_to_block(load_blk);
    let offset = builder.ins().ishl_imm(idx_raw, 3);
    let addr = builder.ins().iadd(arr_ptr, offset);
    let result = builder.ins().load(types::I64, MemFlags::new(), addr, 0);
    store_reg(builder, regs_ptr, dst, result);
    builder.ins().jump(merge_blk, &[]);

    // Any failed check produces nil, exactly like the interpreter.
    builder.switch_to_block(nil_blk);
    store_reg(builder, regs_ptr, dst, nil_bits);
    builder.ins().jump(merge_blk, &[]);

    // Every predecessor edge is emitted now; the caller adds the
    // fallthrough jump out of merge_blk.
    builder.seal_block(header_blk);
    builder.seal_block(bounds_blk);
    builder.seal_block(load_blk);
    builder.seal_block(nil_blk);
    builder.seal_block(merge_blk);
    builder.switch_to_block(merge_blk);
}

fn emit_binop(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<RuntimeHelper, FuncRef>,
    regs_ptr: Value,
    op1: usize,
    op2: usize,
    dst: usize,
    helper: RuntimeHelper,
) {
    let a = load_reg(builder, regs_ptr, op1);
    let b = load_reg(builder, regs_ptr, op2);
    let func_ref = *helpers
        .get(&helper)
        .expect("runtime helper not registered in helpers map");
    let call = builder.ins().call(func_ref, &[a, b]);
    let result = builder.inst_results(call)[0];
    store_reg(builder, regs_ptr, dst, result);
}

fn emit_unary(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<RuntimeHelper, FuncRef>,
    regs_ptr: Value,
    src: usize,
    dst: usize,
    helper: RuntimeHelper,
) {
    let a = load_reg(builder, regs_ptr, src);
    let func_ref = *helpers
        .get(&helper)
        .expect("runtime helper not registered in helpers map");
    let call = builder.ins().call(func_ref, &[a]);
    let result = builder.inst_results(call)[0];
    store_reg(builder, regs_ptr, dst, result);
}
fn emit_self_unary(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<RuntimeHelper, FuncRef>,
    regs_ptr: Value,
    reg: usize,
    helper: RuntimeHelper,
) {
    emit_unary(builder, helpers, regs_ptr, reg, reg, helper);
}

fn emit_reg_call3(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<RuntimeHelper, FuncRef>,
    regs_ptr: Value,
    r1: u8,
    r2: u8,
    helper: RuntimeHelper,
) {
    let func_ref = *helpers.get(&helper).expect("helper not registered");
    let a = builder.ins().iconst(types::I32, r1 as i64);
    let b = builder.ins().iconst(types::I32, r2 as i64);
    builder.ins().call(func_ref, &[regs_ptr, a, b]);
}

fn emit_reg_call4(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<RuntimeHelper, FuncRef>,
    regs_ptr: Value,
    r1: u8,
    r2: u8,
    r3: u8,
    helper: RuntimeHelper,
) {
    let func_ref = *helpers.get(&helper).expect("helper not registered");
    let a = builder.ins().iconst(types::I32, r1 as i64);
    let b = builder.ins().iconst(types::I32, r2 as i64);
    let c = builder.ins().iconst(types::I32, r3 as i64);
    builder.ins().call(func_ref, &[regs_ptr, a, b, c]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::OpCode;

    #[test]
    fn test_is_opcode_compilable_mvp() {
        assert!(is_opcode_compilable(OpCode::IAdd));
        assert!(is_opcode_compilable(OpCode::ISub));
        assert!(is_opcode_compilable(OpCode::Move));
        assert!(is_opcode_compilable(OpCode::Jmp));
        assert!(is_opcode_compilable(OpCode::Ret));
    }

    #[test]
    fn test_is_opcode_compilable_extended() {
        // Register copies.
        assert!(is_opcode_compilable(OpCode::Load));
        assert!(is_opcode_compilable(OpCode::Store));
        // Exponentiation (routes through nulang_pow).
        assert!(is_opcode_compilable(OpCode::IPow));
        assert!(is_opcode_compilable(OpCode::FPow));
        // Bitwise integer ops.
        assert!(is_opcode_compilable(OpCode::Xor));
        assert!(is_opcode_compilable(OpCode::Shl));
        assert!(is_opcode_compilable(OpCode::Shr));
        assert!(is_opcode_compilable(OpCode::BitAnd));
        assert!(is_opcode_compilable(OpCode::BitOr));
        // Float negate.
        assert!(is_opcode_compilable(OpCode::FNeg));
        // Opcodes the interpreter itself does not implement stay unsupported.
        // (IPow/FPow ARE implemented and now JIT-compilable.)
        assert!(!is_opcode_compilable(OpCode::FMod));
        assert!(!is_opcode_compilable(OpCode::ConstL));
    }

    #[test]
    fn test_is_opcode_compilable_not_mvp() {
        assert!(!is_opcode_compilable(OpCode::Spawn));
        assert!(!is_opcode_compilable(OpCode::Send));
        assert!(!is_opcode_compilable(OpCode::FFICall));
    }

    #[test]
    fn test_is_opcode_compilable_float_ops() {
        assert!(is_opcode_compilable(OpCode::FAdd));
        assert!(is_opcode_compilable(OpCode::FSub));
        assert!(is_opcode_compilable(OpCode::FMul));
        assert!(is_opcode_compilable(OpCode::FDiv));
        assert!(is_opcode_compilable(OpCode::FCmpEq));
        assert!(is_opcode_compilable(OpCode::FCmpLt));
        assert!(is_opcode_compilable(OpCode::FCmpGt));
    }

    #[test]
    fn test_is_opcode_compilable_conversion() {
        assert!(is_opcode_compilable(OpCode::IToF));
        assert!(is_opcode_compilable(OpCode::FToI));
        assert!(is_opcode_compilable(OpCode::INeg));
        assert!(is_opcode_compilable(OpCode::IInc));
        assert!(is_opcode_compilable(OpCode::IDec));
    }

    #[test]
    fn test_is_opcode_compilable_logical() {
        assert!(is_opcode_compilable(OpCode::Not));
        assert!(is_opcode_compilable(OpCode::And));
        assert!(is_opcode_compilable(OpCode::Or));
        assert!(is_opcode_compilable(OpCode::DbgPrint));
    }

    #[test]
    fn test_is_opcode_compilable_compare() {
        assert!(is_opcode_compilable(OpCode::ICmpEq));
        assert!(is_opcode_compilable(OpCode::ICmpLt));
        assert!(is_opcode_compilable(OpCode::ICmpGt));
        assert!(is_opcode_compilable(OpCode::ICmpLe));
        assert!(is_opcode_compilable(OpCode::ICmpGe));
    }

    #[test]
    fn test_is_opcode_compilable_special() {
        assert!(is_opcode_compilable(OpCode::Nop));
        assert!(is_opcode_compilable(OpCode::Halt));
        assert!(is_opcode_compilable(OpCode::Const0));
        assert!(is_opcode_compilable(OpCode::ConstU));
    }

    #[test]
    fn test_is_opcode_compilable_swap_dup() {
        assert!(is_opcode_compilable(OpCode::Swap));
        assert!(is_opcode_compilable(OpCode::Dup));
    }
}
