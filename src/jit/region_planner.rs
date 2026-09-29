//! Backend-neutral planning for native compilation.
//!
//! This module owns the language/runtime analysis that decides *what* may be
//! compiled: region boundaries, direct-call safety, recursion safety, and
//! type metadata. Machine-code backends consume a `RegionPlan`; they should
//! not need to rediscover these semantics independently.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

use crate::bytecode::CodeModule;

use super::compiler;
use super::typed_compiler::{self, TypeMetadata};
use super::STRAIGHT_LINE_MIN;

/// A backend-neutral description of one bytecode region that may be lowered
/// to native code.
pub(crate) struct RegionPlan {
    pub(crate) len: usize,
    pub(crate) native_calls: HashMap<usize, usize>,
    pub(crate) type_metadata: TypeMetadata,
}

/// Backend-neutral proof that a function can run as an isolated native leaf.
///
/// Leaf code executes against the caller's physical register buffer, so
/// `clobbers` is the exact set that must be preserved to recreate the
/// interpreter's separate callee frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeLeafPlan {
    pub(crate) start: usize,
    pub(crate) body_len: usize,
    pub(crate) ret_reg: u8,
    pub(crate) clobbers: Vec<u8>,
}

impl RegionPlan {
    #[inline]
    pub(crate) fn type_metadata(&self) -> Option<&TypeMetadata> {
        if self.type_metadata.is_empty() {
            None
        } else {
            Some(&self.type_metadata)
        }
    }
}

/// Caches module-level safety analysis and produces region plans.
///
/// Keeping this separate from `JitSession` makes region semantics reusable
/// by alternative native backends (MIR, a custom emitter, or a future JIT)
/// without coupling them to Cranelift state.
#[derive(Default)]
pub(crate) struct RegionPlanner {
    may_suspend: FxHashMap<usize, Vec<bool>>,
    recursive: FxHashMap<usize, Vec<bool>>,
}

impl RegionPlanner {
    pub(crate) fn plan(&mut self, module_idx: usize, pc: usize, module: &CodeModule) -> RegionPlan {
        if !self.may_suspend.contains_key(&module_idx) {
            self.may_suspend
                .insert(module_idx, compute_may_suspend(module));
        }
        if !self.recursive.contains_key(&module_idx) {
            self.recursive.insert(module_idx, compute_recursive(module));
        }

        let may_suspend = self.may_suspend.get(&module_idx).map(Vec::as_slice);
        let recursive = self.recursive.get(&module_idx).map(Vec::as_slice);
        let (len, native_calls) = find_compilable_region_with_calls(
            pc,
            &module.instructions,
            module,
            may_suspend,
            recursive,
        );

        RegionPlan {
            len,
            native_calls,
            type_metadata: typed_compiler::infer_reg_types(module, pc),
        }
    }
}

// ---------------------------------------------------------------------------
// Tiered Execution
// ---------------------------------------------------------------------------

/// Find a contiguous region of compilable instructions starting at `offset`.
/// Returns the number of instructions in the region.
///
/// Regions normally stop BEFORE the first branch (straight-line only): after
/// a region runs the VM unconditionally advances pc by the region length, so
/// a compiled branch to an outside target would resume at the wrong place.
/// HOWEVER, when the scan encounters a BACKWARD edge (a branch whose target
/// precedes its own pc — the hallmark of a loop), the region is extended to
/// include the branches so the hot loop compiles natively: the loop head,
/// body and back-jump run in one VM entry, and any branch to a target outside
/// the region yields the interpreter there via the branch-exit slot. This
/// makes tight loops far faster (the straight-line body-slice approach cost a
/// 256-register snapshot/restore per iteration). Forward-only branchy code
/// (e.g. an `if`/recursion that leads to a `Call`) keeps the straight-line
/// boundary, which is why recursion does not regress.
/// A direct call's callee value is staged into `FUNC_VALUE_REG` (254) by
/// `mir_codegen` before the `Call` (a `load_constant` then the arg staging).
/// Recover the statically-known callee of a `Call` at `pc` by scanning
/// backward to the nearest prior write to reg 254:
/// - a `ConstU`/`Const0/1/2` into 254 → direct call to that function index;
/// - a `Move` (or anything else) into 254 → an indirect call (closure /
///   runtime value) — the target is not statically known.
/// `func_start` bounds the scan to the calling function's code. This is a
/// hint only: compiled direct calls re-check the live value in reg 254 at
/// run time and fall back to the interpreter on mismatch, so a stale
/// recovery here never calls the wrong function.
///
/// Foundation for JIT-compiling direct calls (the next slice): currently
/// exercised by the `may_suspend` analysis and its test.
#[allow(dead_code)]
pub(crate) fn direct_call_target(
    module: &crate::bytecode::CodeModule,
    pc: usize,
    func_start: usize,
) -> Option<usize> {
    use crate::bytecode::{Constant, OpCode};
    const FUNC_VALUE_REG: u8 = 254;
    const SPILL_TEMP_MIN: u8 = 12;
    const SPILL_TEMP_MAX: u8 = 14;

    let call = *module.instructions.get(pc)?;
    if call.opcode != OpCode::Call {
        return None;
    }
    let argc = call.op2;

    // MIR emits one exact staging shape:
    //   load direct callee -> r254
    //   [optional SpillLoad -> r12..r14]*
    //   [optional Move -> r0..r(argc-1)]*
    //   Call r254, argc, dst
    //
    // Do not scan across unrelated code looking for a stale r254 definition.
    let mut p = pc;
    while p > func_start {
        p -= 1;
        let instr = module.instructions[p];
        match instr.opcode {
            OpCode::Move if instr.op2 < argc => continue,
            OpCode::SpillLoad if (SPILL_TEMP_MIN..=SPILL_TEMP_MAX).contains(&instr.op3) => continue,
            OpCode::Const0 | OpCode::Const1 | OpCode::Const2 if instr.op1 == FUNC_VALUE_REG => {
                let idx = match instr.opcode {
                    OpCode::Const0 => 0,
                    OpCode::Const1 => 1,
                    _ => 2,
                };
                return Some(idx);
            }
            OpCode::ConstM1 if instr.op1 == FUNC_VALUE_REG => return None,
            OpCode::ConstU if instr.op3 == FUNC_VALUE_REG => {
                let pool = instr.imm16() as usize;
                return match module.constants.get(pool) {
                    Some(Constant::Int(i)) if *i >= 0 => Some(*i as usize),
                    _ => None,
                };
            }
            OpCode::Move if instr.op2 == FUNC_VALUE_REG => return None,
            _ => return None,
        }
    }
    None
}

/// Opcodes that can never suspend (a pure function's safe set). A function
/// whose body contains only these, plus direct calls to other safe functions,
/// is non-suspending and safe to call from JIT-compiled code. Everything
/// else — effects (`Perform`/`PerformDirect`/`Handle`/`Resume`/`Unwind`),
/// actor ops, async effects, `SignalWait`/`Receive*`, foreign calls,
/// `SConcat`/record/closure ops — is conservatively treated as suspending.
#[allow(dead_code)]
fn is_non_suspending_op(op: crate::bytecode::OpCode) -> bool {
    use crate::bytecode::OpCode;
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
    )
}

/// Compute the transitive "may suspend" vector for a module (indexed by
/// function-table index). A function may suspend if its body contains a
/// suspending opcode (or any opcode outside the pure whitelist), or an
/// indirect call (unknown target), or a direct call to a may-suspend
/// function. Fixed point over the direct-call graph recovered by
/// `direct_call_target`.
#[allow(dead_code)]
pub(crate) fn compute_may_suspend(module: &crate::bytecode::CodeModule) -> Vec<bool> {
    use crate::bytecode::OpCode;
    let n = module.function_table.len();
    let mut result = vec![false; n];
    // Directly unsafe: contains a non-whitelisted opcode (effect/actor/
    // foreign/suspending) or an indirect call (Call/ClosureCall whose target
    // is not a statically-recovered direct callee).
    for i in 0..n {
        let start = module.function_table[i];
        let end = if i + 1 < n {
            module.function_table[i + 1]
        } else {
            module.instructions.len()
        };
        for pc in start..end {
            let op = module.instructions[pc].opcode;
            if matches!(op, OpCode::Call | OpCode::ClosureCall) {
                if direct_call_target(module, pc, start).is_none() {
                    result[i] = true; // indirect call: unknown target
                }
                // direct call: leave for the fixed-point propagation
            } else if !is_non_suspending_op(op) {
                result[i] = true;
                break;
            }
        }
    }
    // Propagate through the direct-call graph until stable.
    loop {
        let mut changed = false;
        for i in 0..n {
            if result[i] {
                continue;
            }
            let start = module.function_table[i];
            let end = if i + 1 < n {
                module.function_table[i + 1]
            } else {
                module.instructions.len()
            };
            for pc in start..end {
                if matches!(
                    module.instructions[pc].opcode,
                    OpCode::Call | OpCode::ClosureCall
                ) {
                    if let Some(callee) = direct_call_target(module, pc, start) {
                        if callee < n && result[callee] {
                            result[i] = true;
                            changed = true;
                            break;
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    result
}

/// Per function: can it transitively reach itself via direct calls (i.e. is
/// it part of a direct-call recursion cycle)? A recursive function must NOT
/// be run through the re-entrant direct-call helper: each helper invocation
/// consumes native stack (compiled region -> helper -> interpreter step ->
/// nested region -> ...), so unbounded recursion would overflow the stack.
/// The interpreter handles recursion on heap-allocated frames; a recursive
/// callee stays there. Computed via transitive closure over the direct-call
/// graph (n is small — one per function).
pub(crate) fn compute_recursive(module: &crate::bytecode::CodeModule) -> Vec<bool> {
    use crate::bytecode::OpCode;
    let n = module.function_table.len();
    let mut reach = vec![vec![false; n]; n];
    for i in 0..n {
        let start = module.function_table[i];
        let end = if i + 1 < n {
            module.function_table[i + 1]
        } else {
            module.instructions.len()
        };
        for pc in start..end {
            if matches!(
                module.instructions[pc].opcode,
                OpCode::Call | OpCode::ClosureCall
            ) {
                if let Some(callee) = direct_call_target(module, pc, start) {
                    if callee < n {
                        reach[i][callee] = true;
                    }
                }
            }
        }
    }
    // Floyd-Warshall transitive closure.
    for k in 0..n {
        for i in 0..n {
            if reach[i][k] {
                for j in 0..n {
                    if reach[k][j] {
                        reach[i][j] = true;
                    }
                }
            }
        }
    }
    (0..n).map(|i| reach[i][i]).collect()
}

/// Region-length scanner WITHOUT direct-call folding; used by the unit tests.
/// The runtime path uses [`find_compilable_region_with_calls`] so direct
/// non-suspending calls fold into regions.
#[allow(dead_code)]
pub(crate) fn find_compilable_region(
    offset: usize,
    instructions: &[crate::bytecode::Instruction],
) -> usize {
    let mut len = 0;
    let mut first_branch: Option<usize> = None;
    let mut has_back_edge = false;
    let mut max_forward_target = offset;

    for i in offset..instructions.len().min(offset + 500) {
        if !compiler::is_opcode_compilable(instructions[i].opcode) {
            break;
        }
        let op = instructions[i].opcode;

        // Halt is always a hard boundary. Returns are normally boundaries too,
        // except when an earlier in-region branch targets bytecode beyond the
        // return. MIR can lay an early-exit block (Ret/RetVal) lexically before
        // sibling loop blocks; in that shape the return is an internal CFG exit
        // and the scanner must cross it to discover the loop back-edge.
        if op == crate::bytecode::OpCode::Halt {
            break;
        }
        if matches!(
            op,
            crate::bytecode::OpCode::Ret | crate::bytecode::OpCode::RetVal
        ) {
            if max_forward_target <= i {
                break;
            }
            len += 1;
            continue;
        }

        let is_branch = matches!(
            op,
            crate::bytecode::OpCode::Jmp
                | crate::bytecode::OpCode::JmpT
                | crate::bytecode::OpCode::JmpF
        );
        if is_branch {
            if first_branch.is_none() {
                first_branch = Some(len);
            }
            let target = match op {
                crate::bytecode::OpCode::Jmp => {
                    (i as i64 + instructions[i].simm16() as i64) as usize
                }
                _ => (i as i64 + instructions[i].offset16() as i64) as usize,
            };
            if target > i {
                max_forward_target = max_forward_target.max(target);
            }
            // A genuine loop back-edge lands WITHIN the region (target >= offset):
            // the loop head is the region start or an earlier in-region pc, and
            // re-entering it continues the loop. A backward jump to BEFORE the
            // region start (target < offset) is an EXIT (e.g. a return path's
            // jump back to a RetVal), not a loop — don't treat it as a back-edge.
            if target >= offset && target < i {
                has_back_edge = true;
            }
        }
        len += 1;

        // Once a back-edge closes the CFG frontier there is no reason to pull
        // unrelated lexical bytecode (for example the top-level wrapper Call)
        // into the native region. A fallthrough exit resumes at offset + len.
        if has_back_edge && i >= max_forward_target {
            break;
        }
    }

    if has_back_edge {
        // A genuine loop loops INTERNALLY in the compiled code, so the JIT
        // enter/exit + probe cost is amortized across all its iterations —
        // always worth compiling.
        len
    } else {
        let straight = first_branch.unwrap_or(len);
        // A small straight-line region — whether a function body or a loop
        // fragment — is re-entered by the interpreter once per call (a body)
        // or per enclosing-loop iteration (a fragment), so the JIT
        // enter/exit + probe cost is paid EVERY time. That exceeds the cost of
        // just interpreting `straight` instructions below ~STRAIGHT_LINE_MIN,
        // so compiling a small non-looping region is a regression (a
        // call-heavy loop benchmarked ~4.6x SLOWER when its 2-instruction
        // callee was compiled). Genuine loops (internal back-edge) loop
        // natively and amortize the cost, so they are always compiled; only
        // small non-looping regions are rejected.
        if straight < STRAIGHT_LINE_MIN {
            0
        } else {
            straight
        }
    }
}

const NATIVE_LEAF_MAX_INSTRS: usize = 32;

/// Prove that a named function can execute as a native leaf thunk.
///
/// This is intentionally strict: one straight-line body, no nested calls,
/// branches, heap/container operations, effects, or suspension. The returned
/// clobber set is exact for the accepted opcode subset.
pub(crate) fn analyze_native_leaf(
    module: &crate::bytecode::CodeModule,
    func_idx: usize,
) -> Option<NativeLeafPlan> {
    use crate::bytecode::OpCode;

    let start = *module.function_table.get(func_idx)?;
    let mut end = module.instructions.len();

    if let Some(info) = module
        .debug_functions
        .iter()
        .find(|info| info.code_offset == start)
    {
        end = end.min(start.saturating_add(info.code_len));
    }
    for &offset in &module.function_table {
        if offset > start {
            end = end.min(offset);
        }
    }
    for behavior in &module.behaviors {
        if behavior.code_offset > start {
            end = end.min(behavior.code_offset);
        }
    }
    if let Some(entry) = module.entry_point {
        if entry > start {
            end = end.min(entry);
        }
    }
    if end <= start {
        return None;
    }

    let terminal_pc = end - 1;
    let terminal = *module.instructions.get(terminal_pc)?;
    let ret_reg = match terminal.opcode {
        OpCode::Ret => 0,
        OpCode::RetVal => terminal.op1,
        _ => return None,
    };

    let body_len = terminal_pc - start;
    if body_len > NATIVE_LEAF_MAX_INSTRS {
        return None;
    }

    let mut writes = rustc_hash::FxHashSet::default();
    for instr in &module.instructions[start..terminal_pc] {
        match instr.opcode {
            OpCode::Nop => {}
            OpCode::Const0 | OpCode::Const1 | OpCode::Const2 | OpCode::ConstM1 => {
                writes.insert(instr.op1);
            }
            OpCode::ConstU => {
                writes.insert(instr.op3);
            }
            OpCode::Load | OpCode::Store | OpCode::Move | OpCode::Dup => {
                writes.insert(instr.op2);
            }
            OpCode::Swap => {
                writes.insert(instr.op1);
                writes.insert(instr.op2);
            }
            OpCode::IAdd
            | OpCode::ISub
            | OpCode::IMul
            | OpCode::IDiv
            | OpCode::IMod
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
            | OpCode::ICmpEq
            | OpCode::ICmpLt
            | OpCode::ICmpGt
            | OpCode::ICmpLe
            | OpCode::ICmpGe
            | OpCode::FCmpEq
            | OpCode::FCmpLt
            | OpCode::FCmpGt
            | OpCode::And
            | OpCode::Or => {
                writes.insert(instr.op3);
            }
            OpCode::INeg => return None,
            OpCode::Not | OpCode::IToF | OpCode::FToI => {
                writes.insert(instr.op2);
            }
            OpCode::FNeg => {
                writes.insert(instr.op3);
            }
            OpCode::IInc | OpCode::IDec => {
                writes.insert(instr.op1);
            }
            _ => return None,
        }
    }

    let mut clobbers: Vec<u8> = writes.into_iter().collect();
    clobbers.sort_unstable();
    Some(NativeLeafPlan {
        start,
        body_len,
        ret_reg,
        clobbers,
    })
}

/// The code offset of the function containing `pc` (largest
/// `function_table[i] <= pc`), bounding `direct_call_target`'s backward walk.
pub(crate) fn func_start_for(module: &crate::bytecode::CodeModule, pc: usize) -> usize {
    module
        .function_table
        .iter()
        .copied()
        .filter(|&o| o <= pc)
        .next_back()
        .unwrap_or(0)
}

/// If the instruction at `pc` is a `Call` of a provably-non-suspending direct
/// callee (recoverable via `direct_call_target` and gated on `may_suspend`
/// and on not being in a direct-call recursion cycle), return the callee's
/// function-table index. Such a call is safe to compile into the region as a
/// `nulang_jit_direct_call` helper invocation. Returns None for indirect
/// calls, suspending callees, recursive callees, and every other opcode.
pub(crate) fn native_direct_call(
    module: &crate::bytecode::CodeModule,
    pc: usize,
    may_suspend: Option<&[bool]>,
    recursive: Option<&[bool]>,
) -> Option<usize> {
    use crate::bytecode::OpCode;
    let instr = module.instructions.get(pc)?;
    if instr.opcode != OpCode::Call {
        return None;
    }
    let idx = direct_call_target(module, pc, func_start_for(module, pc))?;
    // A suspending callee must not be run re-entrantly from a compiled
    // region: it could suspend mid-run and be re-entered from its call start,
    // double-executing pre-suspend side effects. Stay on the interpreter.
    if may_suspend.is_some_and(|v| v.get(idx) == Some(&true)) {
        return None;
    }
    // A recursive callee must not go through the re-entrant helper either:
    // each helper call consumes native stack, so unbounded recursion would
    // overflow it. The interpreter uses heap-allocated frames instead.
    if recursive.is_some_and(|v| v.get(idx) == Some(&true)) {
        return None;
    }
    Some(idx)
}

/// Like [`find_compilable_region`], but additionally continues past `Call`
/// instructions whose direct callee is provably non-suspending, returning the
/// region length and the map of (absolute pc -> direct callee func index) for
/// the calls that were folded into the region. The caller passes this map to
/// the scalar compiler so it can emit `nulang_jit_direct_call` at those pcs.
pub(crate) fn find_compilable_region_with_calls(
    offset: usize,
    instructions: &[crate::bytecode::Instruction],
    module: &crate::bytecode::CodeModule,
    may_suspend: Option<&[bool]>,
    recursive: Option<&[bool]>,
) -> (usize, std::collections::HashMap<usize, usize>) {
    let mut native_calls = std::collections::HashMap::new();
    let mut len = 0;
    let mut first_branch: Option<usize> = None;
    let mut has_back_edge = false;
    let mut max_forward_target = offset;

    for i in offset..instructions.len().min(offset + 500) {
        let op = instructions[i].opcode;
        if op == crate::bytecode::OpCode::Call {
            match native_direct_call(module, i, may_suspend, recursive) {
                Some(idx) => {
                    native_calls.insert(i, idx);
                }
                None => break, // indirect / suspending / recursive call — stop
            }
        } else if !compiler::is_opcode_compilable(op) {
            break;
        }

        if op == crate::bytecode::OpCode::Halt {
            break;
        }
        if matches!(
            op,
            crate::bytecode::OpCode::Ret | crate::bytecode::OpCode::RetVal
        ) {
            if max_forward_target <= i {
                break;
            }
            len += 1;
            continue;
        }

        let is_branch = matches!(
            op,
            crate::bytecode::OpCode::Jmp
                | crate::bytecode::OpCode::JmpT
                | crate::bytecode::OpCode::JmpF
        );
        if is_branch {
            if first_branch.is_none() {
                first_branch = Some(len);
            }
            let target = match op {
                crate::bytecode::OpCode::Jmp => {
                    (i as i64 + instructions[i].simm16() as i64) as usize
                }
                _ => (i as i64 + instructions[i].offset16() as i64) as usize,
            };
            if target > i {
                max_forward_target = max_forward_target.max(target);
            }
            if target >= offset && target < i {
                has_back_edge = true;
            }
        }

        len += 1;
        if has_back_edge && i >= max_forward_target {
            break;
        }
    }

    if !has_back_edge && first_branch.unwrap_or(len) < STRAIGHT_LINE_MIN {
        (0, std::collections::HashMap::new())
    } else {
        (len, native_calls)
    }
}

#[cfg(test)]
mod planner_tests {
    use super::*;
    use crate::bytecode::{Instruction, OpCode};

    #[test]
    fn test_region_planner_produces_typed_plan_for_hot_arithmetic_region() {
        let mut module = CodeModule::new("planner_typed");
        module.emit(Instruction::new1(OpCode::Const0, 0));
        module.emit(Instruction::new1(OpCode::Const1, 1));
        for _ in 0..8 {
            module.emit(Instruction::new3(OpCode::IAdd, 0, 1, 0));
        }
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut planner = RegionPlanner::default();
        let plan = planner.plan(0, 2, &module);

        assert_eq!(plan.len, 8);
        assert!(plan.native_calls.is_empty());
        assert!(
            plan.type_metadata().is_some(),
            "planner should preserve type facts for the backend"
        );
    }

    #[test]
    fn test_native_leaf_analysis_tracks_exact_clobbers() {
        let mut module = CodeModule::new("native_leaf_analysis");
        module.function_table.push(0);
        module.emit(Instruction::new2(OpCode::Move, 0, 15));
        module.emit(Instruction::new1(OpCode::IInc, 15));
        module.emit(Instruction::new1(OpCode::RetVal, 15));

        let leaf = analyze_native_leaf(&module, 0).expect("straight-line leaf must be eligible");
        assert_eq!(leaf.start, 0);
        assert_eq!(leaf.body_len, 2);
        assert_eq!(leaf.ret_reg, 15);
        assert_eq!(leaf.clobbers, vec![15]);
    }

    #[test]
    fn test_native_leaf_analysis_rejects_erroring_and_branchy_bodies() {
        let mut ineg = CodeModule::new("native_leaf_ineg");
        ineg.function_table.push(0);
        ineg.emit(Instruction::new2(OpCode::INeg, 0, 15));
        ineg.emit(Instruction::new1(OpCode::RetVal, 15));
        assert!(analyze_native_leaf(&ineg, 0).is_none());

        let mut branchy = CodeModule::new("native_leaf_branchy");
        branchy.function_table.push(0);
        branchy.emit(Instruction::new3(OpCode::ICmpGt, 0, 1, 2));
        branchy.emit(Instruction::new3(OpCode::JmpT, 2, 0, 2));
        branchy.emit(Instruction::new1(OpCode::RetVal, 0));
        assert!(analyze_native_leaf(&branchy, 0).is_none());
    }

    #[test]
    fn test_direct_call_target_rejects_stale_r254_definition() {
        let mut module = CodeModule::new("direct_call_stale_target");
        module.function_table.push(0);
        module.emit(Instruction::new1(OpCode::Const0, 254));
        module.emit(Instruction::new1(OpCode::Const1, 1));
        module.emit(Instruction::new3(OpCode::IAdd, 1, 1, 254));
        let call_pc = module.emit(Instruction::new3(OpCode::Call, 254, 0, 10));

        assert_eq!(
            direct_call_target(&module, call_pc, 0),
            None,
            "direct-call recovery must not cross an unrelated r254 write"
        );
    }

    #[test]
    fn test_region_planner_rejects_short_straight_line_region() {
        let mut module = CodeModule::new("planner_short");
        module.emit(Instruction::new1(OpCode::Const0, 0));
        module.emit(Instruction::new3(OpCode::IAdd, 0, 0, 0));
        module.emit(Instruction::new0(OpCode::Halt));
        module.entry_point = Some(0);

        let mut planner = RegionPlanner::default();
        let plan = planner.plan(0, 0, &module);

        assert_eq!(plan.len, 0);
        assert!(plan.native_calls.is_empty());
    }
}
