//! Pre-render proof over the actual compiled page handler and its callees.
//!
//! Check every emitted instruction in the transitive direct-call graph, not
//! just executed branches or the declared effect row. Package imports are
//! already linked into the module, so no separate source-name resolution is
//! needed. Indirect calls whose target is not proven remain server-only.
//! This is a conservative build-time effect policy, not a termination proof,
//! general-purpose bytecode verifier, or sandbox for untrusted programs.

use crate::bytecode::{CodeModule, Constant, Instruction, OpCode};
use std::collections::{BTreeSet, VecDeque};

/// Prove that every statically reachable function obeys the same operation
/// policy enforced by the interpreter during the actual render.
pub(super) fn prove_handler(module: &CodeModule, handler: usize) -> Result<(), String> {
    let mut pending = VecDeque::from([handler]);
    let mut visited = BTreeSet::new();
    while let Some(index) = pending.pop_front() {
        // Recursive and mutually recursive direct calls terminate analysis
        // without making any claim about termination of the program itself.
        if !visited.insert(index) {
            continue;
        }
        let (name, start, end) = function_range(module, index)?;
        let fail =
            |pc, reason: String| format!("static proof: {name} at instruction {pc}: {reason}");
        let mut targets = BTreeSet::from([start]);

        // A function range is a proof boundary: a jump may not escape it.
        // Include every branch target, even in unreachable compiled blocks,
        // so a jump cannot bypass a staged callee constant.
        for pc in start..end {
            let instr = &module.instructions[pc];
            check_instruction(module, instr).map_err(|reason| fail(pc, reason))?;
            let offset = match instr.opcode {
                OpCode::Jmp => Some(instr.simm16()),
                OpCode::JmpT | OpCode::JmpF => Some(instr.offset16()),
                OpCode::Switch => return Err(fail(pc, "unsupported jump table".into())),
                _ => None,
            };
            if let Some(offset) = offset {
                let target = pc
                    .checked_add_signed(isize::from(offset))
                    .filter(|target| (start..end).contains(target))
                    .ok_or_else(|| fail(pc, "jump escapes the compiled function".into()))?;
                targets.insert(target);
            }
        }
        if !matches!(
            module.instructions[end - 1].opcode,
            OpCode::Ret
                | OpCode::RetVal
                | OpCode::Jmp
                | OpCode::TailCall
                | OpCode::Halt
                | OpCode::Panic
        ) {
            return Err(fail(
                end - 1,
                "function may fall through its compiled range".into(),
            ));
        }

        // MIR codegen stages a direct callee constant followed only by
        // argument Moves and SpillLoads before Call. Track that exact pattern
        // within a basic block. Any other instruction or branch entry kills
        // the fact. Unlike the JIT's backward-scan hint, this proves the
        // definition cannot be bypassed on any control-flow path.
        let mut staged = None;
        for pc in start..end {
            if targets.contains(&pc) {
                staged = None;
            }
            let instr = &module.instructions[pc];
            match instr.opcode {
                OpCode::Call | OpCode::TailCall | OpCode::ClosureCall => {
                    let target = staged
                        .filter(|target: &StagedCallee| {
                            target.register == instr.op1
                                && (instr.opcode != OpCode::TailCall || !target.closure)
                        })
                        .ok_or_else(|| fail(pc, "unproven indirect call target".into()))?;
                    if target.index >= module.function_table.len() {
                        return Err(fail(
                            pc,
                            format!("unresolved callee index {}", target.index),
                        ));
                    }
                    pending.push_back(target.index);
                    staged = None;
                }
                OpCode::Move if staged.is_some_and(|target| target.register != instr.op2) => {}
                OpCode::SpillLoad if staged.is_some_and(|target| target.register != instr.op3) => {}
                _ => staged = staged_callee(module, instr),
            }
        }
    }
    Ok(())
}

/// Debug function ranges are emitted by MIR codegen and survive the existing
/// NBC round trip. Refuse absent, malformed, or overlapping ranges rather than
/// scanning another function by accident. No NBC metadata is added.
fn function_range(module: &CodeModule, index: usize) -> Result<(&str, usize, usize), String> {
    let start = *module
        .function_table
        .get(index)
        .ok_or_else(|| format!("static proof: unresolved function index {index}"))?;
    let mut matches = module
        .debug_functions
        .iter()
        .filter(|f| f.code_offset == start);
    let function = matches
        .next()
        .ok_or_else(|| format!("static proof: missing compiled range for function {index}"))?;
    let end = start
        .checked_add(function.code_len)
        .filter(|end| start < *end && *end <= module.instructions.len())
        .ok_or_else(|| format!("static proof: invalid compiled range for {}", function.name))?;
    if matches.next().is_some()
        || module
            .function_table
            .iter()
            .any(|offset| start < *offset && *offset < end)
        || module
            .entry_point
            .is_some_and(|entry| (start..end).contains(&entry))
    {
        return Err(format!(
            "static proof: ambiguous compiled range for {}",
            function.name
        ));
    }
    Ok((&function.name, start, end))
}

#[derive(Clone, Copy)]
struct StagedCallee {
    register: u8,
    index: usize,
    closure: bool,
}

fn staged_callee(module: &CodeModule, instr: &Instruction) -> Option<StagedCallee> {
    let (register, index, closure) = match instr.opcode {
        OpCode::Const0 => (instr.op1, 0, false),
        OpCode::Const1 => (instr.op1, 1, false),
        OpCode::Const2 => (instr.op1, 2, false),
        OpCode::ConstU => match module.constants.get(instr.imm16() as usize)? {
            Constant::Int(index) => (instr.op3, usize::try_from(*index).ok()?, false),
            _ => return None,
        },
        OpCode::Closure => (instr.op3, instr.imm16() as usize, true),
        _ => return None,
    };
    Some(StagedCallee {
        register,
        index,
        closure,
    })
}

// Shared operation allowlist for the static proof and checked interpreter.
// This is a build policy, not an untrusted-code sandbox.
pub(super) fn check_instruction(module: &CodeModule, instr: &Instruction) -> Result<(), String> {
    use OpCode::*;
    match instr.opcode {
        Nop | Halt | Panic | Const0 | Const1 | Const2 | ConstM1 | ConstU | ConstL | Load
        | Store | Move | Pop | Dup | Swap | IAdd | ISub | IMul | IDiv | IMod | INeg | IInc
        | IDec | IPow | Xor | Shl | Shr | BitAnd | BitOr | FAdd | FSub | FMul | FDiv | FNeg
        | FMod | IToF | FToI | FToS | FPow | ICmpEq | ICmpLt | ICmpGt | ICmpLe | ICmpGe
        | FCmpEq | FCmpLt | FCmpGt | SCmpEq | Not | And | Or | Jmp | JmpT | JmpF | Switch
        | Call | TailCall | Ret | RetVal | Closure | CapLoad | CapStore | ClosureCall | Alloc
        | FieldL | FieldS | ArrAlloc | ArrLoad | ArrStore | ArrLen | TupleMk | TupleL | RecMk
        | RecL | RecS | IsTag | Unpack | RecCopy | Copy | Drop | SConcat | MetaType | MetaCap
        | SpillLoad | SpillStore => Ok(()),
        Perform => {
            let operation = module.constants.get(instr.imm16() as usize);
            match operation {
                Some(Constant::String(name)) if allowed_effect(name) => Ok(()),
                Some(Constant::String(name)) => {
                    Err(format!("build-time rendering disallows {name}"))
                }
                _ => Err("build-time rendering encountered an invalid effect operand".into()),
            }
        }
        other => Err(format!("build-time rendering disallows opcode {other:?}")),
    }
}

fn allowed_effect(name: &str) -> bool {
    matches!(
        name,
        "Web.html"
            | "Web.text"
            | "Web.raw"
            | "Array.length"
            | "Array.push"
            | "Array.new"
            | "Array.set"
            | "Array.slice"
            | "Int.to_string"
            | "Int.to_float"
            | "Int.to_hex"
            | "Int.to_binary"
            | "Float.to_int"
            | "Float.to_string"
            | "Float.sin"
            | "Float.cos"
            | "Float.sqrt"
            | "Float.tan"
            | "Float.log"
            | "Float.exp"
            | "Float.log2"
            | "Float.log10"
            | "Float.pow"
            | "String.to_int"
            | "String.to_float"
            | "String.length"
            | "String.charAt"
            | "String.from_char"
            | "String.concat"
            | "String.substring"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::DebugFunctionInfo;
    use OpCode::*;

    fn module(functions: &[&[Instruction]]) -> CodeModule {
        let mut module = CodeModule::new("static-proof");
        module.constants.extend([
            Constant::String("Web.cookie".into()),
            Constant::String("Web.text".into()),
            Constant::Int(3),
        ]);
        for (index, instructions) in functions.iter().enumerate() {
            let offset = module.instructions.len();
            module.function_table.push(offset);
            module.debug_functions.push(DebugFunctionInfo {
                name: format!("function_{index}"),
                code_offset: offset,
                code_len: instructions.len(),
                params: Vec::new(),
                locals: Vec::new(),
            });
            module.instructions.extend_from_slice(instructions);
        }
        module
    }

    fn direct(index: u8) -> Instruction {
        Instruction::new1(
            match index {
                0 => Const0,
                1 => Const1,
                2 => Const2,
                _ => panic!("use ConstU for larger indices"),
            },
            254,
        )
    }

    fn call() -> Instruction {
        Instruction::new3(Call, 254, 0, 0)
    }

    fn ret() -> Instruction {
        Instruction::new0(Ret)
    }

    #[test]
    fn cold_effect_in_transitive_callee_fails_before_execution() {
        let module = module(&[
            &[direct(1), call(), ret()],
            &[direct(2), call(), ret()],
            &[
                Instruction::new3(Jmp, 0, 2, 0),
                Instruction::new3(Perform, 0, 0, 0), // bypassed by Jmp
                ret(),
            ],
        ]);
        let error = prove_handler(&module, 0).unwrap_err();
        assert!(
            error.contains("function_2") && error.contains("disallows Web.cookie"),
            "{error}"
        );
    }

    #[test]
    fn unrelated_effectful_functions_do_not_poison_pure_recursive_graph() {
        let module = module(&[
            &[direct(1), call(), ret()],
            &[direct(0), call(), ret()],
            &[Instruction::new3(Perform, 0, 0, 0), ret()],
        ]);
        assert!(prove_handler(&module, 0).is_ok());
        assert!(prove_handler(&module, 2).is_err());
    }

    #[test]
    fn directly_staged_closure_and_large_index_with_spills_are_proved() {
        let module = module(&[
            &[
                Instruction::new3(Closure, 0, 3, 254),
                Instruction::new2(Move, 10, 0),
                Instruction::new3(ClosureCall, 254, 1, 0),
                Instruction::new3(ConstU, 0, 2, 254), // pool int 3
                Instruction::new3(SpillLoad, 0, 0, 251),
                Instruction::new2(Move, 251, 0),
                call(),
                ret(),
            ],
            &[ret()],
            &[ret()],
            &[Instruction::new3(Perform, 0, 1, 0), ret()],
        ]);
        assert!(prove_handler(&module, 0).is_ok());
    }

    #[test]
    fn branch_cannot_bypass_constant_staging_or_enter_call_directly() {
        for offset in [2, 3] {
            let module = module(&[
                &[
                    Instruction::new3(JmpT, 0, 0, offset),
                    direct(1),
                    Instruction::new2(Move, 10, 0),
                    call(),
                    ret(),
                ],
                &[ret()],
            ]);
            let error = prove_handler(&module, 0).unwrap_err();
            assert!(error.contains("unproven indirect call"), "{error}");
        }
    }

    #[test]
    fn clobbered_or_indirect_call_targets_fail_closed() {
        for earlier in [
            Instruction::new2(Move, 11, 254),
            Instruction::new3(SpillLoad, 0, 0, 254),
            Instruction::new3(IAdd, 10, 11, 254),
        ] {
            let module = module(&[&[direct(1), earlier, call(), ret()], &[ret()]]);
            let error = prove_handler(&module, 0).unwrap_err();
            assert!(error.contains("unproven indirect call"), "{error}");
        }
        let module = module(&[&[direct(2), call(), ret()]]);
        assert!(prove_handler(&module, 0)
            .unwrap_err()
            .contains("unresolved callee"));
    }

    #[test]
    fn unknown_ranges_and_cross_function_control_flow_fail_closed() {
        let mut module = module(&[&[Instruction::new3(Jmp, 0, 1, 0)], &[ret()]]);
        assert!(prove_handler(&module, 0)
            .unwrap_err()
            .contains("jump escapes"));
        module.instructions[0] = Instruction::new0(Nop);
        assert!(prove_handler(&module, 0)
            .unwrap_err()
            .contains("fall through"));
        module.debug_functions[0].code_len = 2;
        assert!(prove_handler(&module, 0).unwrap_err().contains("ambiguous"));
        module.debug_functions.clear();
        assert!(prove_handler(&module, 0)
            .unwrap_err()
            .contains("missing compiled range"));
    }

    #[test]
    fn effect_policy_denies_ambient_and_external_operations() {
        for name in [
            "Web.cookie",
            "Web.method",
            "Web.kv_get",
            "Web.kv_set",
            "Web.serve_static",
            "FS.read",
            "IO.print",
            "Http.get",
            "Time.now",
            "Rand.int",
        ] {
            let mut module = CodeModule::new("test");
            module.constants.push(Constant::String(name.into()));
            assert!(
                check_instruction(&module, &Instruction::new3(OpCode::Perform, 0, 0, 0)).is_err(),
                "{name}"
            );
        }
        for opcode in [
            OpCode::FFICall,
            OpCode::Spawn,
            OpCode::SRead,
            OpCode::PerformAsync,
        ] {
            assert!(
                check_instruction(&CodeModule::new("test"), &Instruction::new0(opcode)).is_err()
            );
        }
    }
}
