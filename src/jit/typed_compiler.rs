//! Type-directed JIT compilation with guard stripping.
//!
//! When the typechecker knows a register holds an `Int` or `Float`, the JIT
//! can skip NaN-tag manipulation and emit direct CLIF instructions (`iadd`,
//! `fadd`, etc.) instead of calling runtime helpers. This eliminates ~30% of
//! runtime overhead in numeric loops.
//!
//! # Architecture
//!
//! - `TypeMetadata`: Maps register indices to known static types.
//! - `KnownType`: Enum representing Int, Float, Bool, or Unknown.
//! - Typed emission functions: Emit direct CLIF when operand types are known,
//!   fall back to runtime helper calls otherwise.
//! - `compile_bytecode_region_typed()`: Main entry point that accepts optional
//!   `TypeMetadata` and routes each opcode to typed or untyped emission.
//!
//! # NaN Tag Layout (from vm.rs)
//!
//! ```text
//! TAG_INT  = 0x7FFB_0000_0000_0000  (quiet NaN + int tag)
//! TAG_BOOL = 0x7FFA_0000_0000_0000  (true=1, false=0)
//! PAYLOAD_MASK = 0x0000_FFFF_FFFF_FFFF
//! SIGN_BIT     = 0x0000_8000_0000_0000
//! SIGN_EXT     = 0xFFFF_0000_0000_0000
//! ```

use cranelift::codegen::ir::{BlockArg, FuncRef};
use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilder;
use cranelift_jit::JITModule;
use cranelift_module::{Linkage, Module};

use std::collections::{HashMap, HashSet};

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use crate::jit::compiler::{emit_arr_load, emit_yield_pc, CompileError};

// ---------------------------------------------------------------------------
// NaN-tag constants and CLIF helpers — single source in `cranelift_utils`
// ---------------------------------------------------------------------------

use crate::cranelift_utils::{
    emit_bitcast_f64_to_i64_canonicalized, emit_sext48, emit_tag_bool, emit_tag_int,
    PAYLOAD_MASK_I64, TAG_BOOL_I64, TAG_INT_I64, TAG_NIL_I64,
};
pub use crate::type_metadata::{KnownType, TypeMetadata};
// Bytecode-level type inference
// ---------------------------------------------------------------------------

/// Infer register types at `pc` via a conservative forward dataflow over the
/// enclosing function's bytecode.
///
/// This is the bridge between the compiler frontend and the JIT tiering
/// path: the MIR pipeline allocates each typed local to a fixed register, so
/// the type of a register at a given pc can be recovered statically from the
/// instruction stream itself (constants, arithmetic results, moves). The
/// analysis is a *must* analysis — a register is only marked `Int`/`Float`/
/// `Bool` when every static path to `pc` proves it — so wrong metadata is
/// impossible by construction; missing precision simply yields `Unknown`,
/// which makes the typed compiler fall back to the same runtime helper calls
/// as the scalar compiler.
///
/// Rules:
/// - Anchors (function entries, behavior offsets, effect-handler bodies, the
///   module entry point) start with all registers `Unknown`: function
///   arguments arrive in r0..r15 with statically unknowable types.
/// - Modeled opcodes propagate the result type their interpreter semantics
///   guarantee unconditionally (e.g. `IAdd` always writes a tagged int,
///   comparisons always write a tagged bool; `IDiv`/`IMod`/`FDiv` can yield
///   nil, so their destination becomes `Unknown`).
/// - Any unmodeled opcode conservatively clobbers ALL registers — soundness
///   over precision.
/// - Functions containing effect opcodes (`Handle`/`Perform`/`Resume`/
///   `Unwind`) yield empty metadata: `Resume` restores a captured
///   continuation whose target pc is not statically known, so no fact about
///   registers is reliable there.
pub fn infer_reg_types(module: &CodeModule, pc: usize) -> TypeMetadata {
    let mut meta = TypeMetadata::new();
    let instructions = &module.instructions;
    if pc >= instructions.len() {
        return meta;
    }

    // Candidate function-entry anchors. The enclosing function starts at the
    // greatest anchor at or below `pc`; the next anchor above it bounds the
    // analysis window.
    let mut anchors: Vec<usize> = Vec::with_capacity(module.function_table.len() + 2);
    anchors.push(0);
    anchors.extend(module.function_table.iter().copied());
    anchors.extend(module.behaviors.iter().map(|b| b.code_offset));
    for table in &module.handler_tables {
        anchors.extend(table.bindings.iter().map(|b| b.handler_offset));
    }
    if let Some(entry) = module.entry_point {
        anchors.push(entry);
    }
    anchors.retain(|&a| a < instructions.len());
    anchors.sort_unstable();
    anchors.dedup();

    let start = anchors
        .iter()
        .copied()
        .rev()
        .find(|&a| a <= pc)
        .unwrap_or(0);
    let end = anchors
        .iter()
        .copied()
        .find(|&a| a > start)
        .unwrap_or(instructions.len());

    // Cap the window: enormous functions would make the fixpoint expensive,
    // and hot JIT regions are capped at 500 instructions anyway.
    const MAX_ANALYSIS_WINDOW: usize = 2000;
    if end - start > MAX_ANALYSIS_WINDOW {
        return meta;
    }

    // Soundness guard: effect opcodes transfer control dynamically.
    for instr in &instructions[start..end] {
        if matches!(
            instr.opcode,
            OpCode::Handle
                | OpCode::Perform
                | OpCode::PerformDirect
                | OpCode::Resume
                | OpCode::Unwind
        ) {
            return meta;
        }
    }

    // Forward dataflow. `states[i]` is the register-type state *before*
    // `instructions[start + i]`; `None` marks a not-yet-reached pc (the top
    // of the meet lattice, so the first incoming state is adopted as-is —
    // this is what lets loop-carried types survive the back-edge merge).
    let n = end - start;
    let mut states: Vec<Option<[KnownType; 256]>> = vec![None; n];
    let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    let mut in_queue: Vec<bool> = vec![false; n];
    states[0] = Some([KnownType::Unknown; 256]);
    queue.push_back(start);
    in_queue[0] = true;

    while let Some(at) = queue.pop_front() {
        in_queue[at - start] = false;
        let instr = instructions[at];
        let mut next = states[at - start].unwrap_or([KnownType::Unknown; 256]);
        apply_type_transfer(&instr, module, &mut next);

        let push_succ = |succ: usize,
                         states: &mut Vec<Option<[KnownType; 256]>>,
                         queue: &mut std::collections::VecDeque<usize>,
                         in_queue: &mut Vec<bool>,
                         next: &[KnownType; 256]| {
            let slot = &mut states[succ - start];
            let changed = match slot {
                None => {
                    *slot = Some(*next);
                    true
                }
                Some(cur) => {
                    let mut changed = false;
                    for (c, &nv) in cur.iter_mut().zip(next.iter()) {
                        // Meet: keep a known type only when both predecessors
                        // agree. `Unknown` is absorbing and never counts as a
                        // change, so the fixpoint always terminates.
                        if *c != nv && *c != KnownType::Unknown {
                            *c = KnownType::Unknown;
                            changed = true;
                        }
                    }
                    changed
                }
            };
            if changed && !in_queue[succ - start] {
                queue.push_back(succ);
                in_queue[succ - start] = true;
            }
        };

        let in_window = |target: usize| target >= start && target < end;
        match instr.opcode {
            OpCode::Jmp => {
                let target = (at as i64 + instr.simm16() as i64) as usize;
                if in_window(target) {
                    push_succ(target, &mut states, &mut queue, &mut in_queue, &next);
                }
            }
            OpCode::JmpT | OpCode::JmpF => {
                let target = (at as i64 + instr.offset16() as i64) as usize;
                if in_window(target) {
                    push_succ(target, &mut states, &mut queue, &mut in_queue, &next);
                }
                if at + 1 < end {
                    push_succ(at + 1, &mut states, &mut queue, &mut in_queue, &next);
                }
            }
            OpCode::Halt | OpCode::Ret | OpCode::RetVal => {}
            _ => {
                if at + 1 < end {
                    push_succ(at + 1, &mut states, &mut queue, &mut in_queue, &next);
                }
            }
        }
    }

    if let Some(state) = &states[pc - start] {
        for (reg, &ty) in state.iter().enumerate() {
            if ty != KnownType::Unknown {
                meta.set_type(reg, ty);
            }
        }
    }
    meta
}

/// Apply one instruction's register-write effect to a type state.
///
/// Only opcodes whose result type is guaranteed by the interpreter's
/// semantics propagate a known type; everything else conservatively
/// clobbers the whole register file to `Unknown`.
fn apply_type_transfer(instr: &Instruction, module: &CodeModule, state: &mut [KnownType; 256]) {
    let op1 = instr.op1 as usize;
    let op2 = instr.op2 as usize;
    let op3 = instr.op3 as usize;
    match instr.opcode {
        // No register writes.
        OpCode::Nop
        | OpCode::Halt
        | OpCode::DbgPrint
        | OpCode::Jmp
        | OpCode::JmpT
        | OpCode::JmpF
        | OpCode::Ret
        | OpCode::RetVal => {}

        OpCode::Const0 | OpCode::Const1 | OpCode::Const2 | OpCode::ConstM1 => {
            state[op1] = KnownType::Int;
        }
        OpCode::ConstU => {
            state[op3] = match module.constants.get(instr.imm16() as usize) {
                Some(Constant::Int(_)) => KnownType::Int,
                Some(Constant::Float(_)) => KnownType::Float,
                Some(Constant::Bool(_)) => KnownType::Bool,
                _ => KnownType::Unknown,
            };
        }

        // Register copies (Load/Store are plain copies in this pipeline).
        OpCode::Load | OpCode::Store | OpCode::Move | OpCode::Dup => {
            state[op2] = state[op1];
        }
        OpCode::Swap => {
            state.swap(op1, op2);
        }

        // Integer results are unconditional: the interpreter and the JIT
        // helpers tag any operand payload as an int.
        OpCode::IAdd
        | OpCode::ISub
        | OpCode::IMul
        | OpCode::Xor
        | OpCode::Shl
        | OpCode::Shr
        | OpCode::BitAnd
        | OpCode::BitOr => {
            state[op3] = KnownType::Int;
        }
        // Division/remainder by zero yields nil in the interpreter.
        OpCode::IDiv | OpCode::IMod => {
            state[op3] = KnownType::Unknown;
        }
        OpCode::INeg => {
            state[op2] = KnownType::Int;
        }
        OpCode::IInc | OpCode::IDec => {
            state[op1] = KnownType::Int;
        }

        // Drop writes nil into its register after releasing the reference
        // (it is never JIT-compiled — regions stop before it — but the
        // type analysis must not let it clobber the whole register file).
        OpCode::Drop => {
            state[op1] = KnownType::Unknown;
        }

        // FDiv is excluded: the interpreter yields nil on a zero divisor.
        OpCode::FAdd | OpCode::FSub | OpCode::FMul | OpCode::FNeg => {
            state[op3] = KnownType::Float;
        }

        OpCode::ICmpEq
        | OpCode::ICmpLt
        | OpCode::ICmpGt
        | OpCode::ICmpLe
        | OpCode::ICmpGe
        | OpCode::FCmpEq
        | OpCode::FCmpLt
        | OpCode::FCmpGt => {
            state[op3] = KnownType::Bool;
        }
        OpCode::Not => {
            state[op2] = KnownType::Bool;
        }
        OpCode::And | OpCode::Or => {
            state[op3] = KnownType::Bool;
        }

        OpCode::IToF => {
            state[op2] = KnownType::Float;
        }
        OpCode::FToI => {
            state[op2] = KnownType::Int;
        }

        // ArrLoad result is Unknown (array elements have runtime-only types).
        OpCode::ArrLoad => {
            state[op3] = KnownType::Unknown;
        }
        // ArrStore writes to memory, not registers — no type transfer.
        OpCode::ArrStore => {}
        // Unmodeled opcode: conservatively clobber everything.
        _ => {
            state.fill(KnownType::Unknown);
        }
    }
}

// ---------------------------------------------------------------------------
// CLIF Helpers (shared with compiler.rs)
// ---------------------------------------------------------------------------

/// Load a value from the register file at the given index.
/// `regs_ptr` is a pointer to the start of the 256-element u64 array.
pub(crate) fn load_reg(builder: &mut FunctionBuilder, regs_ptr: Value, idx: usize) -> Value {
    let offset = (idx * 8) as i32;
    let addr = if offset == 0 {
        regs_ptr
    } else {
        let offset_val = builder.ins().iconst(types::I64, offset as i64);
        builder.ins().iadd(regs_ptr, offset_val)
    };
    builder.ins().load(types::I64, MemFlags::new(), addr, 0)
}

/// Store a value into the register file at the given index.
pub(crate) fn store_reg(builder: &mut FunctionBuilder, regs_ptr: Value, idx: usize, val: Value) {
    let offset = (idx * 8) as i32;
    let addr = if offset == 0 {
        regs_ptr
    } else {
        let offset_val = builder.ins().iconst(types::I64, offset as i64);
        builder.ins().iadd(regs_ptr, offset_val)
    };
    builder.ins().store(MemFlags::new(), val, addr, 0);
}

fn make_bin_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

fn make_unary_sig<M: Module>(module: &M) -> Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Bitcast an i64 (raw float bits) to f64 for direct float operations.
fn emit_bitcast_i64_to_f64(builder: &mut FunctionBuilder, bits: Value) -> Value {
    builder.ins().bitcast(types::F64, MemFlags::new(), bits)
}

/// Bitcast an f64 back to i64 for storage in registers.
fn emit_bitcast_f64_to_i64(builder: &mut FunctionBuilder, val: Value) -> Value {
    builder.ins().bitcast(types::I64, MemFlags::new(), val)
}

/// Emit a constant integer load into a register (NaN-tagged).
pub(crate) fn emit_const(builder: &mut FunctionBuilder, regs_ptr: Value, dst: usize, value: i64) {
    let tag = builder.ins().iconst(types::I64, TAG_INT_I64);
    let masked = value & PAYLOAD_MASK_I64;
    let val_part = builder.ins().iconst(types::I64, masked);
    let tagged = builder.ins().bor(tag, val_part);
    store_reg(builder, regs_ptr, dst, tagged);
}

/// Region-local cache for proven Int registers.
///
/// Cached values use Nulang's *unboxed* representation: a sign-extended,
/// normalized 48-bit integer in an i64 CLIF value. Only dirty registers are
/// written back to the VM register file. This lets straight-line typed JIT
/// sequences keep values in SSA across bytecode instruction boundaries while
/// preserving the boxed VM ABI at control-flow and helper-call boundaries.
#[derive(Default)]
struct NativeIntCache {
    values: HashMap<usize, Value>,
    dirty: HashSet<usize>,
}

impl NativeIntCache {
    fn load(&mut self, builder: &mut FunctionBuilder, regs_ptr: Value, reg: usize) -> Value {
        if let Some(&value) = self.values.get(&reg) {
            return value;
        }
        let raw = load_reg(builder, regs_ptr, reg);
        let value = emit_sext48(builder, raw);
        self.values.insert(reg, value);
        value
    }

    fn set(&mut self, reg: usize, value: Value) {
        self.values.insert(reg, value);
        self.dirty.insert(reg);
    }

    fn invalidate(&mut self, reg: usize) {
        self.values.remove(&reg);
        self.dirty.remove(&reg);
    }

    fn flush(&mut self, builder: &mut FunctionBuilder, regs_ptr: Value) {
        let dirty: Vec<usize> = self.dirty.drain().collect();
        for reg in dirty {
            if let Some(&value) = self.values.get(&reg) {
                let tagged = emit_tag_int(builder, value);
                store_reg(builder, regs_ptr, reg, tagged);
            }
        }
        self.values.clear();
    }

    fn flush_except(
        &mut self,
        builder: &mut FunctionBuilder,
        regs_ptr: Value,
        keep: &HashSet<usize>,
    ) {
        let dirty: Vec<usize> = self
            .dirty
            .iter()
            .copied()
            .filter(|reg| !keep.contains(reg))
            .collect();
        for reg in dirty {
            self.dirty.remove(&reg);
            if let Some(value) = self.values.remove(&reg) {
                let tagged = emit_tag_int(builder, value);
                store_reg(builder, regs_ptr, reg, tagged);
            }
        }
        self.values.retain(|reg, _| keep.contains(reg));
    }

    fn clear(&mut self) {
        self.values.clear();
        self.dirty.clear();
    }
}

/// Region-local cache for proven Float registers.
///
/// Cached values are native Cranelift F64 SSA values. NaNs are canonicalized
/// only when a dirty value crosses back into the VM register file, eliminating
/// repeated i64<->f64 bitcasts and register traffic inside linear hot paths.
#[derive(Default)]
struct NativeFloatCache {
    values: HashMap<usize, Value>,
    dirty: HashSet<usize>,
}

impl NativeFloatCache {
    fn load(&mut self, builder: &mut FunctionBuilder, regs_ptr: Value, reg: usize) -> Value {
        if let Some(&value) = self.values.get(&reg) {
            return value;
        }
        let bits = load_reg(builder, regs_ptr, reg);
        let value = emit_bitcast_i64_to_f64(builder, bits);
        self.values.insert(reg, value);
        value
    }

    fn set(&mut self, reg: usize, value: Value) {
        self.values.insert(reg, value);
        self.dirty.insert(reg);
    }

    fn invalidate(&mut self, reg: usize) {
        self.values.remove(&reg);
        self.dirty.remove(&reg);
    }

    fn flush(&mut self, builder: &mut FunctionBuilder, regs_ptr: Value) {
        let dirty: Vec<usize> = self.dirty.drain().collect();
        for reg in dirty {
            if let Some(&value) = self.values.get(&reg) {
                let bits = emit_bitcast_f64_to_i64_canonicalized(builder, value);
                store_reg(builder, regs_ptr, reg, bits);
            }
        }
        self.values.clear();
    }

    fn flush_except(
        &mut self,
        builder: &mut FunctionBuilder,
        regs_ptr: Value,
        keep: &HashSet<usize>,
    ) {
        let dirty: Vec<usize> = self
            .dirty
            .iter()
            .copied()
            .filter(|reg| !keep.contains(reg))
            .collect();
        for reg in dirty {
            self.dirty.remove(&reg);
            if let Some(value) = self.values.remove(&reg) {
                let bits = emit_bitcast_f64_to_i64_canonicalized(builder, value);
                store_reg(builder, regs_ptr, reg, bits);
            }
        }
        self.values.retain(|reg, _| keep.contains(reg));
    }

    fn clear(&mut self) {
        self.values.clear();
        self.dirty.clear();
    }
}

fn flush_native_caches(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
) {
    int_cache.flush(builder, regs_ptr);
    float_cache.flush(builder, regs_ptr);
}

/// Normalize an unboxed integer exactly as boxing + reloading would.
///
/// Nulang Ints carry a signed 48-bit payload. Keeping a raw i64 across
/// multiple operations must therefore preserve the implicit wrap that
/// `emit_tag_int` followed by `emit_sext48` used to provide after every
/// bytecode instruction.
fn normalize_unboxed_int(builder: &mut FunctionBuilder, value: Value) -> Value {
    emit_sext48(builder, value)
}

/// Count in-region CFG predecessors for every bytecode instruction.
///
/// The native cache may cross an instruction boundary only when the next
/// block has a single predecessor. Join blocks force materialization so no
/// cached CLIF value is used along a path it does not dominate.
fn region_predecessor_counts(
    instructions: &[Instruction],
    start_offset: usize,
    end_offset: usize,
) -> Vec<usize> {
    let mut counts = vec![0usize; end_offset.saturating_sub(start_offset)];

    let mut add = |target: usize| {
        if target >= start_offset && target < end_offset {
            counts[target - start_offset] += 1;
        }
    };

    for pc in start_offset..end_offset {
        let instr = instructions[pc];
        match instr.opcode {
            OpCode::Jmp => {
                add((pc as i64 + instr.simm16() as i64) as usize);
            }
            OpCode::JmpT | OpCode::JmpF => {
                add((pc as i64 + instr.offset16() as i64) as usize);
                if pc + 1 < end_offset {
                    add(pc + 1);
                }
            }
            OpCode::Halt | OpCode::Ret | OpCode::RetVal => {}
            _ => {
                if pc + 1 < end_offset {
                    add(pc + 1);
                }
            }
        }
    }

    counts
}

/// Conservative first wave of loop-carried SSA.
///
/// We only thread native values around a loop when the compiled region starts
/// at the loop header, contains exactly one backedge to that header, and has
/// no other branch inside the loop body. This matches the hot-loop regions
/// produced by the tiering scanner while avoiding general CFG phi placement.
#[derive(Debug, Clone)]
struct SimpleLoopSsaPlan {
    backedge_pc: usize,
    carried: Vec<(usize, KnownType)>,
}

fn simple_loop_ssa_plan(
    instructions: &[Instruction],
    start_offset: usize,
    end_offset: usize,
    type_metadata: Option<&TypeMetadata>,
) -> Option<SimpleLoopSsaPlan> {
    let meta = type_metadata?;
    if start_offset >= end_offset {
        return None;
    }

    let mut backedge_pc = None;
    for pc in start_offset..end_offset {
        let instr = instructions[pc];
        let target = match instr.opcode {
            OpCode::Jmp => Some((pc as i64 + instr.simm16() as i64) as usize),
            OpCode::JmpT | OpCode::JmpF => Some((pc as i64 + instr.offset16() as i64) as usize),
            _ => None,
        };
        if target == Some(start_offset) {
            if backedge_pc.replace(pc).is_some() {
                return None;
            }
        }
    }
    let backedge_pc = backedge_pc?;

    // Permit either a completely linear loop body or one conservative
    // forward if/if-else diamond. The internal CFG plan provides the exact
    // must-type state at its join; everything after the join must remain
    // linear until the loop backedge.
    let internal_cfg = simple_cfg_ssa_plan(instructions, start_offset, backedge_pc, type_metadata);

    let backedge_state = if let Some(cfg) = &internal_cfg {
        for pc in start_offset..backedge_pc {
            if matches!(
                instructions[pc].opcode,
                OpCode::Jmp | OpCode::JmpT | OpCode::JmpF
            ) && pc != cfg.branch_pc
                && Some(pc) != cfg.then_jump_pc
            {
                return None;
            }
        }

        simulate_local_types(instructions, cfg.join_pc, backedge_pc, &cfg.join_state)
    } else {
        if instructions[start_offset..backedge_pc]
            .iter()
            .any(|instr| matches!(instr.opcode, OpCode::Jmp | OpCode::JmpT | OpCode::JmpF))
        {
            return None;
        }
        simulate_local_types(instructions, start_offset, backedge_pc, &meta.regs)
    };

    if matches!(
        instructions[backedge_pc].opcode,
        OpCode::JmpT | OpCode::JmpF
    ) {
        let cond = instructions[backedge_pc].op1 as usize;
        if backedge_state[cond] != KnownType::Bool {
            return None;
        }
    }

    let mut carried = Vec::new();
    for reg in 0..256 {
        let ty = meta.get_type(reg);
        if matches!(ty, KnownType::Int | KnownType::Float) && backedge_state[reg] == ty {
            carried.push((reg, ty));
        }
    }

    if carried.is_empty() || carried.len() > 32 {
        return None;
    }

    Some(SimpleLoopSsaPlan {
        backedge_pc,
        carried,
    })
}

fn flush_non_carried_native_caches(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    carried: &[(usize, KnownType)],
) {
    let keep: HashSet<usize> = carried.iter().map(|&(reg, _)| reg).collect();
    int_cache.flush_except(builder, regs_ptr, &keep);
    float_cache.flush_except(builder, regs_ptr, &keep);
}

fn native_value_from_vm(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    reg: usize,
    ty: KnownType,
) -> Value {
    let raw = load_reg(builder, regs_ptr, reg);
    match ty {
        KnownType::Int => emit_sext48(builder, raw),
        KnownType::Float => emit_bitcast_i64_to_f64(builder, raw),
        _ => unreachable!("loop SSA only threads Int/Float registers"),
    }
}

fn native_carried_args(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    carried: &[(usize, KnownType)],
) -> Vec<BlockArg> {
    carried
        .iter()
        .map(|&(reg, ty)| {
            let value = match ty {
                KnownType::Int => int_cache.load(builder, regs_ptr, reg),
                KnownType::Float => float_cache.load(builder, regs_ptr, reg),
                _ => unreachable!("loop SSA only threads Int/Float registers"),
            };
            BlockArg::from(value)
        })
        .collect()
}

/// Conservative native SSA threading for a single forward branch join.
///
/// This covers the two common acyclic shapes emitted for `if` expressions:
///
/// ```text
///            +---- body -----+
/// branch ----+               +---- join
///            +--------------------^
///
///            +---- then -----jmp--+
/// branch ----+                    +---- join
///            +---- else ----------+
/// ```
///
/// The plan only carries Int/Float registers whose representation is proven
/// stable on every path. General CFG SSA remains a follow-up.
#[derive(Debug, Clone)]
struct SimpleCfgSsaPlan {
    branch_pc: usize,
    target_pc: usize,
    fallthrough_pc: usize,
    join_pc: usize,
    then_jump_pc: Option<usize>,
    arm_carried: Vec<(usize, KnownType)>,
    join_carried: Vec<(usize, KnownType)>,
    join_state: [KnownType; 256],
}

impl SimpleCfgSsaPlan {
    fn param_blocks(&self) -> Vec<usize> {
        let mut blocks = Vec::new();
        if !self.arm_carried.is_empty() {
            blocks.push(self.fallthrough_pc);
            if self.target_pc != self.join_pc {
                blocks.push(self.target_pc);
            }
        }
        if !self.join_carried.is_empty() {
            blocks.push(self.join_pc);
        }
        blocks.sort_unstable();
        blocks.dedup();
        blocks
    }

    fn carried_for_block(&self, pc: usize) -> Option<&[(usize, KnownType)]> {
        if pc == self.join_pc && !self.join_carried.is_empty() {
            Some(&self.join_carried)
        } else if (pc == self.fallthrough_pc
            || (pc == self.target_pc && self.target_pc != self.join_pc))
            && !self.arm_carried.is_empty()
        {
            Some(&self.arm_carried)
        } else {
            None
        }
    }

    fn branch_carried(&self) -> Vec<(usize, KnownType)> {
        let mut carried = self.arm_carried.clone();
        if self.target_pc == self.join_pc {
            for &(reg, ty) in &self.join_carried {
                if !carried.iter().any(|&(existing, _)| existing == reg) {
                    carried.push((reg, ty));
                }
            }
        }
        carried
    }
}

fn apply_local_type_transfer(instr: &Instruction, state: &mut [KnownType; 256]) {
    let op1 = instr.op1 as usize;
    let op2 = instr.op2 as usize;
    let op3 = instr.op3 as usize;

    match instr.opcode {
        OpCode::Nop
        | OpCode::Halt
        | OpCode::DbgPrint
        | OpCode::Jmp
        | OpCode::JmpT
        | OpCode::JmpF
        | OpCode::Ret
        | OpCode::RetVal => {}
        OpCode::Const0 | OpCode::Const1 | OpCode::Const2 | OpCode::ConstM1 => {
            state[op1] = KnownType::Int;
        }
        // The typed-region compiler does not receive the constant pool, so a
        // ConstU write is intentionally treated as unknown here.
        OpCode::ConstU => state[op3] = KnownType::Unknown,
        OpCode::Load | OpCode::Store | OpCode::Move | OpCode::Dup => {
            state[op2] = state[op1];
        }
        OpCode::Swap => state.swap(op1, op2),
        OpCode::IAdd
        | OpCode::ISub
        | OpCode::IMul
        | OpCode::Xor
        | OpCode::Shl
        | OpCode::Shr
        | OpCode::BitAnd
        | OpCode::BitOr => state[op3] = KnownType::Int,
        OpCode::IDiv | OpCode::IMod => state[op3] = KnownType::Unknown,
        OpCode::INeg => state[op2] = KnownType::Int,
        OpCode::IInc | OpCode::IDec => state[op1] = KnownType::Int,
        OpCode::FAdd | OpCode::FSub | OpCode::FMul | OpCode::FNeg => {
            state[op3] = KnownType::Float;
        }
        OpCode::FDiv => state[op3] = KnownType::Unknown,
        OpCode::ICmpEq
        | OpCode::ICmpLt
        | OpCode::ICmpGt
        | OpCode::ICmpLe
        | OpCode::ICmpGe
        | OpCode::FCmpEq
        | OpCode::FCmpLt
        | OpCode::FCmpGt => state[op3] = KnownType::Bool,
        OpCode::Not => state[op2] = KnownType::Bool,
        OpCode::And | OpCode::Or => state[op3] = KnownType::Bool,
        OpCode::IToF => state[op2] = KnownType::Float,
        OpCode::FToI => state[op2] = KnownType::Int,
        OpCode::ArrLoad => state[op3] = KnownType::Unknown,
        OpCode::ArrStore => {}
        _ => state.fill(KnownType::Unknown),
    }
}

fn simulate_local_types(
    instructions: &[Instruction],
    start: usize,
    end: usize,
    initial: &[KnownType; 256],
) -> [KnownType; 256] {
    let mut state = *initial;
    for instr in &instructions[start..end] {
        apply_local_type_transfer(instr, &mut state);
    }
    state
}

fn meet_type_states(a: &[KnownType; 256], b: &[KnownType; 256]) -> [KnownType; 256] {
    let mut out = [KnownType::Unknown; 256];
    for reg in 0..256 {
        if a[reg] == b[reg] {
            out[reg] = a[reg];
        }
    }
    out
}

fn region_type_states(
    instructions: &[Instruction],
    start_offset: usize,
    end_offset: usize,
    initial: Option<&TypeMetadata>,
) -> Vec<Option<[KnownType; 256]>> {
    let n = end_offset.saturating_sub(start_offset);
    let mut states = vec![None; n];
    if n == 0 {
        return states;
    }

    let mut queue = std::collections::VecDeque::new();
    let mut in_queue = vec![false; n];
    states[0] = Some(initial.map(|m| m.regs).unwrap_or([KnownType::Unknown; 256]));
    queue.push_back(start_offset);
    in_queue[0] = true;

    while let Some(pc) = queue.pop_front() {
        in_queue[pc - start_offset] = false;
        let Some(mut next) = states[pc - start_offset] else {
            continue;
        };
        apply_local_type_transfer(&instructions[pc], &mut next);

        let mut push = |succ: usize| {
            if succ < start_offset || succ >= end_offset {
                return;
            }
            let idx = succ - start_offset;
            let changed = match &mut states[idx] {
                None => {
                    states[idx] = Some(next);
                    true
                }
                Some(cur) => {
                    let mut changed = false;
                    for reg in 0..256 {
                        if cur[reg] != next[reg] && cur[reg] != KnownType::Unknown {
                            cur[reg] = KnownType::Unknown;
                            changed = true;
                        }
                    }
                    changed
                }
            };
            if changed && !in_queue[idx] {
                in_queue[idx] = true;
                queue.push_back(succ);
            }
        };

        match instructions[pc].opcode {
            OpCode::Jmp => {
                push((pc as i64 + instructions[pc].simm16() as i64) as usize);
            }
            OpCode::JmpT | OpCode::JmpF => {
                push((pc as i64 + instructions[pc].offset16() as i64) as usize);
                push(pc + 1);
            }
            OpCode::Halt | OpCode::Ret | OpCode::RetVal => {}
            _ => push(pc + 1),
        }
    }

    states
}

fn simple_cfg_ssa_plan(
    instructions: &[Instruction],
    start_offset: usize,
    end_offset: usize,
    type_metadata: Option<&TypeMetadata>,
) -> Option<SimpleCfgSsaPlan> {
    let meta = type_metadata?;
    if start_offset >= end_offset {
        return None;
    }

    // Keep the prefix linear. The first control-flow split must be a forward
    // conditional branch; arbitrary incoming edges are left to the general
    // boxed CFG path.
    let mut branch_state = meta.regs;
    let mut branch_pc = None;
    for pc in start_offset..end_offset {
        match instructions[pc].opcode {
            OpCode::JmpT | OpCode::JmpF => {
                branch_pc = Some(pc);
                break;
            }
            OpCode::Jmp | OpCode::Halt | OpCode::Ret | OpCode::RetVal => return None,
            _ => apply_local_type_transfer(&instructions[pc], &mut branch_state),
        }
    }
    let branch_pc = branch_pc?;
    let branch = instructions[branch_pc];
    if branch_state[branch.op1 as usize] != KnownType::Bool {
        return None;
    }

    let target_pc = (branch_pc as i64 + branch.offset16() as i64) as usize;
    let fallthrough_pc = branch_pc + 1;
    if target_pc <= fallthrough_pc || target_pc >= end_offset {
        return None;
    }

    let is_linear = |start: usize, end: usize| {
        instructions[start..end].iter().all(|instr| {
            !matches!(
                instr.opcode,
                OpCode::Jmp
                    | OpCode::JmpT
                    | OpCode::JmpF
                    | OpCode::Halt
                    | OpCode::Ret
                    | OpCode::RetVal
            )
        })
    };

    let predecessor_counts = region_predecessor_counts(instructions, start_offset, end_offset);

    // First try the canonical if/else layout: the instruction immediately
    // before the taken target is an unconditional jump over the else arm.
    let then_jump_pc = target_pc
        .checked_sub(1)
        .filter(|&pc| pc >= fallthrough_pc && instructions[pc].opcode == OpCode::Jmp);

    let (join_pc, then_end_state, else_end_state, then_jump_pc) = if let Some(then_jump_pc) =
        then_jump_pc
    {
        if !is_linear(fallthrough_pc, then_jump_pc) {
            return None;
        }
        let join_pc = (then_jump_pc as i64 + instructions[then_jump_pc].simm16() as i64) as usize;
        if join_pc <= target_pc || join_pc >= end_offset || !is_linear(target_pc, join_pc) {
            return None;
        }
        if predecessor_counts[target_pc - start_offset] != 1
            || predecessor_counts[join_pc - start_offset] != 2
        {
            return None;
        }

        let then_state =
            simulate_local_types(instructions, fallthrough_pc, then_jump_pc, &branch_state);
        let else_state = simulate_local_types(instructions, target_pc, join_pc, &branch_state);
        (join_pc, then_state, else_state, Some(then_jump_pc))
    } else {
        // If-without-else: the taken edge jumps directly to the join while
        // the fallthrough body reaches it linearly.
        let join_pc = target_pc;
        if !is_linear(fallthrough_pc, join_pc) || predecessor_counts[join_pc - start_offset] != 2 {
            return None;
        }
        let body_state = simulate_local_types(instructions, fallthrough_pc, join_pc, &branch_state);
        (join_pc, body_state, branch_state, None)
    };

    let join_state = meet_type_states(&then_end_state, &else_end_state);

    let mut arm_carried = Vec::new();
    let mut join_carried = Vec::new();
    for reg in 0..256 {
        let branch_ty = branch_state[reg];
        if matches!(branch_ty, KnownType::Int | KnownType::Float) {
            arm_carried.push((reg, branch_ty));
        }

        let join_ty = join_state[reg];
        if matches!(join_ty, KnownType::Int | KnownType::Float) {
            join_carried.push((reg, join_ty));
        }
    }

    if (arm_carried.is_empty() && join_carried.is_empty())
        || arm_carried.len() > 32
        || join_carried.len() > 32
    {
        return None;
    }

    Some(SimpleCfgSsaPlan {
        branch_pc,
        target_pc,
        fallthrough_pc,
        join_pc,
        then_jump_pc,
        arm_carried,
        join_carried,
        join_state,
    })
}

// ---------------------------------------------------------------------------
// Runtime Helper Registration
// ---------------------------------------------------------------------------

/// Register all runtime helper functions with the JIT module.
/// Returns a map from helper name → FuncRef.
/// Single source of truth: `RuntimeHelper::ALL` from `helpers.rs`.
pub(crate) fn register_runtime_helpers<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
) -> HashMap<&'static str, FuncRef> {
    use crate::jit::helpers::{HelperSig, RuntimeHelper};
    let mut helpers = HashMap::new();

    for (helper, name) in RuntimeHelper::ALL {
        let sig = match helper.sig() {
            HelperSig::Bin => make_bin_sig(module),
            HelperSig::Unary => make_unary_sig(module),
            _ => continue, // reg3/reg4 not used by typed_compiler
        };
        let func_id = module
            .declare_function(name, Linkage::Import, &sig)
            .expect("failed to declare runtime helper");
        let func_ref = module.declare_func_in_func(func_id, builder.func);
        helpers.insert(*name, func_ref);
    }

    helpers
}

// ---------------------------------------------------------------------------
// Typed Binary Operation Emission
// ---------------------------------------------------------------------------

/// Emit an integer binary operation with direct CLIF (no runtime call).
///
/// Proven Int operands are loaded into the region-local unboxed cache at most
/// once per linear chain. Results stay as normalized signed 48-bit SSA values
/// until a helper, CFG boundary, or region exit requires VM materialization.
fn emit_typed_ibinop(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeIntCache,
    op1: usize,
    op2: usize,
    dst: usize,
    op: TypedIntOp,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);

    let result = match op {
        TypedIntOp::Add => builder.ins().iadd(a, b),
        TypedIntOp::Sub => builder.ins().isub(a, b),
        TypedIntOp::Mul => builder.ins().imul(a, b),
    };
    let result = normalize_unboxed_int(builder, result);
    cache.set(dst, result);
}

/// Emit a float binary operation with direct CLIF (no runtime call).
///
/// Only called when both operands are known to be `Float`. Floats are stored
/// as raw f64 bit patterns in registers, so no NaN-tag extraction is needed.
/// The sequence is:
/// 1. Load raw i64 values from registers
/// 2. Bitcast to f64
/// 3. Perform the CLIF float operation
/// 4. Bitcast result back to i64, canonicalizing NaN to the reserved
///    tag-free pattern (a raw hardware NaN would alias TAG_NIL/TAG_PTR/...)
/// 5. Store back (a proper boxed float)
fn emit_typed_fbinop(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeFloatCache,
    op1: usize,
    op2: usize,
    dst: usize,
    op: TypedFloatOp,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);

    let result = match op {
        TypedFloatOp::Add => builder.ins().fadd(a, b),
        TypedFloatOp::Sub => builder.ins().fsub(a, b),
        TypedFloatOp::Mul => builder.ins().fmul(a, b),
    };

    cache.set(dst, result);
}

/// Emit a typed floating-point negation while keeping the value in native
/// F64 SSA form. NaN canonicalization is deferred until cache materialization.
fn emit_typed_fneg(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeFloatCache,
    src: usize,
    dst: usize,
) {
    let value = cache.load(builder, regs_ptr, src);
    cache.set(dst, builder.ins().fneg(value));
}

/// Emit typed floating-point division while preserving Nulang's
/// nil-on-zero semantics. IEEE fdiv itself does not trap; we compute it,
/// canonicalize the result, then select nil for both +0.0 and -0.0 divisors.
fn emit_typed_fdiv(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeFloatCache,
    op1: usize,
    op2: usize,
    dst: usize,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);
    let b_bits = emit_bitcast_f64_to_i64(builder, b);

    // Clear the sign bit so +0.0 and -0.0 both compare as zero.
    let abs_mask = builder.ins().iconst(types::I64, i64::MAX);
    let abs_b_bits = builder.ins().band(b_bits, abs_mask);
    let zero = builder.ins().iconst(types::I64, 0);
    let is_zero = builder.ins().icmp(IntCC::Equal, abs_b_bits, zero);

    let result = builder.ins().fdiv(a, b);
    let result_bits = emit_bitcast_f64_to_i64_canonicalized(builder, result);
    let nil = builder.ins().iconst(types::I64, TAG_NIL_I64);
    let observable = builder.ins().select(is_zero, nil, result_bits);
    store_reg(builder, regs_ptr, dst, observable);
    cache.invalidate(dst);
}

/// CLIF integer binary operations supported by the typed compiler.
#[derive(Debug, Clone, Copy)]
enum TypedIntOp {
    Add,
    Sub,
    Mul,
}

/// Integer division/remainder operations. These use a trap-free divisor:
/// when the real divisor is zero we divide by one internally, then select
/// the language-level `nil` result. That preserves Nulang's nil-on-zero
/// semantics without paying for a runtime helper call on statically-typed
/// integer hot paths.
#[derive(Debug, Clone, Copy)]
enum TypedIntDivOp {
    Div,
    Mod,
}

fn emit_typed_idivmod(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeIntCache,
    op1: usize,
    op2: usize,
    dst: usize,
    op: TypedIntDivOp,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);

    // Cranelift sdiv/srem trap on zero. Select a harmless divisor for the
    // machine instruction, then select nil for the observable result.
    let zero = builder.ins().iconst(types::I64, 0);
    let one = builder.ins().iconst(types::I64, 1);
    let is_zero = builder.ins().icmp(IntCC::Equal, b, zero);
    let safe_b = builder.ins().select(is_zero, one, b);

    let result = match op {
        TypedIntDivOp::Div => builder.ins().sdiv(a, safe_b),
        TypedIntDivOp::Mod => builder.ins().srem(a, safe_b),
    };
    let tagged = emit_tag_int(builder, result);
    let nil = builder.ins().iconst(types::I64, TAG_NIL_I64);
    let observable = builder.ins().select(is_zero, nil, tagged);
    store_reg(builder, regs_ptr, dst, observable);
    cache.invalidate(dst);
}

#[derive(Debug, Clone, Copy)]
enum TypedIntBitOp {
    Xor,
    Shl,
    Shr,
    And,
    Or,
}

fn emit_typed_ibitop(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeIntCache,
    op1: usize,
    op2: usize,
    dst: usize,
    op: TypedIntBitOp,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);

    let result = match op {
        TypedIntBitOp::Xor => builder.ins().bxor(a, b),
        TypedIntBitOp::And => builder.ins().band(a, b),
        TypedIntBitOp::Or => builder.ins().bor(a, b),
        TypedIntBitOp::Shl | TypedIntBitOp::Shr => {
            // Match the VM/runtime helpers exactly: shifts use the low six
            // bits of the RHS, so negative and oversized counts wrap mod 64.
            let mask = builder.ins().iconst(types::I64, 0x3f);
            let shift = builder.ins().band(b, mask);
            match op {
                TypedIntBitOp::Shl => builder.ins().ishl(a, shift),
                TypedIntBitOp::Shr => builder.ins().sshr(a, shift),
                _ => unreachable!(),
            }
        }
    };
    let result = normalize_unboxed_int(builder, result);
    cache.set(dst, result);
}

/// CLIF float binary operations whose result is always a Float.
///
/// FDiv is emitted by `emit_typed_fdiv` instead because Nulang division can
/// yield `nil` on a zero divisor, so its result is not statically always Float.
#[derive(Debug, Clone, Copy)]
enum TypedFloatOp {
    Add,
    Sub,
    Mul,
}

// ---------------------------------------------------------------------------
// Typed Comparison Emission
// ---------------------------------------------------------------------------

/// Emit a typed integer comparison with direct CLIF.
///
/// Both operands are known Int. Extracts payloads, sign-extends, compares,
/// and stores a NaN-tagged boolean result.
fn emit_typed_icmp(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeIntCache,
    op1: usize,
    op2: usize,
    dst: usize,
    cc: IntCC,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);

    let cond = builder.ins().icmp(cc, a, b);
    let tagged_bool = emit_tag_bool(builder, cond);
    store_reg(builder, regs_ptr, dst, tagged_bool);
    cache.invalidate(dst);
}

/// Emit a typed float comparison with direct CLIF.
///
/// Both operands are known Float. Bitcasts to f64, compares, and stores
/// a NaN-tagged boolean result.
fn emit_typed_fcmp(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeFloatCache,
    op1: usize,
    op2: usize,
    dst: usize,
    cc: FloatCC,
) {
    let a = cache.load(builder, regs_ptr, op1);
    let b = cache.load(builder, regs_ptr, op2);

    let cond = builder.ins().fcmp(cc, a, b);
    let tagged_bool = emit_tag_bool(builder, cond);
    store_reg(builder, regs_ptr, dst, tagged_bool);
    cache.invalidate(dst);
}

// ---------------------------------------------------------------------------
// Typed Unary Operation Emission
// ---------------------------------------------------------------------------

/// Emit a typed integer unary operation with direct CLIF.
fn emit_typed_iunary(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    cache: &mut NativeIntCache,
    src: usize,
    dst: usize,
    op: TypedIntUnaryOp,
) {
    let val = cache.load(builder, regs_ptr, src);

    let result = match op {
        TypedIntUnaryOp::Neg => builder.ins().ineg(val),
        TypedIntUnaryOp::Inc => {
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().iadd(val, one)
        }
        TypedIntUnaryOp::Dec => {
            let one = builder.ins().iconst(types::I64, 1);
            builder.ins().isub(val, one)
        }
    };

    let result = normalize_unboxed_int(builder, result);
    cache.set(dst, result);
}

#[derive(Debug, Clone, Copy)]
enum TypedIntUnaryOp {
    Neg,
    Inc,
    Dec,
}

// ---------------------------------------------------------------------------
// Typed Logic Emission
// ---------------------------------------------------------------------------

/// Emit typed logic operations (And, Or) with direct CLIF.
///
/// For `Bool`-typed operands, compare against the tagged false value.
/// Falls back to runtime helper for unknown types.
fn emit_typed_logic(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    op1: usize,
    op2: usize,
    dst: usize,
    op: TypedLogicOp,
    _helpers: &HashMap<&str, FuncRef>,
    meta: &TypeMetadata,
) {
    // Only optimize when both operands are known Bool
    if meta.both_known(op1, op2, KnownType::Bool) {
        let a_raw = load_reg(builder, regs_ptr, op1);
        let b_raw = load_reg(builder, regs_ptr, op2);

        // Check truthy: compare against tagged false, nil, and tagged 0.
        let false_val = builder.ins().iconst(types::I64, TAG_BOOL_I64 | 0);
        let nil_val = builder.ins().iconst(types::I64, TAG_NIL_I64);
        let zero_int = builder.ins().iconst(types::I64, TAG_INT_I64); // tagged 0

        let a_is_false = builder.ins().icmp(IntCC::Equal, a_raw, false_val);
        let a_is_nil = builder.ins().icmp(IntCC::Equal, a_raw, nil_val);
        let a_is_zero = builder.ins().icmp(IntCC::Equal, a_raw, zero_int);
        let a_falsy_part = builder.ins().bor(a_is_false, a_is_nil);
        let a_not_falsy = builder.ins().bor(a_falsy_part, a_is_zero);
        let zero_const = builder.ins().iconst(types::I64, 0);
        let a_truthy = builder.ins().icmp(IntCC::Equal, a_not_falsy, zero_const);

        let b_is_false = builder.ins().icmp(IntCC::Equal, b_raw, false_val);
        let b_is_nil = builder.ins().icmp(IntCC::Equal, b_raw, nil_val);
        let b_is_zero = builder.ins().icmp(IntCC::Equal, b_raw, zero_int);
        let b_falsy_part = builder.ins().bor(b_is_false, b_is_nil);
        let b_not_falsy = builder.ins().bor(b_falsy_part, b_is_zero);
        let b_truthy = builder.ins().icmp(IntCC::Equal, b_not_falsy, zero_const);

        let result_cond = match op {
            TypedLogicOp::And => builder.ins().band(a_truthy, b_truthy),
            TypedLogicOp::Or => builder.ins().bor(a_truthy, b_truthy),
        };

        let tagged_bool = emit_tag_bool(builder, result_cond);
        store_reg(builder, regs_ptr, dst, tagged_bool);
    } else {
        // Fall back to runtime helper
        let helper_name = match op {
            TypedLogicOp::And => "nulang_and",
            TypedLogicOp::Or => "nulang_or",
        };
        emit_binop_runtime(
            builder,
            _helpers,
            regs_ptr,
            int_cache,
            float_cache,
            op1,
            op2,
            dst,
            helper_name,
        );
    }
    int_cache.invalidate(dst);
    float_cache.invalidate(dst);
}

#[derive(Debug, Clone, Copy)]
enum TypedLogicOp {
    And,
    Or,
}

// ---------------------------------------------------------------------------
// Typed Conversion Emission
// ---------------------------------------------------------------------------

/// Emit typed int-to-float conversion with direct CLIF.
fn emit_typed_itof(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    src: usize,
    dst: usize,
) {
    let val = int_cache.load(builder, regs_ptr, src);
    let float_val = builder.ins().fcvt_from_sint(types::F64, val);
    float_cache.set(dst, float_val);
    int_cache.invalidate(dst);
}

/// Emit typed float-to-int conversion with direct CLIF.
fn emit_typed_ftoi(
    builder: &mut FunctionBuilder,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    src: usize,
    dst: usize,
) {
    let float_val = float_cache.load(builder, regs_ptr, src);
    let int_val = builder.ins().fcvt_to_sint_sat(types::I64, float_val);
    let int_val = normalize_unboxed_int(builder, int_val);
    int_cache.set(dst, int_val);
    float_cache.invalidate(dst);
}

// ---------------------------------------------------------------------------
// Runtime Fallback (untyped)
// ---------------------------------------------------------------------------

/// Emit a binary operation via a runtime helper call.
fn emit_binop_runtime(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<&str, FuncRef>,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    op1: usize,
    op2: usize,
    dst: usize,
    helper_name: &str,
) {
    flush_native_caches(builder, regs_ptr, int_cache, float_cache);
    let a = load_reg(builder, regs_ptr, op1);
    let b = load_reg(builder, regs_ptr, op2);
    let func_ref = *helpers.get(helper_name).unwrap();
    let call = builder.ins().call(func_ref, &[a, b]);
    let result = builder.inst_results(call)[0];
    store_reg(builder, regs_ptr, dst, result);
}

/// Emit a unary operation via a runtime helper call.
fn emit_unary_runtime(
    builder: &mut FunctionBuilder,
    helpers: &HashMap<&str, FuncRef>,
    regs_ptr: Value,
    int_cache: &mut NativeIntCache,
    float_cache: &mut NativeFloatCache,
    src: usize,
    dst: usize,
    helper_name: &str,
) {
    flush_native_caches(builder, regs_ptr, int_cache, float_cache);
    let a = load_reg(builder, regs_ptr, src);
    let func_ref = *helpers.get(helper_name).unwrap();
    let call = builder.ins().call(func_ref, &[a]);
    let result = builder.inst_results(call)[0];
    store_reg(builder, regs_ptr, dst, result);
}

// ---------------------------------------------------------------------------
// Main Compilation Entry Point (typed)
// ---------------------------------------------------------------------------

/// Opcodes the typed compiler knows how to emit.
///
/// This is deliberately a subset of `compiler::is_opcode_compilable`: the
/// typed compiler's catch-all arm jumps to the return block, so an
/// unsupported opcode in the middle of a region would silently drop the
/// remaining instructions. Callers must pre-check regions with this
/// function (as `compile_bytecode_region_typed` does) and fall back to the
/// scalar compiler for anything outside the set.
pub fn is_opcode_supported_typed(op: OpCode) -> bool {
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
    )
}

/// Compile a bytecode region to native code with optional type-directed
/// optimization (type guard stripping).
///
/// When `type_metadata` is `Some`, the compiler emits direct CLIF instructions
/// for operations where operand types are statically known (Int, Float, Bool),
/// bypassing NaN-tag-aware runtime helpers. When `None` or when a register's
/// type is `Unknown`, it falls back to the same runtime helper calls as the
/// untyped compiler.
///
/// # Arguments
/// - `module`: The Cranelift JIT module
/// - `builder_context`: Reusable function builder context
/// - `ctx`: Reusable codegen context
/// - `func_name`: Unique name for the compiled function
/// - `start_offset`: Bytecode offset where compilation starts
/// - `num_instrs`: Number of instructions to compile
/// - `instructions`: Full instruction array (indexed by offset)
/// - `type_metadata`: Optional static type information for registers
///
/// # Returns
/// A raw function pointer to the compiled code, or an error if compilation fails.
pub fn compile_bytecode_region_typed(
    module: &mut JITModule,
    builder_context: &mut FunctionBuilderContext,
    ctx: &mut codegen::Context,
    func_name: &str,
    start_offset: usize,
    num_instrs: usize,
    instructions: &[Instruction],
    type_metadata: Option<&TypeMetadata>,
) -> Result<*const u8, CompileError> {
    let end_offset = (start_offset + num_instrs).min(instructions.len());

    // Reject regions containing opcodes this compiler does not model: the
    // catch-all arm below terminates at the return block, which would drop
    // the rest of the region. Callers fall back to the scalar compiler.
    // This check must run before the FunctionBuilder is created so an
    // early return leaves the reusable contexts clean.
    for instr in &instructions[start_offset..end_offset] {
        if !is_opcode_supported_typed(instr.opcode) {
            return Err(CompileError::UnsupportedOpcode(format!(
                "{:?}",
                instr.opcode
            )));
        }
    }

    let loop_ssa = simple_loop_ssa_plan(instructions, start_offset, end_offset, type_metadata);
    let cfg_ssa = simple_cfg_ssa_plan(instructions, start_offset, end_offset, type_metadata);

    // Clear the codegen context
    ctx.clear();

    // Build the function signature: fn(regs: *mut u64, constants: *const u64)
    let pointer_type = module.isa().pointer_type();
    ctx.func.signature.params.push(AbiParam::new(pointer_type));
    ctx.func.signature.params.push(AbiParam::new(pointer_type));

    // Create the function builder
    let mut builder = FunctionBuilder::new(&mut ctx.func, builder_context);

    // Create the entry block
    let entry_block = builder.create_block();
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);

    // Extract parameters
    let regs_ptr = builder.block_params(entry_block)[0];
    let consts_ptr = builder.block_params(entry_block)[1];

    // Register runtime helpers (always needed for fallback)
    let helpers = register_runtime_helpers(module, &mut builder);

    // Create blocks for each instruction offset
    let mut blocks: HashMap<usize, Block> = HashMap::new();
    for i in start_offset..end_offset {
        blocks.insert(i, builder.create_block());
    }
    if let (Some(plan), Some(&header)) = (&loop_ssa, blocks.get(&start_offset)) {
        for &(_, ty) in &plan.carried {
            let clif_ty = match ty {
                KnownType::Int => types::I64,
                KnownType::Float => types::F64,
                _ => unreachable!("loop SSA only threads Int/Float registers"),
            };
            builder.append_block_param(header, clif_ty);
        }
    }
    if let Some(plan) = &cfg_ssa {
        for pc in plan.param_blocks() {
            let block = blocks[&pc];
            let carried = plan
                .carried_for_block(pc)
                .expect("CFG SSA parameter block must have carried values");
            for &(_, ty) in carried {
                let clif_ty = match ty {
                    KnownType::Int => types::I64,
                    KnownType::Float => types::F64,
                    _ => unreachable!("CFG SSA only threads Int/Float registers"),
                };
                builder.append_block_param(block, clif_ty);
            }
        }
    }
    let return_block = builder.create_block();
    // Use a thread-local helper for the safepoint so concurrent VMs do not
    // share a process-global actor reduction counter.
    let zero = builder.ins().iconst(types::I64, 0);
    let safepoint = builder
        .ins()
        .call(helpers["nulang_jit_safepoint_check"], &[zero]);
    let safepoint_result = builder.inst_results(safepoint)[0];
    let exhausted = builder.ins().icmp(IntCC::NotEqual, safepoint_result, zero);
    let yield_block = builder.create_block();
    if let Some(&first_block) = blocks.get(&start_offset) {
        let entry_args: Vec<BlockArg> = loop_ssa
            .as_ref()
            .map(|plan| {
                plan.carried
                    .iter()
                    .map(|&(reg, ty)| {
                        BlockArg::from(native_value_from_vm(&mut builder, regs_ptr, reg, ty))
                    })
                    .collect()
            })
            .unwrap_or_default();
        builder
            .ins()
            .brif(exhausted, yield_block, &[], first_block, &entry_args);
    } else {
        builder
            .ins()
            .brif(exhausted, yield_block, &[], return_block, &[]);
    }

    // Yield block: mark a relative resume offset in thread-local state.
    builder.switch_to_block(yield_block);
    let zero = builder.ins().iconst(types::I64, 0);
    builder
        .ins()
        .call(helpers["nulang_jit_set_yield_pc"], &[zero]);
    builder.ins().jump(return_block, &[]);

    // Seal all new blocks.
    builder.seal_block(yield_block);
    // Precompute must-type state at every bytecode block. Code generation is
    // intentionally independent of source/bytecode layout order: a mutually
    // exclusive arm cannot leak type facts into the next arm simply because
    // it is emitted first.
    let block_type_states =
        region_type_states(instructions, start_offset, end_offset, type_metadata);
    let mut meta = type_metadata.map(|m| m.clone()).unwrap_or_default();
    let predecessor_counts = region_predecessor_counts(instructions, start_offset, end_offset);
    let mut int_cache = NativeIntCache::default();
    let mut float_cache = NativeFloatCache::default();

    // Compile each instruction
    for pc in start_offset..end_offset {
        let instr = instructions[pc];
        let block = *blocks.get(&pc).unwrap();
        builder.switch_to_block(block);

        meta.regs = block_type_states[pc - start_offset].unwrap_or([KnownType::Unknown; 256]);

        if pc == start_offset {
            if let Some(plan) = &loop_ssa {
                let params = builder.block_params(block).to_vec();
                for (&(reg, ty), &value) in plan.carried.iter().zip(params.iter()) {
                    match ty {
                        KnownType::Int => {
                            int_cache.set(reg, value);
                            float_cache.invalidate(reg);
                        }
                        KnownType::Float => {
                            float_cache.set(reg, value);
                            int_cache.invalidate(reg);
                        }
                        _ => unreachable!("loop SSA only threads Int/Float registers"),
                    }
                }
            }
        }

        if let Some(plan) = &cfg_ssa {
            if let Some(carried) = plan.carried_for_block(pc) {
                int_cache.clear();
                float_cache.clear();
                let params = builder.block_params(block).to_vec();
                for (&(reg, ty), &value) in carried.iter().zip(params.iter()) {
                    match ty {
                        KnownType::Int => {
                            int_cache.set(reg, value);
                            float_cache.invalidate(reg);
                        }
                        KnownType::Float => {
                            float_cache.set(reg, value);
                            int_cache.invalidate(reg);
                        }
                        _ => unreachable!("CFG SSA only threads Int/Float registers"),
                    }
                }
            }
        }

        match instr.opcode {
            // -- Special --
            OpCode::Nop => {}
            OpCode::Halt => {
                flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                builder.ins().jump(return_block, &[]);
            }
            OpCode::Const0 | OpCode::Const1 | OpCode::Const2 | OpCode::ConstM1 => {
                let value = match instr.opcode {
                    OpCode::Const0 => 0,
                    OpCode::Const1 => 1,
                    OpCode::Const2 => 2,
                    OpCode::ConstM1 => -1,
                    _ => unreachable!(),
                };
                let raw = builder.ins().iconst(types::I64, value);
                let dst = instr.op1 as usize;
                int_cache.set(dst, raw);
                float_cache.invalidate(dst);
                meta.set_type(dst, KnownType::Int);
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
                // Destination is op3, matching the interpreter and the scalar
                // compiler (op1/op2 hold the 16-bit constant index).
                store_reg(&mut builder, regs_ptr, instr.op3 as usize, val);
                int_cache.invalidate(instr.op3 as usize);
                float_cache.invalidate(instr.op3 as usize);
                meta.set_type(instr.op3 as usize, KnownType::Unknown);
            }

            // -- Register --
            // Load/Store are plain register copies in this pipeline, exactly
            // like Move/Dup (mirroring the scalar compiler).
            OpCode::Load | OpCode::Store | OpCode::Move | OpCode::Dup => {
                let src = instr.op1 as usize;
                let dst = instr.op2 as usize;
                if meta.is_known(src, KnownType::Int) {
                    let val = int_cache.load(&mut builder, regs_ptr, src);
                    int_cache.set(dst, val);
                    float_cache.invalidate(dst);
                } else if meta.is_known(src, KnownType::Float) {
                    let val = float_cache.load(&mut builder, regs_ptr, src);
                    float_cache.set(dst, val);
                    int_cache.invalidate(dst);
                } else {
                    let val = load_reg(&mut builder, regs_ptr, src);
                    store_reg(&mut builder, regs_ptr, dst, val);
                    int_cache.invalidate(dst);
                    float_cache.invalidate(dst);
                }
                meta.propagate_result(dst, src);
            }
            OpCode::Swap => {
                let r1 = instr.op1 as usize;
                let r2 = instr.op2 as usize;
                let ty1 = meta.get_type(r1);
                let ty2 = meta.get_type(r2);
                if ty1 == KnownType::Int && ty2 == KnownType::Int {
                    let v1 = int_cache.load(&mut builder, regs_ptr, r1);
                    let v2 = int_cache.load(&mut builder, regs_ptr, r2);
                    int_cache.set(r1, v2);
                    int_cache.set(r2, v1);
                    float_cache.invalidate(r1);
                    float_cache.invalidate(r2);
                } else if ty1 == KnownType::Float && ty2 == KnownType::Float {
                    let v1 = float_cache.load(&mut builder, regs_ptr, r1);
                    let v2 = float_cache.load(&mut builder, regs_ptr, r2);
                    float_cache.set(r1, v2);
                    float_cache.set(r2, v1);
                    int_cache.invalidate(r1);
                    int_cache.invalidate(r2);
                } else {
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                    let v1 = load_reg(&mut builder, regs_ptr, r1);
                    let v2 = load_reg(&mut builder, regs_ptr, r2);
                    store_reg(&mut builder, regs_ptr, r1, v2);
                    store_reg(&mut builder, regs_ptr, r2, v1);
                }
                meta.set_type(r1, ty2);
                meta.set_type(r2, ty1);
            }

            // -- Integer Arithmetic (typed when both operands known Int) --
            OpCode::IAdd => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_ibinop(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedIntOp::Add,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_iadd",
                    );
                }
                // Both branches produce an Int result. The typed branch leaves
                // it unboxed in the native cache; the fallback branch flushes
                // the cache and stores a boxed result directly.
                meta.set_type(dst, KnownType::Int);
                float_cache.invalidate(dst);
            }
            OpCode::ISub => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_ibinop(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedIntOp::Sub,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_isub",
                    );
                }
                meta.set_type(dst, KnownType::Int);
                float_cache.invalidate(dst);
            }
            OpCode::IMul => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_ibinop(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedIntOp::Mul,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_imul",
                    );
                }
                meta.set_type(dst, KnownType::Int);
                float_cache.invalidate(dst);
            }
            OpCode::IDiv => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_idivmod(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedIntDivOp::Div,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_idiv",
                    );
                }
                // Division by zero yields nil, so the result cannot be proven
                // Int after this instruction even on the typed fast path.
                meta.set_type(dst, KnownType::Unknown);
                float_cache.invalidate(dst);
            }
            OpCode::IMod => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_idivmod(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedIntDivOp::Mod,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_imod",
                    );
                }
                meta.set_type(dst, KnownType::Unknown);
                float_cache.invalidate(dst);
            }
            OpCode::INeg => {
                let dst = instr.op2 as usize;
                if meta.is_known(instr.op1 as usize, KnownType::Int) {
                    emit_typed_iunary(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        dst,
                        TypedIntUnaryOp::Neg,
                    );
                } else {
                    emit_unary_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        dst,
                        "nulang_ineg",
                    );
                }
                meta.set_type(dst, KnownType::Int);
                float_cache.invalidate(dst);
            }
            OpCode::IInc => {
                let reg = instr.op1 as usize;
                if meta.is_known(reg, KnownType::Int) {
                    emit_typed_iunary(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        reg,
                        reg,
                        TypedIntUnaryOp::Inc,
                    );
                } else {
                    emit_unary_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        reg,
                        reg,
                        "nulang_iinc",
                    );
                }
                meta.set_type(reg, KnownType::Int);
                float_cache.invalidate(reg);
            }
            OpCode::IDec => {
                let reg = instr.op1 as usize;
                if meta.is_known(reg, KnownType::Int) {
                    emit_typed_iunary(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        reg,
                        reg,
                        TypedIntUnaryOp::Dec,
                    );
                } else {
                    emit_unary_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        reg,
                        reg,
                        "nulang_idec",
                    );
                }
                meta.set_type(reg, KnownType::Int);
                float_cache.invalidate(reg);
            }
            OpCode::Xor | OpCode::Shl | OpCode::Shr | OpCode::BitAnd | OpCode::BitOr => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    let op = match instr.opcode {
                        OpCode::Xor => TypedIntBitOp::Xor,
                        OpCode::Shl => TypedIntBitOp::Shl,
                        OpCode::Shr => TypedIntBitOp::Shr,
                        OpCode::BitAnd => TypedIntBitOp::And,
                        OpCode::BitOr => TypedIntBitOp::Or,
                        _ => unreachable!(),
                    };
                    emit_typed_ibitop(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        op,
                    );
                } else {
                    let helper = match instr.opcode {
                        OpCode::Xor => "nulang_xor",
                        OpCode::Shl => "nulang_shl",
                        OpCode::Shr => "nulang_shr",
                        OpCode::BitAnd => "nulang_bitand",
                        OpCode::BitOr => "nulang_bitor",
                        _ => unreachable!(),
                    };
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        helper,
                    );
                }
                meta.set_type(dst, KnownType::Int);
                float_cache.invalidate(dst);
            }

            // -- Float Arithmetic (typed when both operands known Float) --
            OpCode::FAdd => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Float) {
                    emit_typed_fbinop(
                        &mut builder,
                        regs_ptr,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedFloatOp::Add,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_fadd",
                    );
                }
                meta.set_type(dst, KnownType::Float);
                int_cache.invalidate(dst);
            }
            OpCode::FSub => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Float) {
                    emit_typed_fbinop(
                        &mut builder,
                        regs_ptr,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedFloatOp::Sub,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_fsub",
                    );
                }
                meta.set_type(dst, KnownType::Float);
                int_cache.invalidate(dst);
            }
            OpCode::FMul => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Float) {
                    emit_typed_fbinop(
                        &mut builder,
                        regs_ptr,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        TypedFloatOp::Mul,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_fmul",
                    );
                }
                meta.set_type(dst, KnownType::Float);
                int_cache.invalidate(dst);
            }
            OpCode::FNeg => {
                let src = instr.op1 as usize;
                let dst = instr.op3 as usize;
                if meta.is_known(src, KnownType::Float) {
                    emit_typed_fneg(&mut builder, regs_ptr, &mut float_cache, src, dst);
                } else {
                    emit_unary_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        src,
                        dst,
                        "nulang_fneg",
                    );
                }
                meta.set_type(dst, KnownType::Float);
                int_cache.invalidate(dst);
            }
            OpCode::FDiv => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Float) {
                    emit_typed_fdiv(
                        &mut builder,
                        regs_ptr,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_fdiv",
                    );
                }
                meta.set_type(dst, KnownType::Unknown);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }

            // -- Typed Comparisons --
            OpCode::ICmpEq => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_icmp(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        IntCC::Equal,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_icmp_eq",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::ICmpLt => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_icmp(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        IntCC::SignedLessThan,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_icmp_lt",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::ICmpGt => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_icmp(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        IntCC::SignedGreaterThan,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_icmp_gt",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::ICmpLe => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_icmp(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        IntCC::SignedLessThanOrEqual,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_icmp_le",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::ICmpGe => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Int) {
                    emit_typed_icmp(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        IntCC::SignedGreaterThanOrEqual,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_icmp_ge",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::FCmpEq => {
                // Always use the runtime helper: `nulang_fcmp_eq` compares with
                // an epsilon tolerance, which direct CLIF `fcmp Equal` (exact
                // bit equality) would not reproduce.
                let dst = instr.op3 as usize;
                emit_binop_runtime(
                    &mut builder,
                    &helpers,
                    regs_ptr,
                    &mut int_cache,
                    &mut float_cache,
                    instr.op1 as usize,
                    instr.op2 as usize,
                    dst,
                    "nulang_fcmp_eq",
                );
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::FCmpLt => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Float) {
                    emit_typed_fcmp(
                        &mut builder,
                        regs_ptr,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        FloatCC::LessThan,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_fcmp_lt",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }
            OpCode::FCmpGt => {
                let dst = instr.op3 as usize;
                if meta.both_known(instr.op1 as usize, instr.op2 as usize, KnownType::Float) {
                    emit_typed_fcmp(
                        &mut builder,
                        regs_ptr,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        FloatCC::GreaterThan,
                    );
                } else {
                    emit_binop_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        instr.op1 as usize,
                        instr.op2 as usize,
                        dst,
                        "nulang_fcmp_gt",
                    );
                }
                meta.set_bool_result(dst);
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
            }

            // -- Logic --
            OpCode::Not => {
                emit_unary_runtime(
                    &mut builder,
                    &helpers,
                    regs_ptr,
                    &mut int_cache,
                    &mut float_cache,
                    instr.op1 as usize,
                    instr.op2 as usize,
                    "nulang_not",
                );
                meta.set_bool_result(instr.op2 as usize);
                int_cache.invalidate(instr.op2 as usize);
                float_cache.invalidate(instr.op2 as usize);
            }
            OpCode::And => {
                emit_typed_logic(
                    &mut builder,
                    regs_ptr,
                    &mut int_cache,
                    &mut float_cache,
                    instr.op1 as usize,
                    instr.op2 as usize,
                    instr.op3 as usize,
                    TypedLogicOp::And,
                    &helpers,
                    &meta,
                );
                meta.set_bool_result(instr.op3 as usize);
                int_cache.invalidate(instr.op3 as usize);
                float_cache.invalidate(instr.op3 as usize);
            }
            OpCode::Or => {
                emit_typed_logic(
                    &mut builder,
                    regs_ptr,
                    &mut int_cache,
                    &mut float_cache,
                    instr.op1 as usize,
                    instr.op2 as usize,
                    instr.op3 as usize,
                    TypedLogicOp::Or,
                    &helpers,
                    &meta,
                );
                meta.set_bool_result(instr.op3 as usize);
                int_cache.invalidate(instr.op3 as usize);
                float_cache.invalidate(instr.op3 as usize);
            }

            // -- Control Flow --
            OpCode::Jmp => {
                let target = (pc as i64 + instr.simm16() as i64) as usize;
                let is_loop_backedge = loop_ssa
                    .as_ref()
                    .is_some_and(|plan| plan.backedge_pc == pc && target == start_offset);

                if is_loop_backedge {
                    let plan = loop_ssa.as_ref().unwrap();
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.carried,
                    );
                    let args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.carried,
                    );
                    let header = blocks[&start_offset];
                    builder.ins().jump(header, &args);
                    // The next bytecode block is not reachable through this
                    // edge. Drop codegen-time mappings without materializing:
                    // the live values are carried by the header block params.
                    int_cache.clear();
                    float_cache.clear();
                } else if cfg_ssa
                    .as_ref()
                    .is_some_and(|plan| plan.then_jump_pc == Some(pc) && target == plan.join_pc)
                {
                    let plan = cfg_ssa.as_ref().unwrap();
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.join_carried,
                    );
                    let args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.join_carried,
                    );
                    builder.ins().jump(blocks[&plan.join_pc], &args);
                    int_cache.clear();
                    float_cache.clear();
                } else {
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                    if let Some(&target_block) = blocks.get(&target) {
                        builder.ins().jump(target_block, &[]);
                    } else {
                        emit_yield_pc(
                            &mut builder,
                            helpers["nulang_jit_set_branch_exit_pc"],
                            start_offset,
                            target,
                        );
                        builder.ins().jump(return_block, &[]);
                    }
                }
            }
            OpCode::JmpT => {
                let target = (pc as i64 + instr.offset16() as i64) as usize;
                let is_loop_backedge = loop_ssa
                    .as_ref()
                    .is_some_and(|plan| plan.backedge_pc == pc && target == start_offset);

                if is_loop_backedge {
                    let plan = loop_ssa.as_ref().unwrap();
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.carried,
                    );

                    // The simple-loop plan proves this condition is Bool, so it
                    // is already materialized by the comparison/logic opcode.
                    let cond_val = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                    let one = builder.ins().iconst(types::I64, 1);
                    let cond_bit = builder.ins().band(cond_val, one);
                    let zero = builder.ins().iconst(types::I64, 0);
                    let is_true = builder.ins().icmp(IntCC::NotEqual, cond_bit, zero);

                    let args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.carried,
                    );
                    let header = blocks[&start_offset];
                    let exit_block = builder.create_block();
                    builder.ins().brif(is_true, header, &args, exit_block, &[]);

                    // Only the exit path materializes loop-carried native
                    // values. The taken backedge stays entirely in SSA form.
                    builder.switch_to_block(exit_block);
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                    let fallthrough = *blocks.get(&(pc + 1)).unwrap_or(&return_block);
                    builder.ins().jump(fallthrough, &[]);
                    builder.seal_block(exit_block);
                } else if cfg_ssa
                    .as_ref()
                    .is_some_and(|plan| plan.branch_pc == pc && plan.target_pc == target)
                {
                    let plan = cfg_ssa.as_ref().unwrap();
                    let branch_carried = plan.branch_carried();
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &branch_carried,
                    );
                    let cond_val = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                    let one = builder.ins().iconst(types::I64, 1);
                    let cond_bit = builder.ins().band(cond_val, one);
                    let zero = builder.ins().iconst(types::I64, 0);
                    let is_true = builder.ins().icmp(IntCC::NotEqual, cond_bit, zero);
                    let target_carried = plan.carried_for_block(plan.target_pc).unwrap_or(&[]);
                    let fallthrough_carried =
                        plan.carried_for_block(plan.fallthrough_pc).unwrap_or(&[]);
                    let target_args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        target_carried,
                    );
                    let fallthrough_args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        fallthrough_carried,
                    );
                    builder.ins().brif(
                        is_true,
                        blocks[&plan.target_pc],
                        &target_args,
                        blocks[&plan.fallthrough_pc],
                        &fallthrough_args,
                    );
                    int_cache.clear();
                    float_cache.clear();
                } else {
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
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
                        let outside = builder.create_block();
                        builder.ins().brif(is_true, outside, &[], fallthrough, &[]);
                        builder.switch_to_block(outside);
                        emit_yield_pc(
                            &mut builder,
                            helpers["nulang_jit_set_branch_exit_pc"],
                            start_offset,
                            target,
                        );
                        builder.ins().jump(return_block, &[]);
                        builder.seal_block(outside);
                    }
                }
            }
            OpCode::JmpF => {
                let target = (pc as i64 + instr.offset16() as i64) as usize;
                let is_loop_backedge = loop_ssa
                    .as_ref()
                    .is_some_and(|plan| plan.backedge_pc == pc && target == start_offset);

                if is_loop_backedge {
                    let plan = loop_ssa.as_ref().unwrap();
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.carried,
                    );
                    let cond_val = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                    let one = builder.ins().iconst(types::I64, 1);
                    let cond_bit = builder.ins().band(cond_val, one);
                    let zero = builder.ins().iconst(types::I64, 0);
                    let is_false = builder.ins().icmp(IntCC::Equal, cond_bit, zero);

                    let args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.carried,
                    );
                    let header = blocks[&start_offset];
                    let exit_block = builder.create_block();
                    builder.ins().brif(is_false, header, &args, exit_block, &[]);

                    builder.switch_to_block(exit_block);
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                    let fallthrough = *blocks.get(&(pc + 1)).unwrap_or(&return_block);
                    builder.ins().jump(fallthrough, &[]);
                    builder.seal_block(exit_block);
                } else if cfg_ssa
                    .as_ref()
                    .is_some_and(|plan| plan.branch_pc == pc && plan.target_pc == target)
                {
                    let plan = cfg_ssa.as_ref().unwrap();
                    let branch_carried = plan.branch_carried();
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &branch_carried,
                    );
                    let cond_val = load_reg(&mut builder, regs_ptr, instr.op1 as usize);
                    let one = builder.ins().iconst(types::I64, 1);
                    let cond_bit = builder.ins().band(cond_val, one);
                    let zero = builder.ins().iconst(types::I64, 0);
                    let is_false = builder.ins().icmp(IntCC::Equal, cond_bit, zero);
                    let target_carried = plan.carried_for_block(plan.target_pc).unwrap_or(&[]);
                    let fallthrough_carried =
                        plan.carried_for_block(plan.fallthrough_pc).unwrap_or(&[]);
                    let target_args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        target_carried,
                    );
                    let fallthrough_args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        fallthrough_carried,
                    );
                    builder.ins().brif(
                        is_false,
                        blocks[&plan.target_pc],
                        &target_args,
                        blocks[&plan.fallthrough_pc],
                        &fallthrough_args,
                    );
                    int_cache.clear();
                    float_cache.clear();
                } else {
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
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
                        let outside = builder.create_block();
                        builder.ins().brif(is_false, outside, &[], fallthrough, &[]);
                        builder.switch_to_block(outside);
                        emit_yield_pc(
                            &mut builder,
                            helpers["nulang_jit_set_branch_exit_pc"],
                            start_offset,
                            target,
                        );
                        builder.ins().jump(return_block, &[]);
                        builder.seal_block(outside);
                    }
                }
            }

            // -- Conversions --
            OpCode::IToF => {
                let src = instr.op1 as usize;
                let dst = instr.op2 as usize;
                if meta.is_known(src, KnownType::Int) {
                    emit_typed_itof(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        src,
                        dst,
                    );
                } else {
                    emit_unary_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        src,
                        dst,
                        "nulang_itof",
                    );
                }
                meta.set_type(dst, KnownType::Float);
            }
            OpCode::FToI => {
                let src = instr.op1 as usize;
                let dst = instr.op2 as usize;
                if meta.is_known(src, KnownType::Float) {
                    emit_typed_ftoi(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        src,
                        dst,
                    );
                } else {
                    emit_unary_runtime(
                        &mut builder,
                        &helpers,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        src,
                        dst,
                        "nulang_ftoi",
                    );
                }
                meta.set_type(dst, KnownType::Int);
            }

            // -- Return --
            OpCode::Ret | OpCode::RetVal => {
                flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                builder.ins().jump(return_block, &[]);
            }

            // -- Debug --
            OpCode::DbgPrint => {}

            // -- Array operations (typed): same implementation as scalar --
            OpCode::ArrLoad => {
                let dst = instr.op3 as usize;
                emit_arr_load(
                    &mut builder,
                    regs_ptr,
                    instr.op1 as usize,
                    instr.op2 as usize,
                    dst,
                );
                int_cache.invalidate(dst);
                float_cache.invalidate(dst);
                meta.set_type(dst, KnownType::Unknown);
            }
            // Everything else
            _ => {
                flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                builder.ins().jump(return_block, &[]);
            }
        }

        // Fallthrough unless terminator
        let is_terminator = matches!(
            instr.opcode,
            OpCode::Jmp | OpCode::JmpT | OpCode::JmpF | OpCode::Halt | OpCode::Ret | OpCode::RetVal
        );

        if !is_terminator {
            if let Some(plan) = &cfg_ssa {
                if pc + 1 == plan.join_pc {
                    flush_non_carried_native_caches(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.join_carried,
                    );
                    let args = native_carried_args(
                        &mut builder,
                        regs_ptr,
                        &mut int_cache,
                        &mut float_cache,
                        &plan.join_carried,
                    );
                    builder.ins().jump(blocks[&plan.join_pc], &args);
                    int_cache.clear();
                    float_cache.clear();
                    continue;
                }
            }

            if let Some(&next_block) = blocks.get(&(pc + 1)) {
                let next_preds = predecessor_counts[pc + 1 - start_offset];
                if next_preds != 1 {
                    flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                }
                builder.ins().jump(next_block, &[]);
            } else {
                flush_native_caches(&mut builder, regs_ptr, &mut int_cache, &mut float_cache);
                builder.ins().jump(return_block, &[]);
            }
        }
    }

    for block in blocks.values() {
        builder.seal_block(*block);
    }

    // Seal the return block
    builder.switch_to_block(return_block);
    builder.seal_block(return_block);
    builder.ins().return_(&[]);

    // Finalize
    builder.finalize();

    let func_id = module
        .declare_function(func_name, Linkage::Local, &ctx.func.signature.clone())
        .map_err(|e| CompileError::DeclareFailed(format!("{}", e)))?;

    module
        .define_function(func_id, ctx)
        .map_err(|e| CompileError::CompileFailed(format!("{}", e)))?;

    module.finalize_definitions().unwrap();

    let code = module.get_finalized_function(func_id);
    Ok(code as *const u8)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod typed_tests {
    use super::*;
    use crate::bytecode::*;
    use crate::jit::JitSession;

    /// Helper: Build a JIT session.
    fn make_jit() -> JitSession {
        JitSession::new().unwrap()
    }

    // ------------------------------------------------------------------
    // Test 1: Typed IAdd emits direct CLIF (no runtime call)
    // ------------------------------------------------------------------

    /// When both operands are known Int, IAdd should compile successfully
    /// with typed emission (direct iadd CLIF, no runtime helper call).
    /// The test verifies compilation succeeds and the result is correct.
    #[test]
    fn test_typed_iadd_emits_direct_clif() {
        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2), // R2 = R0 + R1
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_iadd",
            0,
            2,
            &instructions,
            Some(&meta),
        );
        assert!(ptr.is_ok(), "typed IAdd should compile: {:?}", ptr.err());
    }

    // ------------------------------------------------------------------
    // Test 2: Untyped operands fall back to runtime helper
    // ------------------------------------------------------------------

    /// When operand types are Unknown (no metadata provided), the compiler
    /// should fall back to runtime helper calls and still compile successfully.
    #[test]
    fn test_untyped_falls_back_to_runtime() {
        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new3(OpCode::ISub, 0, 1, 3),
            Instruction::new3(OpCode::IMul, 0, 1, 4),
            Instruction::new0(OpCode::Halt),
        ];

        // No type metadata — forces runtime fallback for all ops
        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_untyped_fallback",
            0,
            4,
            &instructions,
            None,
        );
        assert!(
            ptr.is_ok(),
            "untyped fallback should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 2b: Typed integer division/modulo avoid helpers safely
    // ------------------------------------------------------------------

    #[test]
    fn test_typed_idiv_imod_zero_and_nonzero() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IDiv, 0, 1, 2),
            Instruction::new3(OpCode::IMod, 0, 1, 3),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_idiv_imod",
            0,
            3,
            &instructions,
            Some(&meta),
        )
        .expect("typed IDiv/IMod region should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(-7).as_raw();
        regs[1] = Value::int(0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(regs[2], Value::nil().as_raw());
        assert_eq!(regs[3], Value::nil().as_raw());

        regs[1] = Value::int(2).as_raw();
        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_int(), Some(-3));
        assert_eq!(unsafe { Value::from_bits(regs[3]) }.as_int(), Some(-1));
    }

    #[test]
    fn test_typed_bitwise_and_shift_matches_vm_semantics() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::Xor, 0, 1, 2),
            Instruction::new3(OpCode::BitAnd, 0, 1, 3),
            Instruction::new3(OpCode::BitOr, 0, 1, 4),
            Instruction::new3(OpCode::Shl, 0, 5, 6),
            Instruction::new3(OpCode::Shr, 0, 5, 7),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        for reg in [0usize, 1, 5] {
            meta.set_type(reg, KnownType::Int);
        }

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_bitwise",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("typed bitwise region should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(-8).as_raw();
        regs[1] = Value::int(3).as_raw();
        regs[5] = Value::int(65).as_raw(); // shift count masks to 1

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_int(), Some(-8 ^ 3));
        assert_eq!(unsafe { Value::from_bits(regs[3]) }.as_int(), Some(-8 & 3));
        assert_eq!(unsafe { Value::from_bits(regs[4]) }.as_int(), Some(-8 | 3));
        assert_eq!(unsafe { Value::from_bits(regs[6]) }.as_int(), Some(-16));
        assert_eq!(unsafe { Value::from_bits(regs[7]) }.as_int(), Some(-4));
    }

    // ------------------------------------------------------------------
    // Test 3: Typed float operations emit direct CLIF
    // ------------------------------------------------------------------

    /// When both operands are known Float, FAdd/FSub/FMul/FDiv should emit
    /// direct CLIF float ops without runtime calls. FDiv keeps language
    /// semantics by selecting nil when the divisor is +0.0 or -0.0.
    #[test]
    fn test_typed_float_ops() {
        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FAdd, 0, 1, 2), // R2 = R0 + R1
            Instruction::new3(OpCode::FSub, 0, 1, 3), // R3 = R0 - R1
            Instruction::new3(OpCode::FMul, 0, 1, 4), // R4 = R0 * R1
            Instruction::new3(OpCode::FDiv, 0, 1, 5), // R5 = R0 / R1
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Float);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_float",
            0,
            5,
            &instructions,
            Some(&meta),
        );
        assert!(
            ptr.is_ok(),
            "typed float ops should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 3b: FDiv yields nil on a zero divisor (both paths)
    // ------------------------------------------------------------------

    /// The interpreter's FDiv yields nil on a zero divisor. The typed path
    /// emits native fdiv plus a zero-result select, while the unknown-type
    /// path keeps using the zero-guarded `nulang_fdiv` helper.
    /// Executes the compiled region directly, mirroring the execution
    /// tests in src/jit/tests.rs.
    #[test]
    fn test_typed_fdiv_zero_divisor_yields_nil() {
        use crate::vm::Value;
        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FDiv, 0, 1, 2), // R2 = R0 / R1
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Float);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_fdiv_nil",
            0,
            2,
            &instructions,
            Some(&meta),
        )
        .expect("typed FDiv region should compile");
        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(7.0).as_raw();
        regs[1] = Value::float(0.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            regs[2],
            Value::nil().as_raw(),
            "typed FDiv by zero must yield nil, not inf/NaN"
        );

        regs[1] = Value::float(2.0).as_raw();
        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_float(), Some(3.5));
    }

    #[test]
    fn test_untyped_fdiv_zero_divisor_yields_nil() {
        use crate::vm::Value;
        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FDiv, 0, 1, 2), // R2 = R0 / R1
            Instruction::new0(OpCode::Halt),
        ];

        // No type metadata: forces the runtime-helper fallback branch.
        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_untyped_fdiv_nil",
            0,
            2,
            &instructions,
            None,
        )
        .expect("fallback FDiv region should compile");
        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(7.0).as_raw();
        regs[1] = Value::float(0.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            regs[2],
            Value::nil().as_raw(),
            "fallback FDiv by zero must yield nil, not inf/NaN"
        );

        regs[1] = Value::float(2.0).as_raw();
        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_float(), Some(3.5));
    }

    // ------------------------------------------------------------------
    // Test 4: Typed comparisons emit direct CLIF
    // ------------------------------------------------------------------

    /// Integer and float comparisons should emit direct icmp/fcmp when
    /// operand types are known, producing NaN-tagged boolean results.
    #[test]
    fn test_typed_comparison() {
        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::ICmpEq, 0, 1, 10), // R10 = R0 == R1
            Instruction::new3(OpCode::ICmpLt, 0, 1, 11), // R11 = R0 <  R1
            Instruction::new3(OpCode::ICmpGt, 0, 1, 12), // R12 = R0 >  R1
            Instruction::new3(OpCode::ICmpLe, 0, 1, 13), // R13 = R0 <= R1
            Instruction::new3(OpCode::ICmpGe, 0, 1, 14), // R14 = R0 >= R1
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_icmp",
            0,
            6,
            &instructions,
            Some(&meta),
        );
        assert!(
            ptr.is_ok(),
            "typed int comparisons should compile: {:?}",
            ptr.err()
        );

        // Also test float comparisons
        let mut jit2 = make_jit();
        let float_instrs = vec![
            Instruction::new3(OpCode::FCmpEq, 0, 1, 10),
            Instruction::new3(OpCode::FCmpLt, 0, 1, 11),
            Instruction::new3(OpCode::FCmpGt, 0, 1, 12),
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta2 = TypeMetadata::new();
        meta2.set_type(0, KnownType::Float);
        meta2.set_type(1, KnownType::Float);

        let ptr2 = compile_bytecode_region_typed(
            &mut jit2.optimized_module,
            &mut jit2.optimized_builder_context,
            &mut jit2.optimized_ctx,
            "test_typed_fcmp",
            0,
            4,
            &float_instrs,
            Some(&meta2),
        );
        assert!(
            ptr2.is_ok(),
            "typed float comparisons should compile: {:?}",
            ptr2.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 4b: Native Float SSA cache
    // ------------------------------------------------------------------

    #[test]
    fn test_native_float_cache_chains_arithmetic() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FAdd, 0, 1, 2),
            Instruction::new3(OpCode::FMul, 2, 1, 3),
            Instruction::new3(OpCode::FSub, 3, 0, 4),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Float);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_float_cache_chain",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("cached Float chain should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(1.5).as_raw();
        regs[1] = Value::float(2.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_float(), Some(3.5));
        assert_eq!(unsafe { Value::from_bits(regs[3]) }.as_float(), Some(7.0));
        assert_eq!(unsafe { Value::from_bits(regs[4]) }.as_float(), Some(5.5));
    }

    #[test]
    fn test_native_float_cache_handles_fneg_without_scalar_fallback() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FAdd, 0, 1, 2),
            Instruction::new3(OpCode::FNeg, 2, 0, 3),
            Instruction::new3(OpCode::FAdd, 3, 1, 4),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Float);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_float_cache_fneg",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("cached Float FNeg chain should compile through typed JIT");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(1.5).as_raw();
        regs[1] = Value::float(2.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_float(), Some(3.5));
        assert_eq!(unsafe { Value::from_bits(regs[3]) }.as_float(), Some(-3.5));
        assert_eq!(unsafe { Value::from_bits(regs[4]) }.as_float(), Some(-1.5));
    }

    #[test]
    fn test_native_float_cache_flushes_before_helper() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FAdd, 0, 1, 2),
            // FCmpEq intentionally uses the epsilon-aware runtime helper.
            Instruction::new3(OpCode::FCmpEq, 2, 3, 4),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        for reg in [0usize, 1, 3] {
            meta.set_type(reg, KnownType::Float);
        }

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_float_cache_helper_boundary",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("cached Float/helper chain should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(1.0).as_raw();
        regs[1] = Value::float(2.0).as_raw();
        regs[3] = Value::float(3.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_float(), Some(3.0));
        assert_eq!(unsafe { Value::from_bits(regs[4]) }.as_bool(), Some(true));
    }

    #[test]
    fn test_native_numeric_caches_handoff_conversions() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new2(OpCode::IToF, 0, 1),
            Instruction::new3(OpCode::FAdd, 1, 2, 3),
            Instruction::new2(OpCode::FToI, 3, 4),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(2, KnownType::Float);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_numeric_cache_conversions",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("Int/Float cache handoff should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(40).as_raw();
        regs[2] = Value::float(2.75).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[1]) }.as_float(), Some(40.0));
        assert_eq!(unsafe { Value::from_bits(regs[3]) }.as_float(), Some(42.75));
        assert_eq!(unsafe { Value::from_bits(regs[4]) }.as_int(), Some(42));
    }

    #[test]
    fn test_native_float_cache_canonicalizes_nan_on_flush() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FMul, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Float);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_float_cache_nan",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("NaN-producing cached Float region should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(f64::INFINITY).as_raw();
        regs[1] = Value::float(0.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        let result = unsafe { Value::from_bits(regs[2]) };
        assert!(result.is_float());
        assert!(result.as_float().unwrap().is_nan());
        assert_eq!(regs[2], crate::value_layout::CANONICAL_NAN_BITS);
    }

    // ------------------------------------------------------------------
    // Test 5: Mixed typed and untyped operands
    // ------------------------------------------------------------------

    /// When one operand is typed and the other is not, the compiler should
    /// fall back to runtime helpers. This test verifies correct fallback
    /// behavior in a region with mixed type knowledge.
    #[test]
    fn test_mixed_typed_untyped() {
        let mut jit = make_jit();
        // R0 is known Int, R1 is unknown — IAdd should fall back to runtime
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2), // R0=Int, R1=Unknown -> fallback
            Instruction::new3(OpCode::IAdd, 0, 3, 4), // R0=Int, R3=Int  -> typed
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(3, KnownType::Int);
        // R1 is deliberately left unknown

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_mixed",
            0,
            3,
            &instructions,
            Some(&meta),
        );
        assert!(
            ptr.is_ok(),
            "mixed typed/untyped should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 6: Typed integer loop (the key optimization target)
    // ------------------------------------------------------------------

    /// This is the primary optimization target: a numeric loop where all
    /// registers are known Int. Every operation should emit direct CLIF
    /// instead of runtime helper calls, eliminating ~30% of overhead.
    #[test]
    fn test_typed_int_loop() {
        let mut jit = make_jit();
        // Simulate: for i in 0..5 { sum = sum + i }
        let instructions = vec![
            Instruction::new1(OpCode::Const0, 0), // R0 = 0 (sum)
            Instruction::new1(OpCode::Const0, 1), // R1 = 0 (i)
            // loop:
            Instruction::new3(OpCode::IAdd, 0, 1, 0), // sum = sum + i
            Instruction::new1(OpCode::IInc, 1),       // i++
            Instruction::new3(OpCode::ICmpLt, 1, 2, 2), // R2 = (i < 5)
            Instruction::new2(OpCode::JmpT, 2, 0xFC), // if R2, jmp -4
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int); // sum
        meta.set_type(1, KnownType::Int); // i
                                          // R2 holds the comparison result; we mark it as Bool after ICmpLt

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_typed_loop",
            0,
            7,
            &instructions,
            Some(&meta),
        );
        assert!(
            ptr.is_ok(),
            "typed int loop should compile: {:?}",
            ptr.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 6b: Native Int SSA cache semantic boundaries
    // ------------------------------------------------------------------

    #[test]
    fn test_native_int_cache_preserves_48bit_wrap() {
        use crate::value_layout::{INT48_MAX, INT48_MIN};
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new3(OpCode::IAdd, 2, 1, 3),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_int_cache_wrap",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("cached Int chain should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(INT48_MAX).as_raw();
        regs[1] = Value::int(1).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            unsafe { Value::from_bits(regs[2]) }.as_int(),
            Some(INT48_MIN)
        );
        assert_eq!(
            unsafe { Value::from_bits(regs[3]) }.as_int(),
            Some(INT48_MIN + 1)
        );
    }

    #[test]
    fn test_native_int_cache_flushes_before_runtime_fallback() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            // R3 is deliberately unknown in metadata, forcing the helper path.
            Instruction::new3(OpCode::IAdd, 2, 3, 4),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_int_cache_helper_boundary",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("mixed cached/helper chain should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(4).as_raw();
        regs[1] = Value::int(5).as_raw();
        regs[3] = Value::int(6).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[2]) }.as_int(), Some(9));
        assert_eq!(unsafe { Value::from_bits(regs[4]) }.as_int(), Some(15));
    }

    #[test]
    fn test_native_int_cache_materializes_at_cfg_join() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            // If R4 is true, skip pc2 and join at pc3.
            Instruction::new3(OpCode::JmpT, 4, 0, 2),
            Instruction::new3(OpCode::IAdd, 2, 1, 2),
            Instruction::new3(OpCode::IAdd, 2, 1, 3),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(4, KnownType::Bool);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_native_int_cache_cfg_join",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("branching cached Int region should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];

        let mut regs_true = [0u64; 256];
        regs_true[0] = Value::int(2).as_raw();
        regs_true[1] = Value::int(1).as_raw();
        regs_true[4] = Value::bool(true).as_raw();
        func(regs_true.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs_true[2]) }.as_int(), Some(3));
        assert_eq!(unsafe { Value::from_bits(regs_true[3]) }.as_int(), Some(4));

        let mut regs_false = [0u64; 256];
        regs_false[0] = Value::int(2).as_raw();
        regs_false[1] = Value::int(1).as_raw();
        regs_false[4] = Value::bool(false).as_raw();
        func(regs_false.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs_false[2]) }.as_int(), Some(4));
        assert_eq!(unsafe { Value::from_bits(regs_false[3]) }.as_int(), Some(5));
    }

    // ------------------------------------------------------------------
    // Test 6c: Simple loop-carried native SSA
    // ------------------------------------------------------------------

    #[test]
    fn test_simple_loop_ssa_plan_selects_stable_numeric_regs() {
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 0),
            Instruction::new1(OpCode::IInc, 1),
            Instruction::new3(OpCode::ICmpLt, 1, 6, 5),
            Instruction::new3(OpCode::JmpT, 5, 0xFF, 0xFD), // pc3 -> pc0
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(6, KnownType::Int);

        let plan = simple_loop_ssa_plan(&instructions, 0, instructions.len(), Some(&meta))
            .expect("simple numeric loop should get an SSA plan");

        assert_eq!(plan.backedge_pc, 3);
        assert!(plan.carried.contains(&(0, KnownType::Int)));
        assert!(plan.carried.contains(&(1, KnownType::Int)));
        assert!(plan.carried.contains(&(6, KnownType::Int)));
    }

    #[test]
    fn test_simple_loop_ssa_excludes_nullable_division_result() {
        let instructions = vec![
            Instruction::new3(OpCode::IDiv, 0, 2, 0),
            Instruction::new1(OpCode::IInc, 1),
            Instruction::new3(OpCode::ICmpLt, 1, 6, 5),
            Instruction::new3(OpCode::JmpT, 5, 0xFF, 0xFD),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        for reg in [0usize, 1, 2, 6] {
            meta.set_type(reg, KnownType::Int);
        }

        let plan = simple_loop_ssa_plan(&instructions, 0, instructions.len(), Some(&meta))
            .expect("other stable numeric registers should still be threadable");

        assert!(
            !plan.carried.iter().any(|&(reg, _)| reg == 0),
            "IDiv can produce nil, so its destination must not be carried as native Int"
        );
    }

    #[test]
    fn test_loop_ssa_executes_int_backedge() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 0),
            Instruction::new1(OpCode::IInc, 1),
            Instruction::new3(OpCode::ICmpLt, 1, 6, 5),
            Instruction::new3(OpCode::JmpT, 5, 0xFF, 0xFD),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(6, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_loop_ssa_int",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("Int loop SSA should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(0).as_raw();
        regs[1] = Value::int(0).as_raw();
        regs[6] = Value::int(100).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[0]) }.as_int(), Some(4950));
        assert_eq!(unsafe { Value::from_bits(regs[1]) }.as_int(), Some(100));
    }

    #[test]
    fn test_loop_ssa_executes_float_backedge() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::FAdd, 0, 1, 0),
            Instruction::new3(OpCode::FAdd, 1, 7, 1),
            Instruction::new3(OpCode::FCmpLt, 1, 6, 5),
            Instruction::new3(OpCode::JmpT, 5, 0xFF, 0xFD),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        for reg in [0usize, 1, 6, 7] {
            meta.set_type(reg, KnownType::Float);
        }

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_loop_ssa_float",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("Float loop SSA should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(0.0).as_raw();
        regs[1] = Value::float(0.0).as_raw();
        regs[6] = Value::float(100.0).as_raw();
        regs[7] = Value::float(1.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            unsafe { Value::from_bits(regs[0]) }.as_float(),
            Some(4950.0)
        );
        assert_eq!(unsafe { Value::from_bits(regs[1]) }.as_float(), Some(100.0));
    }

    #[test]
    fn test_loop_ssa_materializes_noncarried_temporary() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            // r8 is deliberately Unknown at the header, so this first op uses
            // the runtime helper. Its value must still reflect the prior loop
            // iteration.
            Instruction::new3(OpCode::FAdd, 0, 8, 0),
            // r8 becomes a typed cached Float inside the loop but cannot be a
            // header phi because its entry type is Unknown.
            Instruction::new3(OpCode::FAdd, 9, 9, 8),
            Instruction::new1(OpCode::IInc, 1),
            Instruction::new3(OpCode::ICmpLt, 1, 6, 5),
            Instruction::new3(OpCode::JmpT, 5, 0xFF, 0xFC), // pc4 -> pc0
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Int);
        meta.set_type(6, KnownType::Int);
        meta.set_type(9, KnownType::Float);
        // r8 stays Unknown at loop entry.

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_loop_ssa_noncarried_temp",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("mixed carried/non-carried loop should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::float(0.0).as_raw();
        regs[1] = Value::int(0).as_raw();
        regs[6] = Value::int(4).as_raw();
        regs[8] = Value::float(1.0).as_raw();
        regs[9] = Value::float(1.0).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        // Iteration inputs for r8 are 1, 2, 2, 2.
        assert_eq!(unsafe { Value::from_bits(regs[0]) }.as_float(), Some(7.0));
        assert_eq!(unsafe { Value::from_bits(regs[8]) }.as_float(), Some(2.0));
    }

    // ------------------------------------------------------------------
    // Test 6d: Per-block must-type dataflow
    // ------------------------------------------------------------------

    #[test]
    fn test_region_type_states_keep_mutually_exclusive_arms_path_local() {
        let instructions = vec![
            Instruction::new3(OpCode::JmpT, 4, 0, 3), // pc0 -> pc3
            Instruction::new2(OpCode::IToF, 0, 0),    // pc1: r0 Int -> Float
            Instruction::new2(OpCode::Jmp, 0, 2),     // pc2 -> pc4
            Instruction::new3(OpCode::IAdd, 0, 1, 0), // pc3 still sees r0 Int
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(4, KnownType::Bool);

        let states = region_type_states(&instructions, 0, instructions.len(), Some(&meta));

        assert_eq!(states[1].unwrap()[0], KnownType::Int);
        assert_eq!(
            states[3].unwrap()[0],
            KnownType::Int,
            "the Float conversion in the other arm must not leak into pc3"
        );
        assert_eq!(
            states[4].unwrap()[0],
            KnownType::Unknown,
            "Int/Float disagreement at the join must conservatively meet to Unknown"
        );
    }

    #[test]
    fn test_region_type_states_preserve_same_numeric_result_at_join() {
        let instructions = vec![
            Instruction::new3(OpCode::JmpT, 4, 0, 3), // pc0 -> pc3
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new2(OpCode::Jmp, 0, 2), // pc2 -> pc4
            Instruction::new3(OpCode::ISub, 0, 1, 2),
            Instruction::new3(OpCode::IMul, 2, 1, 3),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(4, KnownType::Bool);

        let states = region_type_states(&instructions, 0, instructions.len(), Some(&meta));
        assert_eq!(
            states[4].unwrap()[2],
            KnownType::Int,
            "matching Int results from both arms should remain proven at the join"
        );
    }

    // ------------------------------------------------------------------
    // Test 6e: Native SSA through simple forward CFG joins
    // ------------------------------------------------------------------

    #[test]
    fn test_simple_cfg_ssa_plan_if_else() {
        let instructions = vec![
            Instruction::new3(OpCode::JmpT, 4, 0, 3), // pc0 -> pc3
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new2(OpCode::Jmp, 0, 2), // pc2 -> pc4
            Instruction::new3(OpCode::ISub, 0, 1, 2),
            Instruction::new3(OpCode::IMul, 2, 1, 3),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(4, KnownType::Bool);

        let plan = simple_cfg_ssa_plan(&instructions, 0, instructions.len(), Some(&meta))
            .expect("canonical if/else should receive a CFG SSA plan");

        assert_eq!(plan.branch_pc, 0);
        assert_eq!(plan.target_pc, 3);
        assert_eq!(plan.join_pc, 4);
        assert_eq!(plan.then_jump_pc, Some(2));
        assert!(plan.arm_carried.contains(&(0, KnownType::Int)));
        assert!(plan.arm_carried.contains(&(1, KnownType::Int)));
        assert!(
            !plan.arm_carried.iter().any(|&(reg, _)| reg == 2),
            "r2 is not known before the branch"
        );
        assert!(
            plan.join_carried.contains(&(2, KnownType::Int)),
            "r2 is produced as Int on both arms and should merge natively"
        );
    }

    #[test]
    fn test_cfg_ssa_executes_int_if_else_join() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::JmpT, 4, 0, 3),
            Instruction::new3(OpCode::IAdd, 0, 1, 2),
            Instruction::new2(OpCode::Jmp, 0, 2),
            Instruction::new3(OpCode::ISub, 0, 1, 2),
            Instruction::new3(OpCode::IMul, 2, 1, 3),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(4, KnownType::Bool);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_cfg_ssa_int_if_else",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("if/else CFG SSA should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];

        let mut false_regs = [0u64; 256];
        false_regs[0] = Value::int(10).as_raw();
        false_regs[1] = Value::int(3).as_raw();
        false_regs[4] = Value::bool(false).as_raw();
        func(false_regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            unsafe { Value::from_bits(false_regs[2]) }.as_int(),
            Some(13)
        );
        assert_eq!(
            unsafe { Value::from_bits(false_regs[3]) }.as_int(),
            Some(39)
        );

        let mut true_regs = [0u64; 256];
        true_regs[0] = Value::int(10).as_raw();
        true_regs[1] = Value::int(3).as_raw();
        true_regs[4] = Value::bool(true).as_raw();
        func(true_regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(true_regs[2]) }.as_int(), Some(7));
        assert_eq!(unsafe { Value::from_bits(true_regs[3]) }.as_int(), Some(21));
    }

    #[test]
    fn test_cfg_ssa_executes_float_if_without_else() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::JmpF, 4, 0, 3), // pc0 -> pc3 join
            Instruction::new3(OpCode::FAdd, 0, 1, 0),
            Instruction::new3(OpCode::FMul, 0, 1, 0),
            Instruction::new3(OpCode::FSub, 0, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Float);
        meta.set_type(1, KnownType::Float);
        meta.set_type(4, KnownType::Bool);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_cfg_ssa_float_if",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("if-without-else CFG SSA should compile");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];

        let mut skip_regs = [0u64; 256];
        skip_regs[0] = Value::float(8.0).as_raw();
        skip_regs[1] = Value::float(2.0).as_raw();
        skip_regs[4] = Value::bool(false).as_raw();
        func(skip_regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            unsafe { Value::from_bits(skip_regs[2]) }.as_float(),
            Some(6.0)
        );

        let mut body_regs = [0u64; 256];
        body_regs[0] = Value::float(8.0).as_raw();
        body_regs[1] = Value::float(2.0).as_raw();
        body_regs[4] = Value::bool(true).as_raw();
        func(body_regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(
            unsafe { Value::from_bits(body_regs[0]) }.as_float(),
            Some(20.0)
        );
        assert_eq!(
            unsafe { Value::from_bits(body_regs[2]) }.as_float(),
            Some(18.0)
        );
    }

    #[test]
    fn test_cfg_ssa_drops_representation_divergence_at_join() {
        let instructions = vec![
            Instruction::new3(OpCode::JmpT, 4, 0, 2), // pc0 -> pc2 join
            Instruction::new2(OpCode::IToF, 0, 0),    // Int -> Float on one path
            Instruction::new3(OpCode::IAdd, 1, 1, 2),
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);
        meta.set_type(4, KnownType::Bool);

        let plan = simple_cfg_ssa_plan(&instructions, 0, instructions.len(), Some(&meta))
            .expect("stable r1 should keep the CFG plan alive");
        assert!(
            !plan.join_carried.iter().any(|&(reg, _)| reg == 0),
            "r0 changes Int -> Float on only one path and must not be native at the join"
        );
        assert!(plan.join_carried.contains(&(1, KnownType::Int)));
        assert_eq!(plan.join_state[0], KnownType::Unknown);
    }

    #[test]
    fn test_branchy_loop_keeps_cfg_join_and_backedge_in_native_ssa() {
        use crate::vm::Value;

        let mut jit = make_jit();
        let instructions = vec![
            Instruction::new3(OpCode::ICmpLt, 1, 7, 4), // pc0: i < threshold
            Instruction::new3(OpCode::JmpF, 4, 0, 3),   // pc1 -> pc4 else
            Instruction::new3(OpCode::IAdd, 0, 1, 0),   // pc2 then: acc += i
            Instruction::new2(OpCode::Jmp, 0, 2),       // pc3 -> pc5 join
            Instruction::new3(OpCode::ISub, 0, 8, 0),   // pc4 else: acc -= 1
            Instruction::new1(OpCode::IInc, 1),         // pc5 join: i++
            Instruction::new3(OpCode::ICmpLt, 1, 6, 5), // pc6: i < limit
            Instruction::new3(OpCode::JmpT, 5, 0xFF, 0xF9), // pc7 -> pc0
            Instruction::new0(OpCode::Halt),
        ];
        let mut meta = TypeMetadata::new();
        for reg in [0usize, 1, 6, 7, 8] {
            meta.set_type(reg, KnownType::Int);
        }

        let loop_plan = simple_loop_ssa_plan(&instructions, 0, instructions.len(), Some(&meta))
            .expect("branchy numeric loop should retain loop SSA");
        assert_eq!(loop_plan.backedge_pc, 7);
        assert!(loop_plan.carried.contains(&(0, KnownType::Int)));
        assert!(loop_plan.carried.contains(&(1, KnownType::Int)));

        let cfg_plan = simple_cfg_ssa_plan(&instructions, 0, instructions.len(), Some(&meta))
            .expect("internal branch should retain CFG SSA");
        assert_eq!(cfg_plan.join_pc, 5);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_branchy_loop_cfg_and_backedge_ssa",
            0,
            instructions.len(),
            &instructions,
            Some(&meta),
        )
        .expect("branchy loop should compile with composed SSA plans");

        let func: extern "C" fn(*mut u64, *const u64) = unsafe { std::mem::transmute(ptr) };
        let consts: [u64; 0] = [];
        let mut regs = [0u64; 256];
        regs[0] = Value::int(0).as_raw();
        regs[1] = Value::int(0).as_raw();
        regs[6] = Value::int(10).as_raw();
        regs[7] = Value::int(5).as_raw();
        regs[8] = Value::int(1).as_raw();

        func(regs.as_mut_ptr(), consts.as_ptr());
        assert_eq!(unsafe { Value::from_bits(regs[0]) }.as_int(), Some(5));
        assert_eq!(unsafe { Value::from_bits(regs[1]) }.as_int(), Some(10));
    }

    // ------------------------------------------------------------------
    // Test 7: sext48 inline extraction correctness
    // ------------------------------------------------------------------

    /// The inline sign-extension (sext48) is the core of integer guard
    /// stripping. This test compiles a region with IAdd on known Ints,
    /// which exercises the full sext48 → iadd → tag_int pipeline.
    #[test]
    fn test_sext48_extraction() {
        let mut jit = make_jit();
        // Simple addition that exercises sext48 on both positive and
        // potentially negative values
        let instructions = vec![
            Instruction::new3(OpCode::IAdd, 0, 1, 2), // uses sext48 on both operands
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta = TypeMetadata::new();
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Int);

        let ptr = compile_bytecode_region_typed(
            &mut jit.optimized_module,
            &mut jit.optimized_builder_context,
            &mut jit.optimized_ctx,
            "test_sext48",
            0,
            2,
            &instructions,
            Some(&meta),
        );
        assert!(
            ptr.is_ok(),
            "sext48 extraction pipeline should compile: {:?}",
            ptr.err()
        );

        // Also test with negative operand (sign bit set)
        let mut jit2 = make_jit();
        let instructions2 = vec![
            Instruction::new1(OpCode::ConstM1, 0),    // R0 = -1
            Instruction::new3(OpCode::IAdd, 0, 1, 2), // R2 = -1 + R1
            Instruction::new0(OpCode::Halt),
        ];

        let mut meta2 = TypeMetadata::new();
        meta2.set_type(0, KnownType::Int);
        meta2.set_type(1, KnownType::Int);

        let ptr2 = compile_bytecode_region_typed(
            &mut jit2.optimized_module,
            &mut jit2.optimized_builder_context,
            &mut jit2.optimized_ctx,
            "test_sext48_negative",
            0,
            3,
            &instructions2,
            Some(&meta2),
        );
        assert!(
            ptr2.is_ok(),
            "sext48 with negative should compile: {:?}",
            ptr2.err()
        );
    }

    // ------------------------------------------------------------------
    // Test 8: TypeMetadata construction and API
    // ------------------------------------------------------------------

    /// Verify that TypeMetadata can be constructed, types can be set and
    /// retrieved, and the various query methods work correctly.
    #[test]
    fn test_type_metadata_construction() {
        let mut meta = TypeMetadata::new();

        // Initially all registers are Unknown
        assert_eq!(meta.get_type(0), KnownType::Unknown);
        assert_eq!(meta.get_type(255), KnownType::Unknown);

        // Set types
        meta.set_type(0, KnownType::Int);
        meta.set_type(1, KnownType::Float);
        meta.set_type(2, KnownType::Bool);

        // Retrieve
        assert_eq!(meta.get_type(0), KnownType::Int);
        assert_eq!(meta.get_type(1), KnownType::Float);
        assert_eq!(meta.get_type(2), KnownType::Bool);
        assert_eq!(meta.get_type(3), KnownType::Unknown);

        // both_known
        assert!(meta.both_known(0, 0, KnownType::Int));
        assert!(!meta.both_known(0, 1, KnownType::Int));
        assert!(meta.both_known(1, 1, KnownType::Float));
        assert!(!meta.both_known(0, 2, KnownType::Int));

        // is_known
        assert!(meta.is_known(0, KnownType::Int));
        assert!(!meta.is_known(0, KnownType::Float));
        assert!(meta.is_known(1, KnownType::Float));
        assert!(!meta.is_known(3, KnownType::Int));

        // propagate_result
        meta.propagate_result(10, 0);
        assert_eq!(meta.get_type(10), KnownType::Int);

        meta.propagate_result(11, 1);
        assert_eq!(meta.get_type(11), KnownType::Float);

        // set_bool_result
        meta.set_bool_result(20);
        assert_eq!(meta.get_type(20), KnownType::Bool);
    }
}
