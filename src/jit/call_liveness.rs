//! Bytecode-register liveness for JIT direct-call lowering.
//!
//! The current JIT region ABI shares one 256-slot register file with compiled
//! code. A future native JIT-to-JIT call therefore has to preserve every
//! caller register whose value is live after the call, except the call's
//! destination register (which is intentionally overwritten by the result).
//!
//! This analysis runs over the whole module rather than only the compiled
//! region. That is important: a value can be live across a call even when its
//! next use happens after the region exits back to the interpreter.
//!
//! The transfer function is deliberately conservative. Opcodes whose register
//! semantics are not explicitly modeled use every register and define none.
//! That can only make the caller-save set larger; it cannot cause an
//! under-save.

use crate::bytecode::{Instruction, OpCode};

const REG_COUNT: usize = 256;
const WORDS: usize = REG_COUNT / 64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegisterSet([u64; WORDS]);

impl RegisterSet {
    pub(crate) fn all() -> Self {
        Self([u64::MAX; WORDS])
    }

    pub(crate) fn insert(&mut self, reg: usize) {
        if reg < REG_COUNT {
            self.0[reg / 64] |= 1u64 << (reg % 64);
        }
    }

    pub(crate) fn remove(&mut self, reg: usize) {
        if reg < REG_COUNT {
            self.0[reg / 64] &= !(1u64 << (reg % 64));
        }
    }

    pub(crate) fn contains(&self, reg: usize) -> bool {
        reg < REG_COUNT && (self.0[reg / 64] & (1u64 << (reg % 64))) != 0
    }

    fn union_with(&mut self, other: Self) {
        for (dst, src) in self.0.iter_mut().zip(other.0) {
            *dst |= src;
        }
    }

    fn without(self, defs: Self) -> Self {
        let mut out = self;
        for (dst, killed) in out.0.iter_mut().zip(defs.0) {
            *dst &= !killed;
        }
        out
    }

    #[cfg(test)]
    fn members(self) -> Vec<usize> {
        (0..REG_COUNT).filter(|&r| self.contains(r)).collect()
    }
}

/// Metadata for a folded direct call.
///
/// caller_save is currently analysis-only: the helper-backed call path still
/// preserves the caller via interpreter frames. The next native-call slice
/// will use this exact set to save/restore only values that survive the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeCallSite {
    pub(crate) callee: usize,
    pub(crate) caller_save: RegisterSet,
}

fn one(reg: usize) -> RegisterSet {
    let mut set = RegisterSet::default();
    set.insert(reg);
    set
}

fn two(a: usize, b: usize) -> RegisterSet {
    let mut set = one(a);
    set.insert(b);
    set
}

fn three(a: usize, b: usize, c: usize) -> RegisterSet {
    let mut set = two(a, b);
    set.insert(c);
    set
}

/// Return (uses, defs) for one bytecode instruction.
///
/// Only JIT-relevant/common VM opcodes need precise modeling. The catch-all
/// returns every register as used and no definitions, which is intentionally
/// pessimistic.
fn uses_defs(instr: &Instruction) -> (RegisterSet, RegisterSet) {
    use OpCode::*;

    let r1 = instr.op1 as usize;
    let r2 = instr.op2 as usize;
    let r3 = instr.op3 as usize;

    match instr.opcode {
        Nop => (RegisterSet::default(), RegisterSet::default()),

        Const0 | Const1 | Const2 | ConstM1 => (RegisterSet::default(), one(r1)),
        ConstU | ConstL => (RegisterSet::default(), one(r3)),

        Load | Store | Move | Dup => (one(r1), one(r2)),
        Swap => (two(r1, r2), two(r1, r2)),

        IAdd | ISub | IMul | IDiv | IMod | IPow | FPow | Xor | Shl | Shr | BitAnd | BitOr
        | FAdd | FSub | FMul | FDiv | FMod | ICmpEq | ICmpLt | ICmpGt | ICmpLe | ICmpGe
        | FCmpEq | FCmpLt | FCmpGt | SCmpEq | And | Or => (two(r1, r2), one(r3)),

        INeg | IToF | FToI | FToS | Not | IsTag => (one(r1), one(r2)),
        FNeg => (one(r1), one(r3)),
        IInc | IDec => (one(r1), one(r1)),

        Jmp => (RegisterSet::default(), RegisterSet::default()),
        JmpT | JmpF => (one(r1), RegisterSet::default()),

        ArrLoad => (two(r1, r2), one(r3)),
        ArrStore => (three(r1, r2, r3), RegisterSet::default()),
        ArrLen => (one(r1), one(r2)),
        FieldL | RecL => (one(r1), one(r3)),
        FieldS | RecS => (two(r1, r3), RegisterSet::default()),
        ArrAlloc => (one(r1), one(r2)),
        RecMk | TupleMk => (RegisterSet::default(), one(r2)),
        RecCopy => (one(r1), one(r2)),

        Call => {
            let mut uses = one(r1);
            for reg in 0..r2.min(REG_COUNT) {
                uses.insert(reg);
            }
            (uses, one(r3))
        }

        RetVal => (one(r1), RegisterSet::default()),
        Ret | Halt => (one(0), RegisterSet::default()),

        DbgPrint | PerformDirect => (RegisterSet::all(), RegisterSet::default()),

        _ => (RegisterSet::all(), RegisterSet::default()),
    }
}

fn successors(instructions: &[Instruction], pc: usize) -> Vec<usize> {
    let n = instructions.len();
    let instr = &instructions[pc];
    let fallthrough = (pc + 1 < n).then_some(pc + 1);

    match instr.opcode {
        OpCode::Jmp => {
            let target = pc as i64 + instr.simm16() as i64;
            if (0..n as i64).contains(&target) {
                vec![target as usize]
            } else {
                Vec::new()
            }
        }
        OpCode::JmpT | OpCode::JmpF => {
            let target = pc as i64 + instr.offset16() as i64;
            let mut out = Vec::with_capacity(2);
            if let Some(next) = fallthrough {
                out.push(next);
            }
            if (0..n as i64).contains(&target) {
                let target = target as usize;
                if !out.contains(&target) {
                    out.push(target);
                }
            }
            out
        }
        OpCode::Ret | OpCode::RetVal | OpCode::Halt | OpCode::Panic => Vec::new(),
        _ => fallthrough.into_iter().collect(),
    }
}

/// Compute the live-out register set at every bytecode PC.
///
/// Standard backwards dataflow:
/// out[n] = union(in[succ])
/// in[n]  = use[n] U (out[n] - def[n])
pub(crate) fn compute_live_out(instructions: &[Instruction]) -> Vec<RegisterSet> {
    let n = instructions.len();
    let mut live_in = vec![RegisterSet::default(); n];
    let mut live_out = vec![RegisterSet::default(); n];

    loop {
        let mut changed = false;

        for pc in (0..n).rev() {
            let mut out = RegisterSet::default();
            for succ in successors(instructions, pc) {
                out.union_with(live_in[succ]);
            }

            let (uses, defs) = uses_defs(&instructions[pc]);
            let mut input = out.without(defs);
            input.union_with(uses);

            if live_out[pc] != out {
                live_out[pc] = out;
                changed = true;
            }
            if live_in[pc] != input {
                live_in[pc] = input;
                changed = true;
            }
        }

        if !changed {
            break;
        }
    }

    live_out
}

/// Caller registers that must survive a native call at pc.
///
/// The destination is removed because the call result intentionally replaces
/// its old value. Staged argument registers are not removed: if one remains
/// live after the call, native callee code may clobber it and it must be saved.
pub(crate) fn caller_save_set(
    instructions: &[Instruction],
    live_out: &[RegisterSet],
    pc: usize,
) -> RegisterSet {
    let mut save = live_out.get(pc).copied().unwrap_or_else(RegisterSet::all);
    if let Some(instr) = instructions.get(pc) {
        if instr.opcode == OpCode::Call {
            save.remove(instr.op3 as usize);
        }
    }
    save
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_saves_only_values_live_after_it() {
        let code = vec![
            Instruction::new3(OpCode::Call, 254, 1, 5),
            Instruction::new3(OpCode::IAdd, 5, 10, 11),
            Instruction::new1(OpCode::RetVal, 11),
        ];

        let live = compute_live_out(&code);
        let save = caller_save_set(&code, &live, 0);

        assert_eq!(save.members(), vec![10]);
        assert!(!save.contains(5), "return destination must not be preserved");
    }

    #[test]
    fn call_liveness_flows_through_loop_backedge() {
        let code = vec![
            Instruction::new3(OpCode::Call, 254, 1, 5),
            Instruction::new3(OpCode::IAdd, 5, 10, 10),
            Instruction::new1(OpCode::IInc, 1),
            Instruction::new3(OpCode::ICmpLt, 1, 2, 3),
            Instruction::new3(OpCode::JmpT, 3, 0xFF, 0xFC),
            Instruction::new1(OpCode::RetVal, 10),
        ];

        let live = compute_live_out(&code);
        let save = caller_save_set(&code, &live, 0);

        assert!(save.contains(1), "loop induction value is live across call");
        assert!(save.contains(2), "loop limit is live across call");
        assert!(save.contains(10), "loop accumulator is live across call");
        assert!(!save.contains(5), "call destination is overwritten by result");
    }

    #[test]
    fn unknown_opcode_is_fail_safe() {
        let code = vec![
            Instruction::new3(OpCode::Call, 254, 0, 7),
            Instruction::new3(OpCode::PerformDirect, 0, 0, 8),
            Instruction::new1(OpCode::RetVal, 8),
        ];

        let live = compute_live_out(&code);
        let save = caller_save_set(&code, &live, 0);

        assert!(save.contains(0));
        assert!(save.contains(254));
        assert!(!save.contains(7), "call destination still must not be restored");
    }
}
