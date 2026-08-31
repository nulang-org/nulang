//! MIR → CIR lowering for the WasmFX backend.
//!
//! Only functions containing at least one suspension point get a CIR
//! representation. Functions with only non-suspending effects (IO.print,
//! Actor.send) or no effects stay on the existing `mir_wasm.rs` codegen path.
//!
//! Suspension points in MIR are implicit — they are `RValue::ReceiveWait`,
//! `RValue::SignalWait`, `RValue::Perform("LLM", "ask")`,
//! `RValue::PerformAsync`, and `RValue::ReceiveMatch` (blocking mailbox
//! dequeue). CIR makes them explicit as `CirTerminator::SuspendAndYield`.
//!
//! Variable mapping: MIR locals use a flat register model. CIR `VarId`s are
//! the flat Wasm local indices, so `var(local) = pc + local.0` where
//! `pc = params.len() + captures.len()` — the same convention as
//! `mir_wasm::WasmBackend::mir_local`. The frame-pointer local is a fixed
//! high index (252), matching the scratch-local convention of `mir_wasm.rs`.

use crate::cir::{
    BinaryOp, BlockId, CirBlock, CirExpr, CirFunction, CirLocal, CirStmt, CirTerminator,
    EffectKind, UnaryOp, VarId,
};
use crate::mir::{self, LocalId, RValue, Stmt, Terminator};
use crate::value_layout;
use std::collections::HashMap;

/// Reserved frame-pointer local (i64 holding a zero-extended i32 pointer).
pub const FRAME_PTR_VAR: VarId = VarId(252);
/// Reserved local receiving the host-provided resume value (i64).
pub const RESULT_VAR: VarId = VarId(253);
/// Number of function imports the WasmFX backend emits; module functions
/// start at this index (mirrors `wasmfx_backend::FUNC_IMPORT_COUNT`).
pub const WASM_FUNC_IMPORT_COUNT: u32 = 6;

/// Returns true if the MIR function contains at least one suspension point.
pub fn has_suspension(func: &mir::Function) -> bool {
    for block in &func.blocks {
        for stmt in &block.stmts {
            if let Stmt::Assign { op, .. } = stmt {
                if is_suspending_rvalue(op) {
                    return true;
                }
            }
            if let Stmt::EnterHandle { .. } = stmt {
                // User-defined effect handler: Terminator::Resume appears
                // somewhere in this function.
                return true;
            }
        }
    }
    false
}

/// Returns true if this rvalue suspends (or may suspend) the computation.
pub fn is_suspending_rvalue(op: &RValue) -> bool {
    match op {
        RValue::Perform {
            effect, op, args, ..
        } => effect == "LLM" && op == "ask" && !args.is_empty(),
        RValue::SignalWait { .. } => true,
        RValue::ReceiveWait { .. } => true,
        RValue::ReceiveMatch { .. } => true,
        RValue::PerformAsync { .. } => true,
        _ => false,
    }
}

/// Non-suspending functions produce a CIR with no `SuspendAndYield`
/// terminators and compile to plain Wasm (no `cont.new`/`suspend`).
pub fn lower_mir_function_unconditional(func: &mir::Function) -> CirFunction {
    let pc = (func.params.len() + func.captures.len()) as u32;

    // CIR block ids map 1:1 to MIR block ids; fresh blocks (from suspension
    // splitting) start after the MIR block count.
    let mut next_fresh_id = func.blocks.len() as u32;
    // Map from MIR block id → the first CIR block id produced from it.
    let mut block_map: HashMap<u32, u32> = HashMap::new();

    let mut cir_blocks: Vec<CirBlock> = Vec::new();

    // Worklist: (stmts to scan, terminator, block id to assign).
    // A MIR block may split into several CIR blocks when it contains
    // multiple suspension points in sequence.
    let mut worklist: Vec<(Vec<Stmt>, Terminator, u32)> = Vec::new();
    for mir_block in &func.blocks {
        block_map.entry(mir_block.id.0).or_insert(mir_block.id.0);
        worklist.push((
            mir_block.stmts.clone(),
            mir_block.terminator.clone(),
            mir_block.id.0,
        ));
    }

    while let Some((stmts, terminator, id)) = worklist.pop() {
        let mut current_stmts: Vec<CirStmt> = Vec::new();
        let mut split_at: Option<usize> = None;

        for (i, stmt) in stmts.iter().enumerate() {
            if let Stmt::Assign { op, .. } = stmt {
                if is_suspending_rvalue(op) {
                    split_at = Some(i);
                    break;
                }
            }
            current_stmts.push(translate_stmt(stmt, func, pc));
        }

        match split_at {
            None => {
                // No suspension: emit the block as-is.
                cir_blocks.push(CirBlock {
                    id: BlockId(id),
                    stmts: current_stmts,
                    terminator: translate_terminator(&terminator, func, pc),
                });
            }
            Some(i) => {
                let stmt = &stmts[i];
                let (dst, op) = match stmt {
                    Stmt::Assign { dst, op } => (*dst, op),
                    _ => unreachable!(),
                };
                let (effect, args) = suspend_effect_and_args(op, pc);
                let resume_id = next_fresh_id;
                next_fresh_id += 1;

                // Pre-suspend block: statements before the suspension,
                // terminating in SuspendAndYield.
                cir_blocks.push(CirBlock {
                    id: BlockId(id),
                    stmts: current_stmts,
                    terminator: CirTerminator::SuspendAndYield {
                        effect,
                        args,
                        resume_block: BlockId(resume_id),
                        resume_var: var(&dst, pc),
                        live_vars: Vec::new(), // populated by cir_analysis
                    },
                });

                // Resume block: statements after the suspension + the
                // original terminator. Push back on the worklist so further
                // suspensions inside it are split too.
                let rest: Vec<Stmt> = stmts[i + 1..].to_vec();
                worklist.push((rest, terminator, resume_id));
            }
        }
    }

    // Reorder blocks so ids are contiguous and entry is first; the codegen
    // dispatches by block id, so a stable id→position mapping is required.
    cir_blocks.sort_by_key(|b| b.id.0);
    for (pos, block) in cir_blocks.iter().enumerate() {
        assert_eq!(block.id.0 as usize, pos, "CIR block ids must be dense");
    }

    // CIR locals: flat Wasm local indices 0..count. The count covers all
    // MIR locals plus reserved locals (frame ptr at 252).
    let max_mir_local = func.locals.iter().map(|l| l.id.0).max().unwrap_or(0);
    let local_count = (pc + max_mir_local + 1).max(FRAME_PTR_VAR.0 + 1);
    let locals: Vec<CirLocal> = (0..local_count)
        .map(|i| CirLocal { id: VarId(i) })
        .collect();

    CirFunction {
        name: func.name.clone(),
        locals,
        blocks: cir_blocks,
        entry_block: BlockId(
            block_map
                .get(&func.entry.0)
                .copied()
                .unwrap_or(func.entry.0),
        ),
    }
}

// ---------------------------------------------------------------------------
// Statement translation
// ---------------------------------------------------------------------------

fn translate_stmt(stmt: &Stmt, func: &mir::Function, pc: u32) -> CirStmt {
    match stmt {
        Stmt::Assign { dst, op } => match op {
            // Suspending rvalues are split out before this point.
            op if is_suspending_rvalue(op) => CirStmt::Assign {
                dst: var(dst, pc),
                src: CirExpr::ConstNil,
            },
            RValue::Perform {
                effect, op, args, ..
            } => match (effect.as_str(), op.as_str()) {
                ("IO", "print") | ("IO", "println") => CirStmt::Assign {
                    dst: var(dst, pc),
                    src: CirExpr::Call {
                        func_idx: IMPORT_IO_PRINT,
                        args: args.iter().map(|a| CirExpr::Var(var(a, pc))).collect(),
                    },
                },
                ("IO", "read") => CirStmt::Assign {
                    dst: var(dst, pc),
                    src: CirExpr::Call {
                        func_idx: IMPORT_IO_READ,
                        args: Vec::new(),
                    },
                },
                ("Array", "length") => {
                    if let Some(arr) = args.first() {
                        CirStmt::Assign {
                            dst: var(dst, pc),
                            src: CirExpr::ArrayLen {
                                arr: Box::new(CirExpr::Var(var(arr, pc))),
                            },
                        }
                    } else {
                        CirStmt::Assign {
                            dst: var(dst, pc),
                            src: CirExpr::ConstNil,
                        }
                    }
                }
                _ => CirStmt::Assign {
                    dst: var(dst, pc),
                    src: CirExpr::ConstNil,
                },
            },
            RValue::Send { actor, args, .. } => CirStmt::Emit {
                effect: EffectKind::ActorSend,
                args: std::iter::once(CirExpr::Var(var(actor, pc)))
                    .chain(args.iter().map(|a| CirExpr::Var(var(a, pc))))
                    .collect(),
            },
            _ => CirStmt::Assign {
                dst: var(dst, pc),
                src: translate_rvalue(op, func, pc),
            },
        },
        Stmt::EnterHandle { .. } | Stmt::PopHandler => {
            // User-defined handler frames: dispatch is deferred to a follow-up
            // (the plan's MVP scope). No observable effect in CIR.
            CirStmt::Assign {
                dst: FRAME_PTR_VAR,
                src: CirExpr::ConstNil,
            }
        }
        Stmt::ArrayStore { .. } | Stmt::StoreFieldNamed { .. } => CirStmt::Assign {
            dst: FRAME_PTR_VAR,
            src: CirExpr::ConstNil,
        },
        Stmt::Emit { event, args } => CirStmt::Emit {
            effect: EffectKind::HostEffect {
                module: "Event".into(),
                name: event.clone(),
            },
            args: args.iter().map(|a| CirExpr::Var(var(a, pc))).collect(),
        },
        Stmt::StateSet { src, .. } => CirStmt::Emit {
            effect: EffectKind::HostEffect {
                module: "Actor".into(),
                name: "state_set".into(),
            },
            args: vec![CirExpr::Var(var(src, pc))],
        },
    }
}

fn translate_rvalue(op: &RValue, _func: &mir::Function, pc: u32) -> CirExpr {
    match op {
        RValue::Const(c) => translate_const(c),
        RValue::Load(l) => CirExpr::Var(var(l, pc)),
        RValue::Binary(bin, a, b) => CirExpr::BinaryOp {
            op: translate_binop(bin),
            lhs: Box::new(CirExpr::Var(var(a, pc))),
            rhs: Box::new(CirExpr::Var(var(b, pc))),
        },
        RValue::Unary(un, a) => CirExpr::UnaryOp {
            op: translate_unop(un),
            operand: Box::new(CirExpr::Var(var(a, pc))),
        },
        RValue::Call {
            func: mir::FuncRef::Index(idx),
            args,
        } => CirExpr::Call {
            func_idx: WASM_FUNC_IMPORT_COUNT + *idx as u32,
            args: args.iter().map(|a| CirExpr::Var(var(a, pc))).collect(),
        },
        RValue::Call {
            func: mir::FuncRef::Local(_),
            ..
        } => CirExpr::ConstNil, // closures unsupported, same as mir_wasm
        RValue::ArrayLen(arr) => CirExpr::ArrayLen {
            arr: Box::new(CirExpr::Var(var(arr, pc))),
        },
        RValue::ArrayLoad { arr, idx } => CirExpr::ArrayLoad {
            arr: Box::new(CirExpr::Var(var(arr, pc))),
            idx: Box::new(CirExpr::Var(var(idx, pc))),
        },
        RValue::StateGet { .. } => CirExpr::ConstNil, // host-managed, MVP stub
        // Unsupported in the WasmFX MVP (mir_wasm falls through to nil too):
        RValue::Perform { .. }
        | RValue::PerformAsync { .. }
        | RValue::SignalWait { .. }
        | RValue::Receive { .. }
        | RValue::ReceiveMatch { .. }
        | RValue::ReceiveWait { .. }
        | RValue::ReceiveCommit
        | RValue::Send { .. }
        | RValue::StrConcat(..)
        | RValue::StringEq(..)
        | RValue::LoadFieldNamed { .. }
        | RValue::LoadFieldPos { .. }
        | RValue::Tuple(..)
        | RValue::Record(..)
        | RValue::RecordUpdate { .. }
        | RValue::ArrayLit(..)
        | RValue::Closure { .. }
        | RValue::Spawn { .. }
        | RValue::Ask { .. }
        | RValue::FFICall { .. }
        | RValue::Migrate { .. }
        | RValue::SelfRef
        | RValue::CapabilityCheck { .. }
        | RValue::Resume(..)
        | RValue::Panic(..) => CirExpr::ConstNil,
    }
}

fn translate_const(c: &crate::bytecode::Constant) -> CirExpr {
    use crate::bytecode::Constant;
    match c {
        Constant::Int(n) => CirExpr::ConstI64(value_layout::tag_int(*n) as i64),
        Constant::Float(f) => CirExpr::ConstF64(*f),
        Constant::Bool(b) => CirExpr::ConstBool(*b),
        Constant::Nil => CirExpr::ConstNil,
        Constant::Unit => CirExpr::ConstUnit,
        Constant::String(s) => CirExpr::ConstString(s.clone()),
        _ => CirExpr::ConstNil,
    }
}

fn translate_binop(op: &crate::ast::BinOp) -> BinaryOp {
    use crate::ast::BinOp;
    match op {
        BinOp::Add => BinaryOp::Add,
        BinOp::Sub => BinaryOp::Sub,
        BinOp::Mul => BinaryOp::Mul,
        BinOp::Div => BinaryOp::Div,
        BinOp::Mod => BinaryOp::Mod,
        BinOp::Eq => BinaryOp::Eq,
        BinOp::Ne => BinaryOp::Neq,
        BinOp::Lt => BinaryOp::Lt,
        BinOp::Le => BinaryOp::Lte,
        BinOp::Gt => BinaryOp::Gt,
        BinOp::Ge => BinaryOp::Gte,
        BinOp::And => BinaryOp::And,
        BinOp::Or => BinaryOp::Or,
        _ => BinaryOp::Add, // unreachable for lowered code; keep total
    }
}

fn translate_unop(op: &crate::ast::UnOp) -> UnaryOp {
    use crate::ast::UnOp;
    match op {
        UnOp::Neg => UnaryOp::Neg,
        UnOp::Not => UnaryOp::Not,
        _ => UnaryOp::Not,
    }
}

fn translate_terminator(term: &Terminator, _func: &mir::Function, pc: u32) -> CirTerminator {
    match term {
        Terminator::Return(Some(l)) => CirTerminator::Return(Some(CirExpr::Var(var(l, pc)))),
        Terminator::Return(None) => CirTerminator::Return(None),
        Terminator::Jump(t) => CirTerminator::Jump(BlockId(t.0)),
        Terminator::Branch { cond, then_, else_ } => CirTerminator::Branch {
            cond: CirExpr::Var(var(cond, pc)),
            then_block: BlockId(then_.0),
            else_block: BlockId(else_.0),
        },
        Terminator::Resume(l) => CirTerminator::Resume(CirExpr::Var(var(l, pc))),
        Terminator::Unterminated => CirTerminator::Return(None),
    }
}

// ---------------------------------------------------------------------------
// Suspension lowering
// ---------------------------------------------------------------------------

/// Compute the effect kind and suspend payload args for a suspending rvalue.
fn suspend_effect_and_args(op: &RValue, pc: u32) -> (EffectKind, Vec<CirExpr>) {
    match op {
        RValue::Perform {
            effect, op, args, ..
        } if effect == "LLM" && op == "ask" => (
            EffectKind::LlmAsk,
            args.iter().map(|a| CirExpr::Var(var(a, pc))).collect(),
        ),
        RValue::SignalWait { name } => (
            EffectKind::SignalWait,
            vec![CirExpr::ConstString(name.clone())],
        ),
        RValue::ReceiveWait {
            timeout,
            max_params,
            ..
        } => (
            EffectKind::MailboxDequeue,
            vec![
                CirExpr::ConstI64(*max_params as i64),
                CirExpr::Var(var(timeout, pc)),
            ],
        ),
        RValue::ReceiveMatch { max_params, .. } => (
            EffectKind::MailboxDequeue,
            vec![CirExpr::ConstI64(*max_params as i64)],
        ),
        RValue::PerformAsync {
            effect_op, args, ..
        } => (
            EffectKind::PerformAsync,
            std::iter::once(CirExpr::ConstString(effect_op.clone()))
                .chain(args.iter().map(|a| CirExpr::Var(var(a, pc))))
                .collect(),
        ),
        _ => (
            EffectKind::HostEffect {
                module: "__cir".into(),
                name: "unknown_suspend".into(),
            },
            Vec::new(),
        ),
    }
}

/// Map a MIR local to the flat Wasm local index (CIR VarId).
fn var(local: &LocalId, pc: u32) -> VarId {
    VarId(pc + local.0)
}

// Import indices mirrored from mir_wasm.rs (the WasmFX backend reuses the
// same host-import table).
const IMPORT_IO_PRINT: u32 = 3;
const IMPORT_IO_READ: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cir::CirTerminator;
    use crate::mir::{FunctionBuilder, LocalId, RValue, Terminator};
    use crate::types::{PrimitiveType, Type};
    /// Build a MIR function that adds two int params.
    fn build_add_function() -> mir::Function {
        let mut b = FunctionBuilder::new("add", Some(Type::Primitive(PrimitiveType::Int)));
        let a = b.add_param("a", Type::Primitive(PrimitiveType::Int));
        let b_id = b.add_param("b", Type::Primitive(PrimitiveType::Int));
        let sum = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(sum, RValue::Binary(crate::ast::BinOp::Add, a, b_id));
        b.terminate(Terminator::Return(Some(sum)));
        b.build()
    }

    fn build_llm_ask_function() -> mir::Function {
        let mut b = FunctionBuilder::new("ask_llm", Some(Type::Primitive(PrimitiveType::String)));
        let prompt = b.add_temp(Type::Primitive(PrimitiveType::String));
        let result = b.add_temp(Type::Primitive(PrimitiveType::String));
        b.assign(
            prompt,
            RValue::Const(crate::bytecode::Constant::String("test".into())),
        );
        b.assign(
            result,
            RValue::Perform {
                effect: "LLM".into(),
                op: "ask".into(),
                args: vec![prompt],
                resolved_handler: None,
            },
        );
        b.terminate(Terminator::Return(Some(result)));
        b.build()
    }

    /// Build a MIR function with `SignalWait`.
    fn build_signal_wait_function() -> mir::Function {
        let mut b = FunctionBuilder::new("wait_signal", Some(Type::Primitive(PrimitiveType::Unit)));
        let result = b.add_temp(Type::Primitive(PrimitiveType::Unit));
        b.assign(
            result,
            RValue::SignalWait {
                name: "tick".into(),
            },
        );
        b.terminate(Terminator::Return(Some(result)));
        b.build()
    }

    /// Build a MIR function with `ReceiveWait`.
    fn build_receive_wait_function() -> mir::Function {
        let mut b = FunctionBuilder::new("wait_msg", Some(Type::Primitive(PrimitiveType::Unit)));
        let timeout = b.add_temp(Type::Primitive(PrimitiveType::Int));
        b.assign(timeout, RValue::Const(crate::bytecode::Constant::Int(5000)));
        let result = b.add_temp(Type::Primitive(PrimitiveType::Unit));
        b.assign(
            result,
            RValue::ReceiveWait {
                behavior_ids: vec![],
                max_params: 4,
                timeout,
            },
        );
        b.terminate(Terminator::Return(Some(result)));
        b.build()
    }

    #[test]
    fn test_has_suspension_no_suspend() {
        let func = build_add_function();
        assert!(!has_suspension(&func), "add should not suspend");
    }

    #[test]
    fn test_has_suspension_llm_ask() {
        let func = build_llm_ask_function();
        assert!(has_suspension(&func), "LLM.ask should suspend");
    }

    #[test]
    fn test_has_suspension_signal_wait() {
        let func = build_signal_wait_function();
        assert!(has_suspension(&func), "SignalWait should suspend");
    }

    #[test]
    fn test_has_suspension_receive_wait() {
        let func = build_receive_wait_function();
        assert!(has_suspension(&func), "ReceiveWait should suspend");
    }

    #[test]
    fn test_is_suspending_rvalue_llm_ask() {
        let rv = RValue::Perform {
            effect: "LLM".into(),
            op: "ask".into(),
            args: vec![LocalId(0)],
            resolved_handler: None,
        };
        assert!(is_suspending_rvalue(&rv));
    }

    #[test]
    fn test_is_suspending_rvalue_add_not() {
        let rv = RValue::Binary(crate::ast::BinOp::Add, LocalId(0), LocalId(1));
        assert!(!is_suspending_rvalue(&rv));
    }

    #[test]
    fn test_lower_non_suspending_produces_clean_cir() {
        let func = build_add_function();
        let cir = lower_mir_function_unconditional(&func);
        // Entry block should have no SuspendAndYield
        assert!(!cir.blocks.is_empty());
        for block in &cir.blocks {
            assert!(
                !matches!(block.terminator, CirTerminator::SuspendAndYield { .. }),
                "non-suspending function should have no SuspendAndYield"
            );
        }
    }

    #[test]
    fn test_lower_suspending_splits_block() {
        let func = build_llm_ask_function();
        let cir = lower_mir_function_unconditional(&func);
        // Should have at least 2 blocks: pre-suspend + resume
        assert!(
            cir.blocks.len() >= 2,
            "suspending function should have >=2 CIR blocks, got {}",
            cir.blocks.len()
        );
        let has_suspend = cir
            .blocks
            .iter()
            .any(|b| matches!(b.terminator, CirTerminator::SuspendAndYield { .. }));
        assert!(has_suspend, "suspending function must have SuspendAndYield");
    }

    #[test]
    fn test_var_mapping() {
        // pc=2 (two params), local.0=0 => VarId(2)
        assert_eq!(var(&LocalId(0), 2), VarId(2));
        // pc=2 (two params), local.0=1 => VarId(3)
        assert_eq!(var(&LocalId(1), 2), VarId(3));
        // pc=0, local.0=0 => VarId(0)
        assert_eq!(var(&LocalId(0), 0), VarId(0));
    }
}
