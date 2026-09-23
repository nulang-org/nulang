//! WasmFX backend: compiles MIR to WebAssembly with stack-switching
//! (WasmFX proposal) instructions for suspending effects.
//!
//! Every MIR function is lowered through CIR ([`crate::cir`],
//! [`crate::cir_lower`]) and compiled to a Wasm function with the CPS
//! state-machine structure of `mir_wasm.rs` (a `Loop` with one `Block` per
//! CIR block and a `BrTable` dispatcher). Suspending functions additionally
//! get a shared *resume function* per function: each `SuspendAndYield`
//! saves live variables into a frame in linear memory, creates a
//! continuation wrapping the resume function (`cont.new`), pre-binds the
//! frame pointer (`cont.bind`), and yields to the host (`suspend $tag`).
//! The host later resumes the continuation with the effect result; the
//! resume function reads the resume block id from the frame header,
//! dispatches to it, restores live variables, and continues the
//! computation.
//!
//! Module structure mirrors `mir_wasm.rs` (same import table, memory,
//! string interning, and `nulang_init` export) plus tag imports for the
//! suspension effect kinds. The tag imports are only emitted when at least
//! one function actually suspends.

use crate::cir::{BinaryOp, CirExpr, CirFunction, CirStmt, CirTerminator, EffectKind, UnaryOp};
use crate::cir_analysis;
use crate::cir_lower;
use crate::mir;
use crate::types::NuResult;
use crate::value_layout;
use std::collections::HashMap;
use wasm_encoder::*;

// ── Import / type index constants ───────────────────────────────────────
// Function imports count separately from tag imports; module functions
// start at FUNC_IMPORT_COUNT.

const IMPORT_ALLOC_IDX: u32 = 0;
const IMPORT_EMIT: u32 = 5;
const FUNC_IMPORT_COUNT: u32 = 6;

/// Tag import indices (tag index space, in import order).
const TAG_LLM_ASK: u32 = 0;
const TAG_SIGNAL_WAIT: u32 = 1;
const TAG_MAILBOX_DEQUEUE: u32 = 2;
const TAG_PERFORM_ASYNC: u32 = 3;
const TAG_HOST_EFFECT: u32 = 4;

// Fixed type indices (mirrors mir_wasm.rs for 0..3).
const TY_VOID_TO_I64: u32 = 0;
const TY_I64_TO_I64: u32 = 1;
const TY_I64I64_TO_I64: u32 = 2;
const TY_I32I32_TO_I64: u32 = 3;
const TY_I64_TO_VOID: u32 = 4; // tag payload: (i64) -> ()
const TY_CONT_FULL: u32 = 5; // cont (param i64 i64) (result i64)
const TY_CONT_BOUND: u32 = 6; // cont (param i64) (result i64)
const TY_FIXED_COUNT: u32 = 7;

/// Dispatcher state local (mirrors mir_wasm.rs).
const STATE_LOCAL: u32 = 251;
/// Frame-pointer local (matches cir_lower::FRAME_PTR_VAR).
const FRAME_PTR_LOCAL: u32 = 252;
/// Local receiving the host-provided resume value (matches cir_lower::RESULT_VAR).
const RESULT_LOCAL: u32 = 253;
/// Scratch locals for binop/unop/emit computations.
const SCRATCH_A: u32 = 254;
const SCRATCH_B: u32 = 255;
// ── WasmFxBackend ──────────────────────────────────────────────────────

pub struct WasmFxBackend {
    types: TypeSection,
    imports: ImportSection,
    functions: FunctionSection,
    exports: ExportSection,
    codes: CodeSection,
    data: DataSection,
    /// Accumulated data-segment bytes for interned strings.
    string_data: Vec<u8>,
    /// String content → (offset in data segment, length).
    interned: HashMap<String, (u32, u32)>,
    /// MIR function index → Wasm function index.
    func_index_map: HashMap<usize, u32>,
    /// Module function param counts → type index (mirrors mir_wasm.rs).
    func_types: HashMap<Vec<ValType>, u32>,
    next_type_idx: u32,
    next_func_idx: u32,
    /// Wasm function index of the shared resume function, per CIR function
    /// wasm index.
    resume_func_of: HashMap<u32, u32>,
    /// Whether any function contains a suspension point (drives tag imports).
    any_suspension: bool,
    /// CIR plans for module functions and behaviors, in wasm index order.
    cir_plans: Vec<(u32, CirFunction, bool)>, // (wasm_idx, cir, has_suspension)
    /// Resume functions to emit, in wasm index order.
    resume_plans: Vec<(u32, CirFunction)>,
}

impl WasmFxBackend {
    pub fn new() -> Self {
        let mut types = TypeSection::new();
        types.ty().function([], [ValType::I64]); // 0
        types.ty().function([ValType::I64], [ValType::I64]); // 1
        types
            .ty()
            .function([ValType::I64, ValType::I64], [ValType::I64]); // 2
        types
            .ty()
            .function([ValType::I32, ValType::I32], [ValType::I64]); // 3
        types.ty().function([ValType::I64], []); // 4 — tag payload type
                                                 // Continuation types (wasm-encoder 0.220 exposes these only through
                                                 // the subtype path on the core-type encoder).
        types.ty().subtype(&SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Cont(ContType(TY_I64I64_TO_I64)), // 5
                shared: false,
            },
        });
        types.ty().subtype(&SubType {
            is_final: true,
            supertype_idx: None,
            composite_type: CompositeType {
                inner: CompositeInnerType::Cont(ContType(TY_I64_TO_I64)), // 6
                shared: false,
            },
        });

        WasmFxBackend {
            types,
            imports: ImportSection::new(),
            functions: FunctionSection::new(),
            exports: ExportSection::new(),
            codes: CodeSection::new(),
            data: DataSection::new(),
            string_data: Vec::new(),
            interned: HashMap::new(),
            func_index_map: HashMap::new(),
            func_types: HashMap::new(),
            next_type_idx: TY_FIXED_COUNT,
            next_func_idx: FUNC_IMPORT_COUNT,
            resume_func_of: HashMap::new(),
            any_suspension: false,
            cir_plans: Vec::new(),
            resume_plans: Vec::new(),
        }
    }

    // ── Compile ───────────────────────────────────────────────────

    pub fn compile(&mut self, mir_module: &mir::Module, _module_name: &str) -> NuResult<Vec<u8>> {
        // Pre-scan: closures and user-defined handler/resume semantics are
        // unsupported. WasmFX suspension for built-in async effects is real,
        // but user-defined effect-handler Resume is still an MVP stub in CIR
        // codegen and must never be silently reinterpreted as `nil`.
        for func in mir_module
            .functions
            .iter()
            .chain(mir_module.behaviors.iter())
        {
            for block in &func.blocks {
                if matches!(&block.terminator, mir::Terminator::Resume(_)) {
                    return Err(crate::types::NuError::VMError {
                        msg: "WasmFX backend restricted profile: user-defined effect handlers and continuation resume are not supported yet; use the bytecode backend"
                            .into(),
                        span: crate::types::Span::default(),
                    });
                }

                for stmt in &block.stmts {
                    if matches!(stmt, mir::Stmt::EnterHandle { .. } | mir::Stmt::PopHandler)
                        || matches!(
                            stmt,
                            mir::Stmt::Assign {
                                op: mir::RValue::Resume(..),
                                ..
                            }
                        )
                    {
                        return Err(crate::types::NuError::VMError {
                            msg: "WasmFX backend restricted profile: user-defined effect handlers and continuation resume are not supported yet; use the bytecode backend"
                                .into(),
                            span: crate::types::Span::default(),
                        });
                    }

                    if let mir::Stmt::Assign { op, .. } = stmt {
                        if let mir::RValue::Call {
                            func: mir::FuncRef::Local(_),
                            ..
                        } = op
                        {
                            return Err(crate::types::NuError::VMError {
                                msg: "WasmFX backend does not support closures (FuncRef::Local)"
                                    .into(),
                                span: crate::types::Span::default(),
                            });
                        }
                    }
                }
            }
        }

        // Phase 1a: lower all functions + behaviors to CIR.
        let mut cir_of: Vec<(usize, CirFunction, bool)> = Vec::new();
        for (idx, func) in mir_module.functions.iter().enumerate() {
            let cir = cir_lower::lower_mir_function_unconditional(func);
            let susp = cir_lower::has_suspension(func);
            self.any_suspension |= susp;
            cir_of.push((idx, cir, susp));
        }
        for (idx, func) in mir_module.behaviors.iter().enumerate() {
            let mir_idx = mir_module.functions.len() + idx;
            let cir = cir_lower::lower_mir_function_unconditional(func);
            let susp = cir_lower::has_suspension(func);
            self.any_suspension |= susp;
            cir_of.push((mir_idx, cir, susp));
        }

        // Run live-variable analysis on every CIR function (no-op for
        // non-suspending ones).
        for (_, cir, _) in &mut cir_of {
            cir_analysis::compute_live_vars(cir);
        }
        // Intern strings referenced by CIR expressions.
        for (_, cir, _) in &cir_of {
            self.intern_cir_strings(cir);
        }

        // Phase 1b: register function types.
        for func in &mir_module.functions {
            self.register_function_type(func);
        }
        for func in &mir_module.behaviors {
            self.register_function_type(func);
        }

        // Phase 1c: rebuild imports (tags only when something suspends).
        self.rebuild_imports();

        // Phase 1d: assign wasm function indices.
        for (i, _) in mir_module.functions.iter().enumerate() {
            let wasm_idx = self.next_func_idx;
            self.next_func_idx += 1;
            self.func_index_map.insert(i, wasm_idx);
            self.functions
                .function(self.func_type_idx(&mir_module.functions[i]));
        }
        for (i, _) in mir_module.behaviors.iter().enumerate() {
            let wasm_idx = self.next_func_idx;
            self.next_func_idx += 1;
            self.func_index_map
                .insert(mir_module.functions.len() + i, wasm_idx);
            self.functions
                .function(self.func_type_idx(&mir_module.behaviors[i]));
        }
        for (mir_idx, cir, susp) in &cir_of {
            let wasm_idx = self.func_index_map[mir_idx];
            self.cir_plans.push((wasm_idx, cir.clone(), *susp));
            if *susp {
                let resume_idx = self.next_func_idx;
                self.next_func_idx += 1;
                self.resume_func_of.insert(wasm_idx, resume_idx);
                self.functions.function(TY_I64I64_TO_I64);
                self.resume_plans.push((resume_idx, cir.clone()));
            }
        }

        // Phase 2: emit code bodies in function index order.
        let mut bodies: Vec<(u32, Function)> = Vec::new();
        // Module functions + behaviors first.
        for (wasm_idx, cir, susp) in &self.cir_plans {
            let body = self.build_cir_body(cir, *susp, *wasm_idx, false);
            bodies.push((*wasm_idx, body));
        }
        // Resume functions.
        for (resume_idx, cir) in &self.resume_plans {
            let body = self.build_cir_body(cir, true, *resume_idx, true);
            bodies.push((*resume_idx, body));
        }
        bodies.sort_by_key(|(idx, _)| *idx);
        for (_, body) in bodies {
            self.codes.function(&body);
        }

        if !mir_module.functions.is_empty() {
            let main_idx = FUNC_IMPORT_COUNT + mir_module.functions.len() as u32 - 1;
            self.exports
                .export("nulang_init", ExportKind::Func, main_idx);
        }

        // Emit data segment.
        if !self.string_data.is_empty() {
            self.data
                .active(0, &ConstExpr::i32_const(0), self.string_data.clone());
        }

        // Build module.
        let mut module = Module::new();
        module.section(&self.types);
        module.section(&self.imports);
        module.section(&self.functions);
        module.section(&self.exports);
        module.section(&self.codes);
        module.section(&self.data);
        Ok(module.finish())
    }

    fn intern_cir_strings(&mut self, cir: &CirFunction) {
        for block in &cir.blocks {
            for stmt in &block.stmts {
                match stmt {
                    CirStmt::Assign { src, .. } => self.intern_expr_strings(src),
                    CirStmt::Emit { args, .. } => {
                        for a in args {
                            self.intern_expr_strings(a);
                        }
                    }
                    _ => {}
                }
            }
            match &block.terminator {
                CirTerminator::Return(Some(e))
                | CirTerminator::Resume(e)
                | CirTerminator::Branch { cond: e, .. } => self.intern_expr_strings(e),
                CirTerminator::SuspendAndYield { args, .. } => {
                    for a in args {
                        self.intern_expr_strings(a);
                    }
                }
                _ => {}
            }
        }
    }

    fn intern_expr_strings(&mut self, e: &CirExpr) {
        match e {
            CirExpr::ConstString(s) => {
                self.intern_string(s);
            }
            CirExpr::BinaryOp { lhs, rhs, .. } => {
                self.intern_expr_strings(lhs);
                self.intern_expr_strings(rhs);
            }
            CirExpr::UnaryOp { operand, .. } => self.intern_expr_strings(operand),
            CirExpr::Call { args, .. } => {
                for a in args {
                    self.intern_expr_strings(a);
                }
            }
            CirExpr::ArrayLen { arr } | CirExpr::ArrayLoad { arr, .. } => {
                self.intern_expr_strings(arr);
            }
            _ => {}
        }
    }

    /// Intern a string into the data segment. Returns (offset, len).
    fn intern_string(&mut self, s: &str) -> (u32, u32) {
        if let Some(&entry) = self.interned.get(s) {
            return entry;
        }
        let offset = self.string_data.len() as u32;
        let len = s.len() as u32;
        self.string_data.extend_from_slice(s.as_bytes());
        self.interned.insert(s.to_string(), (offset, len));
        (offset, len)
    }

    fn rebuild_imports(&mut self) {
        // Function import types.
        let ty_alloc = self.ensure_type(vec![ValType::I32], vec![ValType::I32]);
        let ty_dispatch = self.ensure_type(vec![ValType::I32; 4], vec![ValType::I64]);
        let ty_emit = self.ensure_type(vec![ValType::I32, ValType::I32], vec![ValType::I64]);

        let mut imports = ImportSection::new();
        imports.import(
            "env",
            "memory",
            MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            },
        );
        imports.import("env", "nulang_alloc", EntityType::Function(ty_alloc));
        imports.import("env", "nulang_dispatch", EntityType::Function(ty_dispatch));
        imports.import("env", "log", EntityType::Function(TY_I32I32_TO_I64));
        imports.import("env", "io_print", EntityType::Function(TY_I32I32_TO_I64));
        imports.import("env", "io_read", EntityType::Function(TY_VOID_TO_I64));
        imports.import("env", "nulang_emit", EntityType::Function(ty_emit));

        // Tag imports — only when the module contains suspension points.
        if self.any_suspension {
            let tag_ty = TagType {
                kind: TagKind::Exception,
                func_type_idx: TY_I64_TO_VOID,
            };
            for (name, _idx) in [
                ("tag_llm_ask", TAG_LLM_ASK),
                ("tag_signal_wait", TAG_SIGNAL_WAIT),
                ("tag_mailbox_dequeue", TAG_MAILBOX_DEQUEUE),
                ("tag_perform_async", TAG_PERFORM_ASYNC),
                ("tag_host_effect", TAG_HOST_EFFECT),
            ] {
                imports.import("env", name, EntityType::Tag(tag_ty));
            }
        }
        self.imports = imports;
    }

    fn ensure_type(&mut self, params: Vec<ValType>, results: Vec<ValType>) -> u32 {
        let idx = self.next_type_idx;
        self.next_type_idx += 1;
        if results.is_empty() {
            self.types.ty().function(params, []);
        } else {
            self.types.ty().function(params, results);
        }
        idx
    }

    // ── Function type registration ─────────────────────────────────

    fn register_function_type(&mut self, func: &mir::Function) {
        let count = func.params.len() + func.captures.len();
        let param_types: Vec<ValType> = vec![ValType::I64; count];
        if self.func_types.contains_key(&param_types) {
            return;
        }
        let type_idx = self.next_type_idx;
        self.next_type_idx += 1;
        self.func_types.insert(param_types.clone(), type_idx);
        if param_types.is_empty() {
            self.types.ty().function([], [ValType::I64]);
        } else {
            self.types.ty().function(param_types, [ValType::I64]);
        }
    }

    fn func_type_idx(&self, func: &mir::Function) -> u32 {
        let count = func.params.len() + func.captures.len();
        let param_types: Vec<ValType> = vec![ValType::I64; count];
        self.func_types.get(&param_types).copied().unwrap_or(0)
    }

    // ── CIR body construction ──────────────────────────────────────

    /// Build a Wasm function body for a CIR function.
    ///
    /// `resume_mode: true` compiles the shared resume function: the frame
    /// pointer and host result arrive as parameters 0 and 1, and the
    /// dispatcher's initial state is read from the frame header.
    fn build_cir_body(
        &self,
        cir: &CirFunction,
        _susp: bool,
        own_idx: u32,
        resume_mode: bool,
    ) -> Function {
        let wasm_locals: Vec<(u32, ValType)> = vec![(256, ValType::I64)];
        let mut body = Function::new(wasm_locals);

        let n = cir.blocks.len();
        let state_local = STATE_LOCAL;

        if resume_mode {
            // Prologue: stash params into reserved locals, load resume state
            // from the frame header.
            body.instruction(&Instruction::LocalGet(0));
            body.instruction(&Instruction::LocalSet(FRAME_PTR_LOCAL));
            body.instruction(&Instruction::LocalGet(1));
            body.instruction(&Instruction::LocalSet(RESULT_LOCAL));
            body.instruction(&Instruction::LocalGet(FRAME_PTR_LOCAL));
            body.instruction(&Instruction::I32WrapI64);
            body.instruction(&Instruction::I32Load(MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            body.instruction(&Instruction::I64ExtendI32U);
            body.instruction(&Instruction::LocalSet(state_local));
        } else {
            body.instruction(&Instruction::I64Const(cir.entry_block.0 as i64));
            body.instruction(&Instruction::LocalSet(state_local));
        }

        body.instruction(&Instruction::Loop(BlockType::Empty));
        for _ in 0..n {
            body.instruction(&Instruction::Block(BlockType::Empty));
        }
        body.instruction(&Instruction::LocalGet(state_local));
        body.instruction(&Instruction::I32WrapI64);
        let targets: Vec<u32> = (0..n as u32).collect();
        body.instruction(&Instruction::BrTable(
            std::borrow::Cow::Owned(targets.clone()),
            targets.last().copied().unwrap_or(0),
        ));

        for li in 0..n {
            body.instruction(&Instruction::End); // end block
            let block = &cir.blocks[li];
            for stmt in &block.stmts {
                self.compile_cir_stmt(&mut body, stmt, cir);
            }
            self.compile_cir_terminator(
                &mut body,
                &block.terminator,
                li as u32,
                n as u32,
                cir,
                own_idx,
            );
        }

        body.instruction(&Instruction::End); // end Loop
        body.instruction(&Instruction::I64Const(value_layout::TAG_NIL as i64));
        body.instruction(&Instruction::End); // function end
        body
    }

    // ── CIR statement codegen ──────────────────────────────────────

    fn compile_cir_stmt(&self, body: &mut Function, stmt: &CirStmt, cir: &CirFunction) {
        match stmt {
            CirStmt::Assign { dst, src } => {
                self.compile_cir_expr(body, src, cir);
                body.instruction(&Instruction::LocalSet(dst.0));
            }
            CirStmt::Emit { args, .. } => {
                self.compile_emit(body, args, cir);
            }
            CirStmt::SaveFrame {
                vars,
                offsets,
                frame_ptr,
            } => {
                // Allocate the frame: 16-byte header + 8 bytes per live var.
                let total = crate::cir::FRAME_HEADER_SIZE + vars.len() * 8;
                body.instruction(&Instruction::I32Const(total as i32));
                body.instruction(&Instruction::Call(IMPORT_ALLOC_IDX));
                body.instruction(&Instruction::I64ExtendI32U);
                body.instruction(&Instruction::LocalSet(frame_ptr.0));
                for (var, offset) in vars.iter().zip(offsets.iter()) {
                    body.instruction(&Instruction::LocalGet(frame_ptr.0));
                    body.instruction(&Instruction::I64Const(*offset as i64));
                    body.instruction(&Instruction::I64Add);
                    body.instruction(&Instruction::I32WrapI64);
                    body.instruction(&Instruction::LocalGet(var.0));
                    body.instruction(&Instruction::I64Store(MemArg {
                        offset: 0,
                        align: 3,
                        memory_index: 0,
                    }));
                }
            }
            CirStmt::RestoreFrame {
                vars,
                offsets,
                frame_ptr,
            } => {
                for (var, offset) in vars.iter().zip(offsets.iter()) {
                    body.instruction(&Instruction::LocalGet(frame_ptr.0));
                    body.instruction(&Instruction::I64Const(*offset as i64));
                    body.instruction(&Instruction::I64Add);
                    body.instruction(&Instruction::I32WrapI64);
                    body.instruction(&Instruction::I64Load(MemArg {
                        offset: 0,
                        align: 3,
                        memory_index: 0,
                    }));
                    body.instruction(&Instruction::LocalSet(var.0));
                }
            }
        }
    }

    /// Fire-and-forget effect: pack args into a frame, call `nulang_emit`.
    fn compile_emit(&self, body: &mut Function, args: &[CirExpr], cir: &CirFunction) {
        let total = crate::cir::FRAME_HEADER_SIZE + args.len() * 8;
        body.instruction(&Instruction::I32Const(total as i32));
        body.instruction(&Instruction::Call(IMPORT_ALLOC_IDX));
        body.instruction(&Instruction::I64ExtendI32U);
        body.instruction(&Instruction::LocalSet(SCRATCH_B));
        for (i, arg) in args.iter().enumerate() {
            let offset = crate::cir::FRAME_HEADER_SIZE + i * 8;
            body.instruction(&Instruction::LocalGet(SCRATCH_B));
            body.instruction(&Instruction::I64Const(offset as i64));
            body.instruction(&Instruction::I64Add);
            body.instruction(&Instruction::I32WrapI64);
            self.compile_cir_expr(body, arg, cir);
            body.instruction(&Instruction::I64Store(MemArg {
                offset: 0,
                align: 3,
                memory_index: 0,
            }));
        }
        body.instruction(&Instruction::LocalGet(SCRATCH_B));
        body.instruction(&Instruction::I32WrapI64);
        body.instruction(&Instruction::I32Const(args.len() as i32));
        body.instruction(&Instruction::Call(IMPORT_EMIT));
        body.instruction(&Instruction::Drop);
    }

    // ── CIR terminator codegen ─────────────────────────────────────

    fn compile_cir_terminator(
        &self,
        body: &mut Function,
        term: &CirTerminator,
        li: u32,
        n: u32,
        cir: &CirFunction,
        own_idx: u32,
    ) {
        match term {
            CirTerminator::Return(Some(e)) => {
                self.compile_cir_expr(body, e, cir);
                body.instruction(&Instruction::Return);
            }
            CirTerminator::Return(None) => {
                body.instruction(&Instruction::I64Const(value_layout::TAG_UNIT as i64));
                body.instruction(&Instruction::Return);
            }
            CirTerminator::Jump(t) => {
                let tl = t.0;
                if tl > li {
                    body.instruction(&Instruction::Br(tl - li - 1));
                } else {
                    body.instruction(&Instruction::I64Const(tl as i64));
                    body.instruction(&Instruction::LocalSet(STATE_LOCAL));
                    body.instruction(&Instruction::Br(n - 1 - li));
                }
            }
            CirTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                self.compile_cir_expr(body, cond, cir);
                body.instruction(&Instruction::I64Const(value_layout::tag_bool(false) as i64));
                body.instruction(&Instruction::I64Ne);
                body.instruction(&Instruction::If(BlockType::Empty));

                let tl = then_block.0;
                if tl > li {
                    body.instruction(&Instruction::Br(tl - li));
                } else {
                    body.instruction(&Instruction::I64Const(tl as i64));
                    body.instruction(&Instruction::LocalSet(STATE_LOCAL));
                    body.instruction(&Instruction::Br(n - li));
                }

                body.instruction(&Instruction::Else);

                let el = else_block.0;
                if el > li {
                    body.instruction(&Instruction::Br(el - li));
                } else {
                    body.instruction(&Instruction::I64Const(el as i64));
                    body.instruction(&Instruction::LocalSet(STATE_LOCAL));
                    body.instruction(&Instruction::Br(n - li));
                }

                body.instruction(&Instruction::End); // end If
            }
            CirTerminator::SuspendAndYield {
                effect,
                resume_block,
                live_vars,
                ..
            } => {
                // Write the frame header (state_id, frame_size, parent=0).
                let total = crate::cir::FRAME_HEADER_SIZE + live_vars.len() * 8;
                body.instruction(&Instruction::LocalGet(FRAME_PTR_LOCAL));
                body.instruction(&Instruction::I32Const(resume_block.0 as i32));
                body.instruction(&Instruction::I32Store(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
                body.instruction(&Instruction::LocalGet(FRAME_PTR_LOCAL));
                body.instruction(&Instruction::I32Const(total as i32));
                body.instruction(&Instruction::I32Store(MemArg {
                    offset: 4,
                    align: 2,
                    memory_index: 0,
                }));

                // Create a continuation wrapping the shared resume function,
                // pre-bind the frame pointer, then suspend with frame_ptr as
                // the payload. Resume functions wrap themselves (the
                // registry maps main function → resume function; a resume
                // function's own index falls back to itself).
                let resume_idx = self
                    .resume_func_of
                    .get(&own_idx)
                    .copied()
                    .unwrap_or(own_idx);
                body.instruction(&Instruction::RefFunc(resume_idx));
                body.instruction(&Instruction::ContNew(TY_CONT_FULL));
                body.instruction(&Instruction::LocalGet(FRAME_PTR_LOCAL));
                body.instruction(&Instruction::ContBind {
                    argument_index: TY_CONT_FULL,
                    result_index: TY_CONT_BOUND,
                });
                body.instruction(&Instruction::LocalGet(FRAME_PTR_LOCAL));
                body.instruction(&Instruction::Suspend(self.tag_for_effect(effect)));
                // Control never returns to this frame.
                body.instruction(&Instruction::Unreachable);
            }
            CirTerminator::Resume(_) => {
                // Defensive only: compile() rejects user-defined handler/resume
                // MIR before CIR lowering until this path has real semantics.
                body.instruction(&Instruction::I64Const(value_layout::TAG_NIL as i64));
                body.instruction(&Instruction::Return);
            }
        }
    }

    fn tag_for_effect(&self, effect: &EffectKind) -> u32 {
        match effect {
            EffectKind::LlmAsk => TAG_LLM_ASK,
            EffectKind::SignalWait => TAG_SIGNAL_WAIT,
            EffectKind::MailboxDequeue => TAG_MAILBOX_DEQUEUE,
            EffectKind::PerformAsync => TAG_PERFORM_ASYNC,
            EffectKind::ActorSend | EffectKind::HostEffect { .. } => TAG_HOST_EFFECT,
        }
    }

    // ── CIR expression codegen ─────────────────────────────────────

    fn compile_cir_expr(&self, body: &mut Function, e: &CirExpr, cir: &CirFunction) {
        match e {
            CirExpr::Var(v) => {
                body.instruction(&Instruction::LocalGet(v.0));
            }
            CirExpr::ConstI64(v) => {
                body.instruction(&Instruction::I64Const(*v));
            }
            CirExpr::ConstF64(v) => {
                // Floats ride in i64 as raw bits (mirrors mir_wasm.rs).
                body.instruction(&Instruction::I64Const(value_layout::float_bits(*v) as i64));
            }
            CirExpr::ConstBool(b) => {
                body.instruction(&Instruction::I64Const(value_layout::tag_bool(*b) as i64));
            }
            CirExpr::ConstNil => {
                body.instruction(&Instruction::I64Const(value_layout::TAG_NIL as i64));
            }
            CirExpr::ConstUnit => {
                body.instruction(&Instruction::I64Const(value_layout::TAG_UNIT as i64));
            }
            CirExpr::ConstString(s) => {
                let (offset, _len) = self.interned.get(s).copied().unwrap_or((0, 0));
                body.instruction(&Instruction::I64Const(
                    value_layout::TAG_STRING as i64 | offset as i64,
                ));
            }
            CirExpr::BinaryOp { op, lhs, rhs } => {
                self.compile_cir_expr(body, lhs, cir);
                self.compile_cir_expr(body, rhs, cir);
                self.emit_cir_binop(body, *op);
            }
            CirExpr::UnaryOp { op, operand } => {
                self.emit_cir_unary(body, *op, operand, cir);
            }
            CirExpr::Call { func_idx, args } => {
                for a in args {
                    self.compile_cir_expr(body, a, cir);
                }
                body.instruction(&Instruction::Call(*func_idx));
            }
            CirExpr::ArrayLen { arr } => {
                self.emit_cir_array_len(body, arr, cir);
            }
            CirExpr::ArrayLoad { arr, idx } => {
                self.emit_cir_array_load(body, arr, idx, cir);
            }
        }
    }

    fn emit_cir_binop(&self, body: &mut Function, op: BinaryOp) {
        let pm = value_layout::PAYLOAD_MASK as i64;
        let ti = value_layout::TAG_INT as i64;

        // Extract payloads: both operands on the stack as tagged i64.
        // Mask b (top of stack).
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        body.instruction(&Instruction::LocalSet(SCRATCH_A));
        // Mask a.
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        body.instruction(&Instruction::LocalGet(SCRATCH_A));

        let sign_extend_both = |b: &mut Function| {
            b.instruction(&Instruction::LocalSet(SCRATCH_A));
            b.instruction(&Instruction::I64Const(16));
            b.instruction(&Instruction::I64Shl);
            b.instruction(&Instruction::I64Const(16));
            b.instruction(&Instruction::I64ShrS);
            b.instruction(&Instruction::LocalGet(SCRATCH_A));
            b.instruction(&Instruction::I64Const(16));
            b.instruction(&Instruction::I64Shl);
            b.instruction(&Instruction::I64Const(16));
            b.instruction(&Instruction::I64ShrS);
        };

        match op {
            BinaryOp::And => {
                body.instruction(&Instruction::I64And);
                body.instruction(&Instruction::I64Const(value_layout::TAG_BOOL as i64));
                body.instruction(&Instruction::I64Or);
                return;
            }
            BinaryOp::Or => {
                body.instruction(&Instruction::I64Or);
                body.instruction(&Instruction::I64Const(value_layout::TAG_BOOL as i64));
                body.instruction(&Instruction::I64Or);
                return;
            }
            BinaryOp::Add => {
                body.instruction(&Instruction::I64Add);
            }
            BinaryOp::Sub => {
                body.instruction(&Instruction::I64Sub);
            }
            BinaryOp::Mul => {
                body.instruction(&Instruction::I64Mul);
            }
            BinaryOp::Div => {
                sign_extend_both(body);
                body.instruction(&Instruction::I64DivS);
            }
            BinaryOp::Mod => {
                sign_extend_both(body);
                body.instruction(&Instruction::I64RemS);
            }
            cmp @ (BinaryOp::Eq
            | BinaryOp::Neq
            | BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Lte
            | BinaryOp::Gte) => {
                sign_extend_both(body);
                match cmp {
                    BinaryOp::Eq => body.instruction(&Instruction::I64Eq),
                    BinaryOp::Neq => body.instruction(&Instruction::I64Ne),
                    BinaryOp::Lt => body.instruction(&Instruction::I64LtS),
                    BinaryOp::Gt => body.instruction(&Instruction::I64GtS),
                    BinaryOp::Lte => body.instruction(&Instruction::I64LeS),
                    BinaryOp::Gte => body.instruction(&Instruction::I64GeS),
                    _ => unreachable!(),
                };
                body.instruction(&Instruction::I64ExtendI32S);
                let tf = value_layout::tag_bool(false) as i64;
                let tt = value_layout::tag_bool(true) as i64;
                body.instruction(&Instruction::I64Const(tt - tf));
                body.instruction(&Instruction::I64Mul);
                body.instruction(&Instruction::I64Const(tf));
                body.instruction(&Instruction::I64Add);
                return;
            }
        }
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        body.instruction(&Instruction::I64Const(ti));
        body.instruction(&Instruction::I64Or);
    }

    fn emit_cir_unary(
        &self,
        body: &mut Function,
        op: UnaryOp,
        operand: &CirExpr,
        cir: &CirFunction,
    ) {
        let pm = value_layout::PAYLOAD_MASK as i64;
        match op {
            UnaryOp::Neg => {
                body.instruction(&Instruction::I64Const(0));
                self.compile_cir_expr(body, operand, cir);
                body.instruction(&Instruction::I64Const(pm));
                body.instruction(&Instruction::I64And);
                body.instruction(&Instruction::I64Sub);
                body.instruction(&Instruction::I64Const(pm));
                body.instruction(&Instruction::I64And);
                body.instruction(&Instruction::I64Const(value_layout::TAG_INT as i64));
                body.instruction(&Instruction::I64Or);
            }
            UnaryOp::Not => {
                let tf = value_layout::tag_bool(false) as i64;
                let tt = value_layout::tag_bool(true) as i64;
                self.compile_cir_expr(body, operand, cir);
                body.instruction(&Instruction::I64Const(tt));
                body.instruction(&Instruction::I64Eq);
                body.instruction(&Instruction::I64ExtendI32S);
                body.instruction(&Instruction::I64Const(tf - tt));
                body.instruction(&Instruction::I64Mul);
                body.instruction(&Instruction::I64Const(tt));
                body.instruction(&Instruction::I64Add);
            }
        }
    }

    fn emit_cir_array_len(&self, body: &mut Function, arr: &CirExpr, cir: &CirFunction) {
        let pm = value_layout::PAYLOAD_MASK as i64;
        self.compile_cir_expr(body, arr, cir);
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        body.instruction(&Instruction::I32WrapI64);
        body.instruction(&Instruction::I64Load(MemArg {
            offset: 0,
            align: 3,
            memory_index: 0,
        }));
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        body.instruction(&Instruction::I64Const(value_layout::TAG_INT as i64));
        body.instruction(&Instruction::I64Or);
    }

    fn emit_cir_array_load(
        &self,
        body: &mut Function,
        arr: &CirExpr,
        idx: &CirExpr,
        cir: &CirFunction,
    ) {
        let pm = value_layout::PAYLOAD_MASK as i64;
        self.compile_cir_expr(body, arr, cir);
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        self.compile_cir_expr(body, idx, cir);
        body.instruction(&Instruction::I64Const(pm));
        body.instruction(&Instruction::I64And);
        body.instruction(&Instruction::I64Const(8));
        body.instruction(&Instruction::I64Mul);
        body.instruction(&Instruction::I64Const(8));
        body.instruction(&Instruction::I64Add);
        body.instruction(&Instruction::I64Add);
        body.instruction(&Instruction::I32WrapI64);
        body.instruction(&Instruction::I64Load(MemArg {
            offset: 0,
            align: 3,
            memory_index: 0,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use crate::typechecker::TypeChecker;
    use crate::types::NuResult;

    fn compile_source_to_wasmfx(source: &str) -> NuResult<Vec<u8>> {
        let tokens = Lexer::new(source).lex()?;
        let ast = Parser::new(tokens).parse_module()?;
        let mut tc = TypeChecker::new();
        tc.check_module(&ast)?;
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir = crate::mir_lower::lower_module(&hir)?;
        let mut backend = WasmFxBackend::new();
        backend.compile(&mir, "test")
    }

    #[test]
    fn test_compile_literal_int() {
        let wasm = compile_source_to_wasmfx("42").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should have the nulang_init export
        let wasm_str = String::from_utf8_lossy(&wasm);
        assert!(
            wasm_str.contains("nulang_init"),
            "missing nulang_init export"
        );
    }

    #[test]
    fn test_compile_addition() {
        let wasm = compile_source_to_wasmfx("1 + 2").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain i64.add instruction (0x7C)
        assert!(wasm.contains(&0x7Cu8), "missing i64.add");
    }

    #[test]
    fn test_compile_multiplication() {
        let wasm = compile_source_to_wasmfx("4 * 5").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain i64.mul instruction (0x7E)
        assert!(wasm.contains(&0x7Eu8), "missing i64.mul");
    }

    #[test]
    fn test_compile_bool_true() {
        let wasm = compile_source_to_wasmfx("true").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
    }

    #[test]
    fn test_compile_comparison_eq() {
        let wasm = compile_source_to_wasmfx("1 == 1").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain i64.eq (0x51)
        assert!(wasm.contains(&0x51u8), "missing i64.eq");
    }

    #[test]
    fn test_compile_comparison_lt() {
        let wasm = compile_source_to_wasmfx("1 < 2").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain i64.lt_s (0x53)
        assert!(wasm.contains(&0x53u8), "missing i64.lt_s");
    }

    #[test]
    fn test_compile_float() {
        let wasm = compile_source_to_wasmfx("3.14").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
    }

    #[test]
    fn test_compile_let_binding() {
        let wasm = compile_source_to_wasmfx("let x = 10; x").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
    }

    #[test]
    fn test_compile_string() {
        let wasm = compile_source_to_wasmfx(r#""hello""#).expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain the string in the data section
        let wasm_str = String::from_utf8_lossy(&wasm);
        assert!(wasm_str.contains("hello"), "missing string data");
    }

    #[test]
    fn test_compile_io_print() {
        let wasm = compile_source_to_wasmfx(r#"perform IO.print("hi")"#).expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain call instruction (0x10) for the io_print import
        assert!(wasm.contains(&0x10u8), "missing call instruction");
        let wasm_str = String::from_utf8_lossy(&wasm);
        assert!(wasm_str.contains("io_print"), "missing io_print import");
    }

    #[test]
    fn test_compile_llm_ask_suspend() {
        let wasm = compile_source_to_wasmfx(r#"perform LLM.ask("test")"#).expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain cont.new (0xE0) and suspend (0xE2)
        let e0_count = wasm.iter().filter(|&&b| b == 0xE0).count();
        let e2_count = wasm.iter().filter(|&&b| b == 0xE2).count();
        assert!(e0_count > 0, "missing cont.new (0xE0)");
        assert!(e2_count > 0, "missing suspend (0xE2)");
    }

    #[test]
    fn test_compile_llm_ask_has_tags() {
        let wasm = compile_source_to_wasmfx(r#"perform LLM.ask("test")"#).expect("compile");
        let wasm_str = String::from_utf8_lossy(&wasm);
        // Should have tag_llm_ask import for suspension
        assert!(
            wasm_str.contains("tag_llm_ask"),
            "missing tag_llm_ask import"
        );
    }

    #[test]
    fn test_compile_non_suspending_no_tags() {
        let wasm = compile_source_to_wasmfx("42").expect("compile");
        let wasm_str = String::from_utf8_lossy(&wasm);
        // Non-suspending modules should NOT have tag imports
        assert!(
            !wasm_str.contains("tag_"),
            "non-suspending module should not have tag imports"
        );
    }

    #[test]
    fn test_compile_if_expr() {
        let wasm = compile_source_to_wasmfx("if true then { 1 } else { 2 }").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
    }

    #[test]
    fn test_compile_subtraction() {
        let wasm = compile_source_to_wasmfx("10 - 3").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
        // Should contain i64.sub (0x7D)
        assert!(wasm.contains(&0x7Du8), "missing i64.sub");
    }

    #[test]
    fn test_compile_block_expr() {
        let wasm = compile_source_to_wasmfx("{ 1; 2; 3 }").expect("compile");
        assert_eq!(&wasm[0..4], b"\0asm", "not valid WASM magic");
    }
}
