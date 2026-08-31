//! HIR -> MIR lowering.
//!
//! Converts the typed High-level IR into the 3-address-code Mid-level IR.
//!
//! Guarantees:
//!   - Everything this pass emits compiles to *correct* bytecode; any
//!     construct that cannot be lowered faithfully yet returns an honest
//!     `NotYetImplemented` error instead of emitting placeholder code.
//!   - Lexical scoping (with shadowing) is respected via a scope stack.
//!   - Lambdas and recursive let-bindings are lifted to top-level MIR
//!     functions; closures capture enclosing locals by value.
//!   - `actor` declarations are supported: behaviors compile through the
//!     same machinery as ordinary functions (see `lower_behavior_def`), with
//!     `self` bound as a local and `spawn`/`send`/`ask`/`self.field` lowered
//!     to their dedicated MIR constructs. `workflow` (including `parallel`
//!     blocks and saga compensation) and `agent` (including `@tool`-backed
//!     tools) desugar to actors at the HIR layer (see
//!     `hir_lower::desugar_workflow`/`desugar_agent`) and are supported the
//!     same way.

use crate::ast::Pattern;
use crate::hir;
use crate::mir;
use crate::types::{NuError, NuResult, Span, Type};
use std::collections::HashSet;

type FxHashMap<K, V> =
    std::collections::HashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

fn compile_err(msg: impl Into<String>, span: Span) -> NuError {
    NuError::VMError {
        msg: msg.into(),
        span,
    }
}

pub fn lower_module(hir: &hir::Module) -> NuResult<mir::Module> {
    let mut ctx = ModuleCtx::new(&hir.name);

    // Pass 1: reserve function/behavior slots and build actor metadata up
    // front, so forward references and mutual recursion between functions,
    // and between actors' send/ask sites, all resolve regardless of source
    // order. (The stable compiler only supports actors declared before use;
    // this pass is strictly more permissive, which cannot make any
    // currently-valid program disagree between the two backends.)
    for decl in &hir.decls {
        reserve_decl(&mut ctx, decl)?;
    }

    // Pass 2: lower function and behavior bodies into their reserved slots.
    for decl in &hir.decls {
        lower_decl_bodies(&mut ctx, decl)?;
    }

    let mut module = ctx.finish()?;
    crate::mir_inline::inline_local_closures(&mut module);
    Ok(module)
}

/// Nested modules are purely a namespacing construct: the stable compiler's
/// `compile_decl` flattens `Decl::Module { decls, .. }` by recursing over
/// `decls` in place, so this pass does the same instead of erroring.
fn reserve_decl(ctx: &mut ModuleCtx, decl: &hir::Decl) -> NuResult<()> {
    match decl {
        hir::Decl::CrdtDecl { name, .. } => {
            // CRDT declaration - reserve a placeholder
            let _ = ctx.reserve_function(name); // placeholder
        }
        hir::Decl::Function(f) => {
            if ctx.func_map.contains_key(&f.name) {
                return Err(compile_err(
                    format!(
                        "duplicate function '{}': a function with this name is already \
                         declared in this module (top-level and `module {{ .. }}` blocks \
                         share one flat namespace, so nested modules can't disambiguate \
                         same-named functions -- rename one of them)",
                        f.name
                    ),
                    f.span,
                ));
            }
            let idx = ctx.reserve_function(&f.name);
            ctx.func_map.insert(f.name.clone(), idx);
        }
        hir::Decl::ExternBlock { library, funcs, .. } => {
            for f in funcs {
                let idx = ctx.foreign.len();
                ctx.foreign.push(mir::ForeignFunction {
                    library: library.clone(),
                    symbol: f.name.clone(),
                    params: f.params.iter().map(|(_, t)| t.clone()).collect(),
                    ret: f.ret.clone(),
                });
                ctx.extern_map.insert(f.name.clone(), idx);
            }
        }
        hir::Decl::Actor(a) => {
            let first_idx = ctx.behaviors.len();
            for b in &a.behaviors {
                ctx.reserve_behavior(format!("{}.{}", a.name, b.name));
            }
            let behavior_indices: Vec<usize> = (first_idx..ctx.behaviors.len()).collect();
            // Compensation slots are reserved AFTER all of this actor's
            // real behaviors, so they never fall inside behavior_indices
            // (compensations are invoked directly by offset, never
            // dispatched by name via send/ask).
            for (b, &abs_idx) in a.behaviors.iter().zip(behavior_indices.iter()) {
                if b.compensate.is_some() {
                    let comp_idx =
                        ctx.reserve_behavior(format!("{}.{}__compensate", a.name, b.name));
                    // The step's ABSOLUTE (whole-module) behavior index —
                    // not a per-actor relative index. The codegen resolves
                    // compensations against each actor's own
                    // `behavior_indices`; a relative index would let an
                    // actor declared before the workflow hijack its first
                    // compensation (SPEC2 §10 known-issue #2).
                    ctx.compensation_of.push((abs_idx, comp_idx));
                }
                if let Some(branches) = &b.parallel_branches {
                    ctx.parallel_branches_of.push((abs_idx, branches.clone()));
                }
            }
            let state_models = a
                .state_fields
                .iter()
                .map(|(name, model, _ty, _default)| (name.clone(), *model))
                .collect();
            let state_defaults = a
                .state_fields
                .iter()
                .filter_map(|(name, _model, _ty, default)| match default {
                    hir::Operand::Literal(lit, _) => Some((name.clone(), literal_to_constant(lit))),
                    _ => None,
                })
                .collect();
            ctx.actor_metas.push(crate::bytecode::ActorMeta {
                name: a.name.clone(),
                persistent: a.persistent,
                state_models,
                state_defaults,
                behavior_indices,
                is_workflow: a.is_workflow,
                is_agent: a.is_agent,
                is_organization: a.is_organization,
                is_virtual: a.virtual_,
                tools: a.tools.clone(),
                semantic_memory_dimensions: a.semantic_memory_dimensions,
                procedural_memory_namespace: a.procedural_memory_namespace.clone(),
                backend: crate::ast::ActorBackendKind::Native,
                fallback_config: a.fallback_config.clone(),
                retry_config: a.retry_config.clone(),
                type_hash: None,
                version: a.version,
                migrations: String::new(),
            });
        }
        hir::Decl::Workflow { name, .. } => {
            unreachable!(
                "workflow '{}' should be desugared to an actor by HIR lowering (desugar_workflow)",
                name
            );
        }
        hir::Decl::Agent { name, .. } => {
            unreachable!(
                "agent '{}' should be desugared to an actor by HIR lowering (desugar_agent)",
                name
            );
        }
        hir::Decl::Module { decls, .. } => {
            for d in decls {
                reserve_decl(ctx, d)?;
            }
        }
        hir::Decl::VariantType { variants, .. } => {
            // Variant declarations produce no code, but construction sites
            // need the constructor table: a payload constructor call
            // `Some(x)` builds a `{ ctor, payload }` record and a nullary
            // constructor reference `None` is the bare tag string (see
            // pattern_test for the matching destructuring side). On a name
            // collision between two variant declarations the later one
            // wins, mirroring the typechecker's ctx.bind.
            for (ctor_name, payload) in variants {
                ctx.ctor_map.insert(ctor_name.clone(), payload.is_some());
            }
        }
        hir::Decl::Constant { name, .. } => {
            let idx = ctx.reserve_function(name);
            ctx.func_map.insert(name.clone(), idx);
        }
        // Type-level declarations produce no code.
        hir::Decl::TypeAlias { .. }
        | hir::Decl::RecordType { .. }
        | hir::Decl::EffectDecl { .. }
        | hir::Decl::Import { .. }
        | hir::Decl::Database { .. } => {}
    }
    Ok(())
}

fn lower_decl_bodies(ctx: &mut ModuleCtx, decl: &hir::Decl) -> NuResult<()> {
    match decl {
        hir::Decl::Function(f) => {
            let idx = ctx.func_map[&f.name];
            let func = lower_function_def(ctx, f)?;
            ctx.fill_function(idx, func);
        }
        hir::Decl::Actor(a) => {
            let indices = ctx
                .actor_metas
                .iter()
                .find(|m| m.name == a.name)
                .expect("actor registered in pass 1")
                .behavior_indices
                .clone();
            for (b, &idx) in a.behaviors.iter().zip(indices.iter()) {
                let full_name = format!("{}.{}", a.name, b.name);
                let func = lower_behavior_def(ctx, &full_name, b)?;
                ctx.fill_behavior(idx, func);

                if let Some(comp_body) = &b.compensate {
                    let comp_idx = ctx
                        .compensation_of
                        .iter()
                        .find(|(behavior_idx, _)| *behavior_idx == idx)
                        .map(|(_, comp_idx)| *comp_idx)
                        .expect("compensation slot reserved in pass 1");
                    let comp_def = hir::BehaviorDef {
                        name: format!("{}__compensate", b.name),
                        params: Vec::new(),
                        ret: b.ret.clone(),
                        effect: b.effect.clone(),
                        cap: b.cap,
                        body: comp_body.clone(),
                        compensate: None,
                        parallel_branches: None,
                        span: b.span,
                    };
                    let comp_full_name = format!("{}.{}__compensate", a.name, b.name);
                    let comp_func = lower_behavior_def(ctx, &comp_full_name, &comp_def)?;
                    ctx.fill_behavior(comp_idx, comp_func);
                }
            }
        }
        hir::Decl::Module { decls, .. } => {
            for d in decls {
                lower_decl_bodies(ctx, d)?;
            }
        }
        hir::Decl::Constant { name, body, .. } => {
            let idx = ctx.func_map[name];
            let mut lowerer = FnLowerer::new(ctx, name, None);
            lowerer.lower_body_top(body)?;
            let func = lowerer.b.build();
            ctx.fill_function(idx, func);
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Module context
// ---------------------------------------------------------------------------

struct ModuleCtx {
    name: String,
    functions: Vec<Option<mir::Function>>,
    func_map: FxHashMap<String, usize>,
    extern_map: FxHashMap<String, usize>,
    foreign: Vec<mir::ForeignFunction>,
    /// Actor behaviors, reserved (with their fully-qualified "Actor.behavior"
    /// name) in pass 1 and filled in pass 2 — mirrors `functions`, but never
    /// registered in `func_map` so they stay un-`Call`-able.
    behaviors: Vec<Option<mir::Function>>,
    behavior_names: Vec<String>,
    actor_metas: Vec<crate::bytecode::ActorMeta>,
    /// `(step_behavior_idx, compensation_behavior_idx)` pairs; see `mir::Module`.
    compensation_of: Vec<(usize, usize)>,
    /// `(behavior_idx, branch_names)` pairs; see `mir::Module`.
    parallel_branches_of: Vec<(usize, Vec<String>)>,
    /// Declared variant constructors: ctor name -> has_payload. Populated in
    /// pass 1 from `Decl::VariantType` so construction sites (`Some(41)`,
    /// `None`) resolve regardless of source order; see `reserve_decl`.
    ctor_map: FxHashMap<String, bool>,
    next_lambda: u32,
}

impl ModuleCtx {
    fn new(name: &str) -> Self {
        ModuleCtx {
            name: name.to_string(),
            functions: Vec::new(),
            func_map: FxHashMap::default(),
            extern_map: FxHashMap::default(),
            foreign: Vec::new(),
            behaviors: Vec::new(),
            behavior_names: Vec::new(),
            actor_metas: Vec::new(),
            compensation_of: Vec::new(),
            parallel_branches_of: Vec::new(),
            ctor_map: FxHashMap::default(),
            next_lambda: 0,
        }
    }

    fn reserve_function(&mut self, _name: &str) -> usize {
        self.functions.push(None);
        self.functions.len() - 1
    }

    fn fill_function(&mut self, idx: usize, func: mir::Function) {
        self.functions[idx] = Some(func);
    }

    fn reserve_behavior(&mut self, full_name: String) -> usize {
        self.behaviors.push(None);
        self.behavior_names.push(full_name);
        self.behaviors.len() - 1
    }

    fn fill_behavior(&mut self, idx: usize, func: mir::Function) {
        self.behaviors[idx] = Some(func);
    }

    /// Resolve `spawn ActorName { ... }` to the behavior-table index the VM
    /// uses to look up the actor's metadata (its first behavior's index).
    /// Mirrors the stable compiler's `compile_spawn`.
    fn spawn_behavior_idx(&self, actor_name: &str) -> usize {
        self.actor_metas
            .iter()
            .find(|m| m.name == actor_name)
            .and_then(|m| m.behavior_indices.first().copied())
            .unwrap_or(self.behaviors.len())
    }

    /// Resolve `send`/`ask actor behavior(...)` to a behavior-table index by
    /// name. Mirrors the stable compiler's `behavior_table_index`: an exact
    /// "ActorName.behavior" match first, falling back to any behavior with a
    /// matching suffix if the receiver expression isn't a bare actor-typed
    /// variable name (a known ambiguity inherited from the stable compiler,
    /// not introduced here).
    fn send_behavior_idx(&self, actor_name_hint: &str, behavior: &str) -> usize {
        let full_name = format!("{}.{}", actor_name_hint, behavior);
        if let Some(idx) = self.behavior_names.iter().position(|n| *n == full_name) {
            return idx;
        }
        let suffix = format!(".{}", behavior);
        self.behavior_names
            .iter()
            .position(|n| n.ends_with(&suffix))
            .unwrap_or(self.behaviors.len())
    }

    fn fresh_lambda_name(&mut self) -> String {
        let n = self.next_lambda;
        self.next_lambda += 1;
        format!("__lambda_{}", n)
    }

    fn finish(self) -> NuResult<mir::Module> {
        let mut module = mir::Module::new(&self.name);
        for (i, f) in self.functions.into_iter().enumerate() {
            let mut f = f.ok_or_else(|| {
                compile_err(
                    format!("internal: MIR function slot {} left unfilled", i),
                    Span::default(),
                )
            })?;
            fuse_single_use_temps(&mut f);
            module.functions.push(f);
        }
        for (i, f) in self.behaviors.into_iter().enumerate() {
            let mut f = f.ok_or_else(|| {
                compile_err(
                    format!("internal: MIR behavior slot {} left unfilled", i),
                    Span::default(),
                )
            })?;
            fuse_single_use_temps(&mut f);
            module.behaviors.push(f);
        }
        module.actor_metadata = self.actor_metas;
        module.compensation_of = self.compensation_of;
        module.parallel_branches_of = self.parallel_branches_of;
        module.foreign_functions = self.foreign;
        Ok(module)
    }
}

fn lower_function_def(ctx: &mut ModuleCtx, f: &hir::FunctionDef) -> NuResult<mir::Function> {
    let mut lowerer = FnLowerer::new(ctx, &f.name, Some(f.ret.clone()));
    lowerer.b.set_placement(f.placement);
    for (name, ty) in &f.params {
        let id = lowerer.b.add_param(name.clone(), ty.clone());
        lowerer.bind(name, id);
    }
    lowerer.lower_body_top(&f.body)?;
    Ok(lowerer.b.build())
}

/// Lower a lifted lambda/recursive-function body into a standalone MIR
/// function. `captures` are bound from closure capture slots (in order);
/// `rec` binds a name to the function's own index for recursive calls.
fn lower_lifted(
    ctx: &mut ModuleCtx,
    name: &str,
    params: &[(String, Type)],
    captures: &[String],
    rec: Option<(&str, usize)>,
    body: &hir::Body,
) -> NuResult<mir::Function> {
    let mut lowerer = FnLowerer::new(ctx, name, None);
    for (pname, ty) in params {
        let id = lowerer.b.add_param(pname.clone(), ty.clone());
        lowerer.bind(pname, id);
    }
    for cname in captures {
        let id = lowerer.b.add_capture(cname.clone(), Type::unit());
        lowerer.bind(cname, id);
    }
    if let Some((rec_name, rec_idx)) = rec {
        // The function refers to itself by name. Without captures a raw
        // function-table index suffices (callable like any function value);
        // with captures the self-reference must be a closure carrying the
        // same environment, otherwise recursive calls would lose the
        // captured values (CapLoad would fail outside a closure call).
        let id = lowerer.b.add_local(rec_name, Type::unit());
        if captures.is_empty() {
            lowerer.b.assign(
                id,
                mir::RValue::Const(crate::bytecode::Constant::Int(rec_idx as i64)),
            );
        } else {
            let cap_ids: Vec<mir::LocalId> = captures
                .iter()
                .map(|n| lowerer.lookup(n).expect("capture just bound"))
                .collect();
            lowerer.b.assign(
                id,
                mir::RValue::Closure {
                    func: rec_idx,
                    captures: cap_ids,
                },
            );
        }
        lowerer.bind(rec_name, id);
    }
    lowerer.lower_body_top(body)?;
    Ok(lowerer.b.build())
}

/// Lower an actor behavior body into a standalone MIR function. Identical to
/// an ordinary function except for the prologue statement binding `self` to
/// the current actor reference, mirroring the stable compiler's
/// `compile_behavior`.
fn lower_behavior_def(
    ctx: &mut ModuleCtx,
    full_name: &str,
    bh: &hir::BehaviorDef,
) -> NuResult<mir::Function> {
    let mut lowerer = FnLowerer::new(ctx, full_name, Some(bh.ret.clone()));
    for (name, ty) in &bh.params {
        let id = lowerer.b.add_param(name.clone(), ty.clone());
        lowerer.bind(name, id);
    }
    let self_id = lowerer.b.add_local("self", Type::unit());
    lowerer.b.assign(self_id, mir::RValue::SelfRef);
    lowerer.bind("self", self_id);
    lowerer.lower_body_top(&bh.body)?;
    Ok(lowerer.b.build())
}

// ---------------------------------------------------------------------------
// Function lowering
// ---------------------------------------------------------------------------

struct FnLowerer<'c> {
    ctx: &'c mut ModuleCtx,
    b: mir::FunctionBuilder,
    scopes: Vec<Vec<(String, mir::LocalId)>>,
    loop_exits: Vec<mir::BlockId>,
    loop_results: Vec<mir::LocalId>,
    /// Number of `handle` bodies currently being lowered. Each one pushed a
    /// handler frame at runtime, so an explicit `return` emitted while this
    /// is > 0 must unwind that many frames first (the VM does not unwind
    /// `handler_stack` on `Ret`).
    handle_depth: usize,
    /// Stack of active handler scopes.  Each entry is `(table_index, [(binding_index, effect_qualified_name)])`.
    /// Pushed when lowering a `handle` block (after `EnterHandle`) and
    /// popped when the `handle`'s join block is reached.  Used to resolve
    /// `Perform` operations to a statically-known `HandlerRef`.
    handler_scope: Vec<(usize, Vec<(usize, String)>)>,
}

impl<'c> FnLowerer<'c> {
    fn new(ctx: &'c mut ModuleCtx, name: &str, ret: Option<Type>) -> Self {
        FnLowerer {
            ctx,
            b: mir::FunctionBuilder::new(name, ret),
            scopes: vec![Vec::new()],
            loop_exits: Vec::new(),
            loop_results: Vec::new(),
            handle_depth: 0,
            handler_scope: Vec::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &str, id: mir::LocalId) {
        self.scopes
            .last_mut()
            .expect("scope stack never empty")
            .push((name.to_string(), id));
    }

    fn lookup(&self, name: &str) -> Option<mir::LocalId> {
        for scope in self.scopes.iter().rev() {
            for (n, id) in scope.iter().rev() {
                if n == name {
                    return Some(*id);
                }
            }
        }
        None
    }

    // -- Body lowering ------------------------------------------------------

    /// Terminate the current block with a function return, first unwinding
    /// any handler frames installed by enclosing `handle` bodies (and by the
    /// handler body itself, when the return sits inside one — the VM keeps
    /// the frame on `handler_stack` while the handler runs). Without this
    /// the frame would outlive the function on the VM's `handler_stack`,
    /// and a later unhandled perform of the same effect would dispatch
    /// into the dead function's handler code.
    fn emit_return(&mut self, id: mir::LocalId) {
        for _ in 0..self.handle_depth {
            self.b.emit(mir::Stmt::PopHandler);
            // Pop the corresponding handler scope entry so a `return`
            // inside a handle block correctly unwinds the scope stack.
            self.handler_scope.pop();
        }
        self.b.terminate(mir::Terminator::Return(Some(id)));
    }

    /// Lower a body in function-return position.
    fn lower_body_top(&mut self, body: &hir::Body) -> NuResult<()> {
        for stmt in &body.stmts {
            self.lower_stmt(stmt)?;
        }
        if self.b.is_terminated() {
            return Ok(());
        }
        match &body.terminator {
            hir::Terminator::Yield(op) | hir::Terminator::FnReturn(Some(op)) => {
                let id = self.lower_operand(op)?;
                self.b.terminate(mir::Terminator::Return(Some(id)));
            }
            hir::Terminator::FnReturn(None) => {
                let id = self.unit_temp();
                self.b.terminate(mir::Terminator::Return(Some(id)));
            }
            hir::Terminator::Break(_) => {
                return Err(compile_err("break outside of a loop", Span::default()));
            }
        }
        Ok(())
    }

    /// Lower a body in expression position: its yielded value is assigned to
    /// `dst` and control joins `join`. Explicit returns still return from the
    /// function; breaks target the innermost loop.
    fn lower_body_into(
        &mut self,
        body: &hir::Body,
        dst: mir::LocalId,
        join: mir::BlockId,
    ) -> NuResult<()> {
        use crate::bytecode::Constant;
        for stmt in &body.stmts {
            self.lower_stmt(stmt)?;
        }
        if self.b.is_terminated() {
            return Ok(());
        }
        match &body.terminator {
            hir::Terminator::Yield(op) => {
                let id = self.lower_operand(op)?;
                self.b.assign(dst, mir::RValue::Load(id));
                self.b.terminate(mir::Terminator::Jump(join));
            }
            hir::Terminator::FnReturn(op) => {
                let id = match op {
                    Some(op) => self.lower_operand(op)?,
                    None => self.unit_temp(),
                };
                self.emit_return(id);
            }
            hir::Terminator::Break(op) => {
                let exit = self
                    .loop_exits
                    .last()
                    .copied()
                    .ok_or_else(|| compile_err("break outside of a loop", Span::default()))?;
                let result = self
                    .loop_results
                    .last()
                    .copied()
                    .ok_or_else(|| compile_err("break outside of a loop", Span::default()))?;
                match op {
                    Some(op) => {
                        let val = self.lower_operand(op)?;
                        self.b.assign(result, mir::RValue::Load(val));
                    }
                    None => {
                        self.b.assign(result, mir::RValue::Const(Constant::Unit));
                    }
                }
                self.b.terminate(mir::Terminator::Jump(exit));
            }
        }
        Ok(())
    }

    // -- Statements ----------------------------------------------------------

    fn lower_stmt(&mut self, stmt: &hir::Stmt) -> NuResult<()> {
        if self.b.is_terminated() {
            // Unreachable code after return/break: skip.
            return Ok(());
        }
        // Attach the HIR statement's source line to every MIR statement it
        // emits (drives the debugger's PC<->line table).
        let line = match stmt {
            hir::Stmt::Let { span, .. }
            | hir::Stmt::Assign { span, .. }
            | hir::Stmt::StateSet { span, .. }
            | hir::Stmt::Emit { span, .. } => span.line(),
        } as u32;
        if line != 0 {
            self.b.set_line(line);
        }
        match stmt {
            hir::Stmt::Let {
                name, ty, value, ..
            } => {
                let dst = self.b.add_local(name.clone(), ty.clone());
                self.lower_rvalue(dst, value)?;
                self.bind(name, dst);
                Ok(())
            }
            hir::Stmt::Assign { target, value, .. } => self.lower_assign(target, value),
            hir::Stmt::StateSet { field, value, .. } => {
                let src = self.lower_operand(value)?;
                self.b.emit(mir::Stmt::StateSet {
                    field: field.clone(),
                    src,
                });
                Ok(())
            }
            hir::Stmt::Emit { event, args, .. } => {
                let mut ids = Vec::with_capacity(args.len());
                for a in args {
                    ids.push(self.lower_operand(a)?);
                }
                self.b.emit(mir::Stmt::Emit {
                    event: event.clone(),
                    args: ids,
                });
                Ok(())
            }
        }
    }

    fn lower_assign(&mut self, target: &hir::Place, value: &hir::RValue) -> NuResult<()> {
        match target {
            hir::Place::Var(name, _) => {
                // "self" is bound as an ordinary local in behavior bodies
                // (see lower_behavior_def), so reassigning it needs no
                // special case — it just overwrites that local, same as the
                // stable compiler.
                let dst = self.lookup(name).ok_or_else(|| {
                    compile_err(
                        format!("assignment to undefined variable '{}'", name),
                        Span::default(),
                    )
                })?;
                self.lower_rvalue(dst, value)
            }
            hir::Place::Field { base, field, .. } if place_is_self(base) => {
                let src = self.b.add_temp(Type::unit());
                self.lower_rvalue(src, value)?;
                self.b.emit(mir::Stmt::StateSet {
                    field: field.clone(),
                    src,
                });
                Ok(())
            }
            hir::Place::Field { base, field, .. } => {
                let obj = self.read_place(base)?;
                let src = self.b.add_temp(Type::unit());
                self.lower_rvalue(src, value)?;
                self.b.emit(mir::Stmt::StoreFieldNamed {
                    obj,
                    field: field.clone(),
                    src,
                });
                Ok(())
            }
            hir::Place::Index { base, idx, .. } => {
                let arr = self.read_place(base)?;
                let idx_id = self.lower_operand(idx)?;
                let src = self.b.add_temp(Type::unit());
                self.lower_rvalue(src, value)?;
                self.b.emit(mir::Stmt::ArrayStore {
                    arr,
                    idx: idx_id,
                    src,
                });
                Ok(())
            }
        }
    }

    fn read_place(&mut self, place: &hir::Place) -> NuResult<mir::LocalId> {
        match place {
            hir::Place::Var(name, _) => self.lookup(name).ok_or_else(|| {
                compile_err(format!("undefined variable '{}'", name), Span::default())
            }),
            hir::Place::Field { base, field, .. } => {
                let obj = self.read_place(base)?;
                let dst = self.b.add_temp(Type::unit());
                self.b.assign(
                    dst,
                    mir::RValue::LoadFieldNamed {
                        obj,
                        field: field.clone(),
                    },
                );
                Ok(dst)
            }
            hir::Place::Index { base, idx, .. } => {
                let arr = self.read_place(base)?;
                let idx_id = self.lower_operand(idx)?;
                let dst = self.b.add_temp(Type::unit());
                self.b
                    .assign(dst, mir::RValue::ArrayLoad { arr, idx: idx_id });
                Ok(dst)
            }
        }
    }

    // -- Operands ------------------------------------------------------------

    fn lower_operand(&mut self, op: &hir::Operand) -> NuResult<mir::LocalId> {
        match op {
            hir::Operand::Var(name, _) => {
                if let Some(id) = self.lookup(name) {
                    return Ok(id);
                }
                if let Some(&idx) = self.ctx.func_map.get(name) {
                    // Reference to a top-level function used as a value.
                    let id = self.b.add_temp(Type::unit());
                    self.b.assign(
                        id,
                        mir::RValue::Const(crate::bytecode::Constant::Int(idx as i64)),
                    );
                    return Ok(id);
                }
                if name == "self" {
                    let id = self.b.add_temp(Type::unit());
                    self.b.assign(id, mir::RValue::SelfRef);
                    return Ok(id);
                }
                if let Some(&has_payload) = self.ctx.ctor_map.get(name) {
                    // Declared variant constructor used as a value. Locals
                    // and top-level functions shadow constructors (resolved
                    // above), so a user `let Some = ...` or `fn Some(...)`
                    // always wins over the ctor.
                    return self.lower_ctor_value(name, has_payload);
                }
                Err(compile_err(
                    format!("undefined variable '{}' in MIR lowering", name),
                    Span::default(),
                ))
            }
            hir::Operand::Literal(lit, ty) => {
                let id = self.b.add_temp(ty.clone());
                self.b
                    .assign(id, mir::RValue::Const(literal_to_constant(lit)));
                Ok(id)
            }
            hir::Operand::Unit => Ok(self.unit_temp()),
        }
    }

    fn unit_temp(&mut self) -> mir::LocalId {
        let id = self.b.add_temp(Type::unit());
        self.b
            .assign(id, mir::RValue::Const(crate::bytecode::Constant::Unit));
        id
    }

    /// Lower a declared variant constructor *call* `Some(x)` to the record
    /// `{ ctor: "Some", payload: x }` — the representation pattern_test and
    /// bind_pattern destructure (`ctor` rather than `tag` because `tag` is a
    /// lexer keyword). Field names resolve to module-wide field ids in
    /// codegen, so both sides agree on the layout automatically.
    fn lower_ctor_call(
        &mut self,
        dst: mir::LocalId,
        name: &str,
        has_payload: bool,
        args: Vec<mir::LocalId>,
    ) -> NuResult<()> {
        if !has_payload {
            return Err(compile_err(
                format!(
                    "constructor '{}' takes no payload; use it as a plain value, not a call",
                    name
                ),
                Span::default(),
            ));
        }
        let payload = if args.len() == 1 {
            args[0]
        } else {
            // Pack multiple args into a tuple to match the typechecker's
            // convention: a constructor with payload (T, U) expects two
            // separate arguments at the call site, not a single tuple.
            let tup = self.b.add_temp(Type::unit());
            self.b.assign(tup, mir::RValue::Tuple(args.to_vec()));
            tup
        };
        let tag = self.b.add_temp(Type::string());
        self.b.assign(
            tag,
            mir::RValue::Const(crate::bytecode::Constant::String(name.to_string())),
        );
        self.b.assign(
            dst,
            mir::RValue::Record(vec![
                ("ctor".to_string(), tag),
                ("payload".to_string(), payload),
            ]),
        );
        Ok(())
    }

    /// Lower a declared variant constructor used as a *value* (not a call).
    /// A nullary constructor is the bare tag string — the payload-less
    /// variant pattern test string-compares the whole scrutinee against the
    /// tag, so the value must be exactly that representation. A payload
    /// constructor eta-expands to a lifted `fn(x) { { ctor, payload: x } }`
    /// so `let f = Some in f(1)` works like any other function value.
    fn lower_ctor_value(&mut self, name: &str, has_payload: bool) -> NuResult<mir::LocalId> {
        if !has_payload {
            let id = self.b.add_temp(Type::string());
            self.b.assign(
                id,
                mir::RValue::Const(crate::bytecode::Constant::String(name.to_string())),
            );
            return Ok(id);
        }
        let lname = self.ctx.fresh_lambda_name();
        let idx = self.ctx.reserve_function(&lname);
        let mut body = hir::Body::new();
        body.stmts.push(hir::Stmt::Let {
            name: "__ctor".to_string(),
            ty: Type::unit(),
            value: hir::RValue::Record(
                vec![
                    (
                        "ctor".to_string(),
                        hir::Operand::Literal(
                            crate::ast::Literal::String(name.to_string()),
                            Type::string(),
                        ),
                    ),
                    (
                        "payload".to_string(),
                        hir::Operand::Var("__payload".to_string(), Type::unit()),
                    ),
                ],
                Type::unit(),
            ),
            span: Span::default(),
        });
        body.terminator =
            hir::Terminator::Yield(hir::Operand::Var("__ctor".to_string(), Type::unit()));
        let lifted = lower_lifted(
            self.ctx,
            &lname,
            &[("__payload".to_string(), Type::unit())],
            &[],
            None,
            &body,
        )?;
        self.ctx.fill_function(idx, lifted);
        // No captures: the function-table index is the value (callable like
        // any top-level function reference — see lower_lifted).
        let id = self.b.add_temp(Type::unit());
        self.b.assign(
            id,
            mir::RValue::Const(crate::bytecode::Constant::Int(idx as i64)),
        );
        Ok(id)
    }

    // -- RValues (dst-directed) -----------------------------------------------

    fn lower_rvalue(&mut self, dst: mir::LocalId, rv: &hir::RValue) -> NuResult<()> {
        use crate::bytecode::Constant;
        match rv {
            hir::RValue::Use(op) => {
                let id = self.lower_operand(op)?;
                self.b.assign(dst, mir::RValue::Load(id));
                Ok(())
            }
            hir::RValue::Panic(msg) => {
                self.b.assign(dst, mir::RValue::Panic(msg.clone()));
                Ok(())
            }
            hir::RValue::Literal(lit, _) => {
                self.b
                    .assign(dst, mir::RValue::Const(literal_to_constant(lit)));
                Ok(())
            }
            hir::RValue::Binary(op, l, r, _) => {
                let lid = self.lower_operand(l)?;
                let rid = self.lower_operand(r)?;
                // String equality/inequality must use SCmpEq (content
                // comparison), not ICmpEq (raw-bit comparison), because
                // a string value may be a constant-pool id or a heap
                // pointer depending on how it was constructed.
                // HIR Operand::Var always carries Type::unit(), so we must
                // also consult the MIR local types (which are populated from
                // Stmt::Let type annotations) to detect string-typed variables.
                let l_mir_ty = self.b.local_ty(lid);
                let r_mir_ty = self.b.local_ty(rid);
                let is_string = l.ty() == Type::string()
                    || r.ty() == Type::string()
                    || *l_mir_ty == Type::string()
                    || *r_mir_ty == Type::string();
                match (op, is_string) {
                    (crate::ast::BinOp::Eq, true) => {
                        self.b.assign(dst, mir::RValue::StringEq(lid, rid));
                    }
                    (crate::ast::BinOp::Ne, true) => {
                        let eq_dst = self.b.add_temp(Type::bool());
                        self.b.assign(eq_dst, mir::RValue::StringEq(lid, rid));
                        self.b
                            .assign(dst, mir::RValue::Unary(crate::ast::UnOp::Not, eq_dst));
                    }
                    (crate::ast::BinOp::Add, true) => {
                        self.b.assign(dst, mir::RValue::StrConcat(lid, rid));
                        // A concatenation result is always a String. The dst
                        // local may have been created with a placeholder type
                        // (hir_lower's `binary_type` cannot see through a
                        // variable operand that lowers to Type::unit()), so
                        // record the precise type here — otherwise a chained
                        // `s + 2 + 3` lowers the second add as `Binary(Add)`
                        // instead of `StrConcat`, and the native/AOT backends
                        // mis-tag the pointer as an int.
                        self.b.set_local_ty(dst, Type::string());
                    }
                    _ => {
                        self.b.assign(dst, mir::RValue::Binary(*op, lid, rid));
                        // The dst local was created by `hir_lower::binary_type`,
                        // which cannot see through variable operands (HIR vars
                        // carry `Type::unit()`). Consult the MIR operand locals
                        // (which hold the real types) so a float arithmetic
                        // result is typed Float — otherwise downstream unary
                        // ops (e.g. `-(x + y)` for float x,y) mis-compile the
                        // float bits as an int.
                        let lt = self.b.local_ty(lid);
                        let rt = self.b.local_ty(rid);
                        if *lt == Type::float() || *rt == Type::float() {
                            self.b.set_local_ty(dst, Type::float());
                        }
                    }
                }
                Ok(())
            }
            hir::RValue::Unary(op, e, _) => {
                let id = self.lower_operand(e)?;
                self.b.assign(dst, mir::RValue::Unary(*op, id));
                Ok(())
            }
            hir::RValue::Call { func, args, .. } => {
                let mut aids = Vec::with_capacity(args.len());
                for a in args {
                    aids.push(self.lower_operand(a)?);
                }
                let func_ref = match func {
                    hir::Operand::Var(name, _) => {
                        if let Some(id) = self.lookup(name) {
                            mir::FuncRef::Local(id)
                        } else if let Some(&idx) = self.ctx.func_map.get(name) {
                            mir::FuncRef::Index(idx)
                        } else if let Some(&eidx) = self.ctx.extern_map.get(name) {
                            self.b.assign(
                                dst,
                                mir::RValue::FFICall {
                                    idx: eidx,
                                    args: aids,
                                },
                            );
                            return Ok(());
                        } else if let Some(&has_payload) = self.ctx.ctor_map.get(name) {
                            // Declared variant constructor call. Locals,
                            // top-level functions and externs shadow
                            // constructors (resolved above), so a user
                            // `fn Some(...)` wins over the ctor.
                            return self.lower_ctor_call(dst, name, has_payload, aids);
                        } else {
                            return Err(compile_err(
                                format!("call to undefined function '{}'", name),
                                Span::default(),
                            ));
                        }
                    }
                    _ => {
                        let id = self.lower_operand(func)?;
                        mir::FuncRef::Local(id)
                    }
                };
                self.b.assign(
                    dst,
                    mir::RValue::Call {
                        func: func_ref,
                        args: aids,
                    },
                );
                Ok(())
            }
            hir::RValue::Closure {
                params,
                body,
                captures,
                ..
            } => {
                // Capture only names that are actually locals in scope here;
                // top-level functions and externs resolve inside the lifted
                // function without capturing.
                let capture_names: Vec<String> = captures
                    .iter()
                    .filter(|n| self.lookup(n).is_some())
                    .cloned()
                    .collect();
                let capture_ids: Vec<mir::LocalId> = capture_names
                    .iter()
                    .map(|n| self.lookup(n).expect("capture just resolved"))
                    .collect();
                let lname = self.ctx.fresh_lambda_name();
                let idx = self.ctx.reserve_function(&lname);
                let lifted = lower_lifted(self.ctx, &lname, params, &capture_names, None, body)?;
                self.ctx.fill_function(idx, lifted);
                self.b.assign(
                    dst,
                    mir::RValue::Closure {
                        func: idx,
                        captures: capture_ids,
                    },
                );
                Ok(())
            }
            hir::RValue::RecClosure {
                name, params, body, ..
            } => {
                let lname = format!("__rec_{}", name);
                let idx = self.ctx.reserve_function(&lname);
                // Like ordinary closures, recursive closures capture the
                // enclosing locals they use: free vars of the body minus
                // params and the self name, filtered to what is actually
                // in scope here.
                let mut exclude: HashSet<String> = params.iter().map(|(n, _)| n.clone()).collect();
                exclude.insert(name.clone());
                let capture_names: Vec<String> = hir_body_used_vars(body, &exclude)
                    .into_iter()
                    .filter(|n| self.lookup(n).is_some())
                    .collect();
                let capture_ids: Vec<mir::LocalId> = capture_names
                    .iter()
                    .map(|n| self.lookup(n).expect("capture just resolved"))
                    .collect();
                let lifted = lower_lifted(
                    self.ctx,
                    &lname,
                    params,
                    &capture_names,
                    Some((name, idx)),
                    body,
                )?;
                self.ctx.fill_function(idx, lifted);
                if capture_ids.is_empty() {
                    // No captures: the binding holds the function-table
                    // index as a value (callable like any function value).
                    self.b
                        .assign(dst, mir::RValue::Const(Constant::Int(idx as i64)));
                } else {
                    // With captures the binding must be a real closure so
                    // the captured environment travels with every call.
                    self.b.assign(
                        dst,
                        mir::RValue::Closure {
                            func: idx,
                            captures: capture_ids,
                        },
                    );
                }
                Ok(())
            }
            hir::RValue::Tuple(elems, _) => {
                let mut ids = Vec::with_capacity(elems.len());
                for e in elems {
                    ids.push(self.lower_operand(e)?);
                }
                self.b.assign(dst, mir::RValue::Tuple(ids));
                Ok(())
            }
            hir::RValue::Record(fields, _) => {
                let mut fs = Vec::with_capacity(fields.len());
                for (n, e) in fields {
                    fs.push((n.clone(), self.lower_operand(e)?));
                }
                self.b.assign(dst, mir::RValue::Record(fs));
                Ok(())
            }
            hir::RValue::RecordUpdate {
                base, overrides, ..
            } => {
                let base_id = self.lower_operand(base)?;
                let mut ov = Vec::with_capacity(overrides.len());
                for (n, e) in overrides {
                    ov.push((n.clone(), self.lower_operand(e)?));
                }
                self.b.assign(
                    dst,
                    mir::RValue::RecordUpdate {
                        base: base_id,
                        overrides: ov,
                    },
                );
                Ok(())
            }
            hir::RValue::Array(elems, _) => {
                let mut ids = Vec::with_capacity(elems.len());
                for e in elems {
                    ids.push(self.lower_operand(e)?);
                }
                self.b.assign(dst, mir::RValue::ArrayLit(ids));
                Ok(())
            }
            hir::RValue::FieldAccess { base, field, .. } if operand_is_self(base) => {
                self.b.assign(
                    dst,
                    mir::RValue::StateGet {
                        field: field.clone(),
                    },
                );
                Ok(())
            }
            hir::RValue::FieldAccess { base, field, .. } => {
                let obj = self.lower_operand(base)?;
                // Numeric field access (t.0, t.1) → positional tuple load.
                // Non-numeric (r.x) → named record field load.
                if let Ok(index) = field.parse::<u8>() {
                    self.b.assign(dst, mir::RValue::LoadFieldPos { obj, index });
                } else {
                    self.b.assign(
                        dst,
                        mir::RValue::LoadFieldNamed {
                            obj,
                            field: field.clone(),
                        },
                    );
                }
                Ok(())
            }
            hir::RValue::Index { base, idx, .. } => {
                let arr = self.lower_operand(base)?;
                let idx_id = self.lower_operand(idx)?;
                self.b
                    .assign(dst, mir::RValue::ArrayLoad { arr, idx: idx_id });
                Ok(())
            }
            hir::RValue::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                let cid = self.lower_operand(cond)?;
                let then_bb = self.b.create_block();
                let else_bb = self.b.create_block();
                let join = self.b.create_block();
                self.b.terminate(mir::Terminator::Branch {
                    cond: cid,
                    then_: then_bb,
                    else_: else_bb,
                });

                self.b.switch_to(then_bb);
                self.push_scope();
                self.lower_body_into(then_body, dst, join)?;
                self.pop_scope();

                self.b.switch_to(else_bb);
                match else_body {
                    Some(eb) => {
                        self.push_scope();
                        self.lower_body_into(eb, dst, join)?;
                        self.pop_scope();
                    }
                    None => {
                        self.b.assign(dst, mir::RValue::Const(Constant::Unit));
                        self.b.terminate(mir::Terminator::Jump(join));
                    }
                }

                self.b.switch_to(join);
                Ok(())
            }
            hir::RValue::Match {
                scrutinee, arms, ..
            } => self.lower_match(dst, scrutinee, arms),
            hir::RValue::For {
                var,
                iterable,
                body,
            } => self.lower_for(dst, var, iterable, body),
            hir::RValue::While { cond, body, .. } => self.lower_while(dst, cond, body),
            hir::RValue::Block(body) => {
                let join = self.b.create_block();
                self.push_scope();
                self.lower_body_into(body, dst, join)?;
                self.pop_scope();
                self.b.switch_to(join);
                Ok(())
            }
            hir::RValue::Perform {
                effect, op, args, ..
            } => {
                // Mirror the stable compiler's special cases.
                if effect == "Inference" && op == "ask" {
                    let prompt = match args.first() {
                        Some(a) => self.lower_operand(a)?,
                        None => self.unit_temp(),
                    };
                    self.b.assign(
                        dst,
                        mir::RValue::PerformAsync {
                            effect_op: format!("Inference.ask"),
                            args: vec![prompt],
                            resolved_handler: None,
                        },
                    );
                    return Ok(());
                }
                // `perform Provider.ask("llm", prompt)` is the longevity-path
                // equivalent of `perform LLM.ask(prompt)`: the vocabulary
                // references an eternal "provider" abstraction, not a
                // transient technology. Lower it to the generic PerformAsync
                // path. Non-"llm" providers fall through to the generic
                // Perform path.
                if effect == "Provider" && op == "ask" && args.len() == 2 {
                    if let Some(hir::Operand::Literal(crate::ast::Literal::String(name), _)) =
                        args.first()
                    {
                        if name == "llm" {
                            let prompt = self.lower_operand(&args[1])?;
                            self.b.assign(
                                dst,
                                mir::RValue::PerformAsync {
                                    effect_op: format!("Inference.ask"),
                                    args: vec![prompt],
                                    resolved_handler: None,
                                },
                            );
                            return Ok(());
                        }
                    }
                }
                // 1-arg Timer.sleep(ms) routes through PerformAsync for suspension.
                // 2-arg Timer.sleep(name, ms) stays as Perform for workflow timers.
                if effect == "Timer" && op == "sleep" && args.len() == 1 {
                    let ms = self.lower_operand(&args[0])?;
                    self.b.assign(
                        dst,
                        mir::RValue::PerformAsync {
                            effect_op: format!("Timer.sleep"),
                            args: vec![ms],
                            resolved_handler: None,
                        },
                    );
                    return Ok(());
                }
                if effect == "Signal" && op == "wait" {
                    if let Some(hir::Operand::Literal(crate::ast::Literal::String(name), _)) =
                        args.first()
                    {
                        self.b
                            .assign(dst, mir::RValue::SignalWait { name: name.clone() });
                        return Ok(());
                    }
                }
                let mut ids = Vec::with_capacity(args.len());
                for a in args {
                    ids.push(self.lower_operand(a)?);
                }
                // Resolve this perform to a statically-known handler if one
                // is in scope.  Search the handler scope stack from the
                // innermost (most recent) to outermost.
                let qualified = format!("{}.{}", effect, op);
                let resolved = self
                    .handler_scope
                    .iter()
                    .rev()
                    .find_map(|(table_idx, bindings)| {
                        bindings.iter().rev().find_map(|(binding_idx, name)| {
                            if name == &qualified {
                                Some(mir::HandlerRef {
                                    table_index: *table_idx as u32,
                                    binding_index: *binding_idx as u32,
                                })
                            } else {
                                None
                            }
                        })
                    });
                self.b.assign(
                    dst,
                    mir::RValue::Perform {
                        effect: effect.clone(),
                        op: op.clone(),
                        args: ids,
                        resolved_handler: resolved,
                    },
                );
                Ok(())
            }
            hir::RValue::Resume { value, .. } => {
                let val = self.lower_operand(value)?;
                self.b.assign(dst, mir::RValue::Resume(val));
                Ok(())
            }
            hir::RValue::Handle { body, handlers, .. } => self.lower_handle(dst, body, handlers),
            hir::RValue::Migrate { actor, node, .. } => {
                let a = self.lower_operand(actor)?;
                let n = self.lower_operand(node)?;
                self.b
                    .assign(dst, mir::RValue::Migrate { actor: a, node: n });
                Ok(())
            }
            hir::RValue::CapCheck { operand, .. } => {
                let id = self.lower_operand(operand)?;
                self.b.assign(dst, mir::RValue::CapabilityCheck { val: id });
                Ok(())
            }
            hir::RValue::SelfRef(_) => {
                self.b.assign(dst, mir::RValue::SelfRef);
                Ok(())
            }
            hir::RValue::FFICall { symbol, args, .. } => {
                let idx = self.ctx.extern_map.get(symbol).copied().ok_or_else(|| {
                    compile_err(
                        format!("unknown extern function '{}'", symbol),
                        Span::default(),
                    )
                })?;
                let mut ids = Vec::with_capacity(args.len());
                for a in args {
                    ids.push(self.lower_operand(a)?);
                }
                self.b.assign(dst, mir::RValue::FFICall { idx, args: ids });
                Ok(())
            }
            hir::RValue::Spawn {
                actor_type,
                init,
                target_node,
                capabilities,
                ..
            } => {
                let idx = self.ctx.spawn_behavior_idx(actor_type);
                let init_rvs: Vec<(String, mir::RValue)> = init
                    .iter()
                    .map(|(name, op)| match op {
                        hir::Operand::Literal(lit, _) => {
                            (name.clone(), mir::RValue::Const(literal_to_constant(lit)))
                        }
                        _ => {
                            let local = self.lower_operand(op).unwrap_or_else(|_| self.unit_temp());
                            (name.clone(), mir::RValue::Load(local))
                        }
                    })
                    .collect();
                let target_local = target_node
                    .as_ref()
                    .map(|op| self.lower_operand(op))
                    .transpose()?;
                self.b.assign(
                    dst,
                    mir::RValue::Spawn {
                        behavior_idx: idx,
                        init: init_rvs,
                        target_node: target_local,
                        capabilities: capabilities.clone(),
                    },
                );
                Ok(())
            }
            hir::RValue::Send {
                actor,
                behavior,
                args,
                remote,
                ..
            } => {
                let actor_hint = operand_name_hint(actor);
                let idx = self.ctx.send_behavior_idx(&actor_hint, behavior);
                let actor_id = self.lower_operand(actor)?;
                let mut arg_ids = Vec::with_capacity(args.len());
                for a in args {
                    arg_ids.push(self.lower_operand(a)?);
                }
                self.b.assign(
                    dst,
                    mir::RValue::Send {
                        actor: actor_id,
                        behavior_idx: idx,
                        args: arg_ids,
                        remote: *remote,
                    },
                );
                Ok(())
            }
            hir::RValue::Ask {
                actor,
                behavior,
                args,
                remote,
                timeout_ms,
                ..
            } => {
                let actor_hint = operand_name_hint(actor);
                let idx = self.ctx.send_behavior_idx(&actor_hint, behavior);
                let actor_id = self.lower_operand(actor)?;
                let mut arg_ids = Vec::with_capacity(args.len());
                for a in args {
                    arg_ids.push(self.lower_operand(a)?);
                }
                self.b.assign(
                    dst,
                    mir::RValue::Ask {
                        actor: actor_id,
                        behavior_idx: idx,
                        args: arg_ids,
                        remote: *remote,
                        timeout_ms: *timeout_ms,
                    },
                );
                Ok(())
            }
            hir::RValue::PipelineNew { .. } => {
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Pipeline.new".to_string(),
                        args: vec![],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::PipelineStage {
                id,
                name,
                actor,
                template,
                ..
            } => {
                let i = self.lower_operand(id)?;
                let n = self.lower_operand(name)?;
                let a = self.lower_operand(actor)?;
                let t = self.lower_operand(template)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Pipeline.stage".to_string(),
                        args: vec![i, n, a, t],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::PipelineRun { id, input, .. } => {
                let i = self.lower_operand(id)?;
                let inp = self.lower_operand(input)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Pipeline.run".to_string(),
                        args: vec![i, inp],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::SupervisorNew { .. } => {
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Supervisor.new".to_string(),
                        args: vec![],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::SupervisorWorker {
                id,
                name,
                actor,
                description,
                ..
            } => {
                let i = self.lower_operand(id)?;
                let n = self.lower_operand(name)?;
                let a = self.lower_operand(actor)?;
                let d = self.lower_operand(description)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Supervisor.worker".to_string(),
                        args: vec![i, n, a, d],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::SupervisorRun { id, task, .. } => {
                let i = self.lower_operand(id)?;
                let t = self.lower_operand(task)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Supervisor.run".to_string(),
                        args: vec![i, t],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::DebateNew {
                topic,
                rounds,
                threshold,
                ..
            } => {
                let top = self.lower_operand(topic)?;
                let r = self.lower_operand(rounds)?;
                let th = self.lower_operand(threshold)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Debate.new".to_string(),
                        args: vec![top, r, th],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::DebateParticipant {
                id,
                name,
                stance,
                actor,
                ..
            } => {
                let i = self.lower_operand(id)?;
                let n = self.lower_operand(name)?;
                let s = self.lower_operand(stance)?;
                let a = self.lower_operand(actor)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Debate.participant".to_string(),
                        args: vec![i, n, s, a],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::DebateRun { id, .. } => {
                let i = self.lower_operand(id)?;
                self.b.assign(
                    dst,
                    mir::RValue::PerformAsync {
                        effect_op: "Debate.run".to_string(),
                        args: vec![i],
                        resolved_handler: None,
                    },
                );
                Ok(())
            }
            hir::RValue::Receive { arms, after, .. } => self.lower_receive(dst, arms, after),
        }
    }

    // -- Control flow constructs ----------------------------------------------

    /// Lower `receive { | Behavior(pat, ...) if guard => expr ... }` to
    /// selective receive dispatch with pattern matching and guards.
    ///
    /// A `ReceiveMatch`/`ReceiveWait` rvalue scans the mailbox for the first
    /// message whose behavior id matches any arm. Then a compare chain over
    /// the matched arm index selects the arm body. For each arm, the payload
    /// temps are tested against the arm's patterns (using `pattern_test`),
    /// and if a guard is present, it is evaluated. If all tests pass, the
    /// arm's pattern variables are bound (via `bind_pattern`) and a
    /// `ReceiveCommit` rvalue removes the matched message from the
    /// skip-buffer. If any test fails, control falls through to a retry
    /// block that re-emits the `ReceiveMatch` scan, which skips the
    /// already-tried message in the skip-buffer.
    ///
    /// When nothing matches (scan returns None → arm-count sentinel), the
    /// no-match block runs the legacy pop-any `Receive` or, with `after`,
    /// the timeout body.
    fn lower_receive(
        &mut self,
        dst: mir::LocalId,
        arms: &[(String, Vec<Pattern>, Option<Box<hir::Body>>, Box<hir::Body>)],
        after: &Option<(Box<hir::Body>, Box<hir::Body>)>,
    ) -> NuResult<()> {
        use crate::bytecode::Constant;
        if arms.is_empty() && after.is_none() {
            self.b.assign(dst, mir::RValue::Receive);
            return Ok(());
        }
        let behavior_ids: Vec<u16> = arms
            .iter()
            .map(|(name, _, _, _)| self.ctx.send_behavior_idx("", name) as u16)
            .collect();
        let max_params = arms.iter().map(|(_, p, _, _)| p.len()).max().unwrap_or(0);
        let timeout = match after {
            Some((ms_body, _)) => {
                let timeout = self.b.add_temp(Type::int());
                let cont = self.b.create_block();
                self.lower_body_into(ms_body, timeout, cont)?;
                self.b.switch_to(cont);
                Some(timeout)
            }
            None => None,
        };
        let arm_idx = self.b.add_temp(Type::int());
        let payload_temps: Vec<mir::LocalId> = (0..max_params)
            .map(|_| self.b.add_temp(Type::unit()))
            .collect();
        let join = self.b.create_block();
        let no_match = self.b.create_block();
        // The scan block is emitted once; pattern/guard failure jumps back to
        // it for a retry. The skip-buffer skips already-tried messages.
        let scan = self.b.create_block();
        // Jump from the entry block to the scan block (the scan block
        // re-executes on pattern/guard retry).
        self.b.terminate(mir::Terminator::Jump(scan));
        self.b.switch_to(scan);
        match timeout {
            Some(timeout) => self.b.assign(
                arm_idx,
                mir::RValue::ReceiveWait {
                    behavior_ids: behavior_ids.clone(),
                    max_params,
                    timeout,
                },
            ),
            None => self.b.assign(
                arm_idx,
                mir::RValue::ReceiveMatch {
                    behavior_ids: behavior_ids.clone(),
                    max_params,
                },
            ),
        }
        // Compare chain: dispatch on arm_idx. For each arm, test patterns +
        // guard. On pass: commit + bind + arm body -> join. On fail: jump to
        // `scan` (retry). The last arm's else_ goes to `no_match`.
        for (i, (_name, patterns, guard, arm_body)) in arms.iter().enumerate() {
            let test = self.b.add_temp(Type::bool());
            let idx_const = self.b.add_temp(Type::int());
            self.b
                .assign(idx_const, mir::RValue::Const(Constant::Int(i as i64)));
            self.b.assign(
                test,
                mir::RValue::Binary(crate::ast::BinOp::Eq, arm_idx, idx_const),
            );
            let arm_bb = self.b.create_block();
            let next_bb = if i == arms.len() - 1 {
                no_match
            } else {
                self.b.create_block()
            };
            self.b.terminate(mir::Terminator::Branch {
                cond: test,
                then_: arm_bb,
                else_: next_bb,
            });
            self.b.switch_to(arm_bb);
            // Bind pattern variables to payload temps first (guards reference
            // them). The pattern_test calls above already verified the
            // structural match; bind_pattern binds the variables for use in
            // the guard and arm body.
            self.push_scope();
            for (pat, &temp) in patterns.iter().zip(payload_temps.iter()) {
                self.bind_pattern(pat, temp);
            }
            // Test payload patterns. Each pattern slot is tested against its
            // corresponding payload temp. All must pass.
            let pat_tests: Vec<mir::LocalId> = patterns
                .iter()
                .enumerate()
                .map(|(j, pat)| self.pattern_test(pat, payload_temps[j]).unwrap())
                .collect();
            let pass_bb = self.b.create_block();
            // AND all pattern tests + guard into a single bool.
            let mut acc = if pat_tests.is_empty() {
                let t = self.b.add_temp(Type::bool());
                self.b.assign(t, mir::RValue::Const(Constant::Bool(true)));
                t
            } else {
                pat_tests[0]
            };
            for &pt in pat_tests.get(1..).unwrap_or(&[]) {
                let combined = self.b.add_temp(Type::bool());
                self.b.assign(
                    combined,
                    mir::RValue::Binary(crate::ast::BinOp::And, acc, pt),
                );
                acc = combined;
            }
            if let Some(guard_body) = guard {
                let guard_result = self.b.add_temp(Type::bool());
                let guard_cont = self.b.create_block();
                self.lower_body_into(guard_body, guard_result, guard_cont)?;
                self.b.switch_to(guard_cont);
                let combined = self.b.add_temp(Type::bool());
                self.b.assign(
                    combined,
                    mir::RValue::Binary(crate::ast::BinOp::And, acc, guard_result),
                );
                acc = combined;
            }
            self.b.terminate(mir::Terminator::Branch {
                cond: acc,
                then_: pass_bb,
                else_: scan, // On failure, retry the scan.
            });
            self.b.switch_to(pass_bb);
            // Commit: remove the matched message from the skip-buffer.
            let _commit = self.b.add_temp(Type::unit());
            self.b.assign(_commit, mir::RValue::ReceiveCommit);
            self.lower_body_into(arm_body, dst, join)?;
            self.pop_scope();
            self.b.switch_to(next_bb);
        }
        // No-match block.
        if arms.is_empty() {
            self.b.terminate(mir::Terminator::Jump(no_match));
        }
        self.b.switch_to(no_match);
        match after {
            Some((_, timeout_body)) => {
                self.lower_body_into(timeout_body, dst, join)?;
            }
            None => {
                self.b.assign(dst, mir::RValue::Receive);
                self.b.terminate(mir::Terminator::Jump(join));
            }
        }
        self.b.switch_to(join);
        Ok(())
    }

    fn lower_match(
        &mut self,
        dst: mir::LocalId,
        scrutinee: &hir::Operand,
        arms: &[(Pattern, Option<Box<hir::Body>>, Box<hir::Body>)],
    ) -> NuResult<()> {
        use crate::bytecode::Constant;
        let sid = self.lower_operand(scrutinee)?;
        if arms.is_empty() {
            self.b.assign(dst, mir::RValue::Const(Constant::Unit));
            return Ok(());
        }
        let join = self.b.create_block();

        for (i, (pat, guard, arm_body)) in arms.iter().enumerate() {
            let is_last = i == arms.len() - 1;
            if is_last && guard.is_none() && matches!(pat, Pattern::Wild | Pattern::Var(_)) {
                // An irrefutable, unguarded last pattern (wildcard or
                // variable binding) is a catch-all: enter it
                // unconditionally (mirrors the stable compiler's fallback
                // semantics). A guarded last arm is NOT a catch-all — when
                // its guard fails the match is non-exhaustive and takes the
                // error path below.
                self.push_scope();
                self.bind_pattern(pat, sid);
                self.lower_body_into(arm_body, dst, join)?;
                self.pop_scope();
            } else {
                let test = self.pattern_test(pat, sid)?;
                let arm_bb = self.b.create_block();
                let next_bb = self.b.create_block();
                self.b.terminate(mir::Terminator::Branch {
                    cond: test,
                    then_: arm_bb,
                    else_: next_bb,
                });
                self.b.switch_to(arm_bb);
                self.push_scope();
                self.bind_pattern(pat, sid);
                if let Some(guard_body) = guard {
                    // The pattern matched: evaluate the guard with the
                    // pattern's bindings in scope, then branch to the arm
                    // body on true or fall through to the next arm on false.
                    let guard_val = self.b.add_temp(Type::bool());
                    let guard_join = self.b.create_block();
                    self.lower_body_into(guard_body, guard_val, guard_join)?;
                    self.b.switch_to(guard_join);
                    let body_bb = self.b.create_block();
                    self.b.terminate(mir::Terminator::Branch {
                        cond: guard_val,
                        then_: body_bb,
                        else_: next_bb,
                    });
                    self.b.switch_to(body_bb);
                }
                self.lower_body_into(arm_body, dst, join)?;
                self.pop_scope();
                self.b.switch_to(next_bb);
                if is_last {
                    // Final else-edge: no arm matched. A refutable last
                    // pattern means the match can be non-exhaustive; fail
                    // with a runtime error instead of silently running the
                    // last arm. Raised as a perform of a reserved effect
                    // name that no source-declared handler can intercept,
                    // so the VM reports it as an unhandled effect.
                    self.b.assign(
                        dst,
                        mir::RValue::Perform {
                            effect: "non-exhaustive match".to_string(),
                            op: "raise".to_string(),
                            args: Vec::new(),
                            resolved_handler: None,
                        },
                    );
                    self.b.terminate(mir::Terminator::Jump(join));
                }
            }
        }

        self.b.switch_to(join);
        Ok(())
    }

    fn pattern_test(&mut self, pat: &Pattern, sid: mir::LocalId) -> NuResult<mir::LocalId> {
        use crate::bytecode::Constant;
        let dst = self.b.add_temp(Type::bool());
        match pat {
            Pattern::Wild | Pattern::Var(_) => {
                self.b.assign(dst, mir::RValue::Const(Constant::Bool(true)));
            }
            Pattern::Lit(lit) => {
                let lit_id = self.b.add_temp(Type::unit());
                self.b
                    .assign(lit_id, mir::RValue::Const(literal_to_constant(lit)));
                self.b
                    .assign(dst, mir::RValue::Binary(crate::ast::BinOp::Eq, sid, lit_id));
            }
            Pattern::Variant(tag, payload) => {
                // Runtime representation: a payload-less constructor is the
                // bare tag string; a payload-carrying constructor is a
                // record `{ ctor: <name>, payload: <value> }` (records are
                // the only heap values MIR can both construct and
                // destructure field-wise — see bind_pattern; the field is
                // `ctor` rather than `tag` because `tag` is a keyword and
                // could never appear in a source record literal). Match the
                // tag accordingly.
                let scrut_tag = if payload.is_some() {
                    let t = self.b.add_temp(Type::unit());
                    self.b.assign(
                        t,
                        mir::RValue::LoadFieldNamed {
                            obj: sid,
                            field: "ctor".to_string(),
                        },
                    );
                    t
                } else {
                    sid
                };
                let tag_id = self.b.add_temp(Type::unit());
                self.b
                    .assign(tag_id, mir::RValue::Const(Constant::String(tag.clone())));
                match payload {
                    Some(inner) => {
                        // Nested pattern: the outer tag must match AND the
                        // inner pattern must match the payload value, so
                        // `Some(Some(x))` rejects both `Some(None)` (inner
                        // tag test fails on the bare-string payload) and
                        // `None` (outer tag test fails on the nil `ctor`
                        // field load).
                        let tag_ok = self.b.add_temp(Type::bool());
                        self.b
                            .assign(tag_ok, mir::RValue::StringEq(scrut_tag, tag_id));
                        let payload_val = self.b.add_temp(Type::unit());
                        self.b.assign(
                            payload_val,
                            mir::RValue::LoadFieldNamed {
                                obj: sid,
                                field: "payload".to_string(),
                            },
                        );
                        let inner_ok = self.pattern_test(inner, payload_val)?;
                        self.b.assign(
                            dst,
                            mir::RValue::Binary(crate::ast::BinOp::And, tag_ok, inner_ok),
                        );
                    }
                    None => {
                        self.b.assign(dst, mir::RValue::StringEq(scrut_tag, tag_id));
                    }
                }
            }
            Pattern::Tuple(pats) => {
                // Structural tuple matching: each position loads its field
                // from the scrutinee (FieldL via LoadFieldPos — tuples are
                // fixed-size heap arrays indexed by position, see the
                // RValue::Tuple codegen) and the sub-pattern is tested
                // recursively; all position tests are AND-ed. An empty
                // tuple pattern matches vacuously.
                //
                // Arity is deliberately NOT checked: a 2-element pattern
                // also matches a 3-element tuple (extra elements are
                // ignored), and a position beyond the scrutinee's length
                // loads nil (FieldL clamps out-of-range reads), which
                // literal sub-patterns reject and variable sub-patterns
                // bind. There is no cheap arity probe — ArrLen reports 0
                // for tuples (its callback only measures
                // HeapTypeTag::Array) and no tuple-length opcode exists —
                // so an arity test would require a new VM opcode.
                if pats.is_empty() {
                    self.b.assign(dst, mir::RValue::Const(Constant::Bool(true)));
                } else {
                    let mut acc: Option<mir::LocalId> = None;
                    for (i, sub) in pats.iter().enumerate() {
                        let elem = self.b.add_temp(Type::unit());
                        self.b.assign(
                            elem,
                            mir::RValue::LoadFieldPos {
                                obj: sid,
                                index: i as u8,
                            },
                        );
                        let t = self.pattern_test(sub, elem)?;
                        acc = Some(match acc {
                            None => t,
                            Some(prev) => {
                                let and_t = self.b.add_temp(Type::bool());
                                self.b.assign(
                                    and_t,
                                    mir::RValue::Binary(crate::ast::BinOp::And, prev, t),
                                );
                                and_t
                            }
                        });
                    }
                    self.b.assign(dst, mir::RValue::Load(acc.unwrap()));
                }
            }
            Pattern::Record(fields) => {
                // Structural record matching: load each named field
                // (LoadFieldNamed/RecL — records are flat arrays indexed by
                // module-wide field ids) and recursively test the
                // sub-pattern; all field tests are AND-ed. Fields the
                // pattern does not name are ignored, so a pattern matches
                // any record that carries at least its named fields; a
                // missing field loads nil (RecL clamps), which literal
                // sub-patterns reject and variable sub-patterns bind. An
                // empty record pattern matches vacuously.
                if fields.is_empty() {
                    self.b.assign(dst, mir::RValue::Const(Constant::Bool(true)));
                } else {
                    let mut acc: Option<mir::LocalId> = None;
                    for (field, sub) in fields {
                        let v = self.b.add_temp(Type::unit());
                        self.b.assign(
                            v,
                            mir::RValue::LoadFieldNamed {
                                obj: sid,
                                field: field.clone(),
                            },
                        );
                        let t = self.pattern_test(sub, v)?;
                        acc = Some(match acc {
                            None => t,
                            Some(prev) => {
                                let and_t = self.b.add_temp(Type::bool());
                                self.b.assign(
                                    and_t,
                                    mir::RValue::Binary(crate::ast::BinOp::And, prev, t),
                                );
                                and_t
                            }
                        });
                    }
                    self.b.assign(dst, mir::RValue::Load(acc.unwrap()));
                }
            }
            Pattern::Alias(_, inner) => {
                return self.pattern_test(inner, sid);
            }
        }
        Ok(dst)
    }

    fn bind_pattern(&mut self, pat: &Pattern, sid: mir::LocalId) {
        match pat {
            Pattern::Var(name) => self.bind(name, sid),
            Pattern::Alias(name, inner) => {
                self.bind(name, sid);
                self.bind_pattern(inner, sid);
            }
            Pattern::Variant(_, Some(inner)) => {
                // Payload-carrying variants are `{ ctor, payload }` records
                // (see pattern_test): bind the inner pattern to the payload
                // field's value, not to the whole scrutinee record. RecL on
                // a non-record scrutinee yields nil, so a value that matched
                // only because the tag field compared equal still binds
                // safely.
                let payload = self.b.add_temp(Type::unit());
                self.b.assign(
                    payload,
                    mir::RValue::LoadFieldNamed {
                        obj: sid,
                        field: "payload".to_string(),
                    },
                );
                self.bind_pattern(inner, payload);
            }
            Pattern::Tuple(pats) => {
                // Bind each element sub-pattern to its positional field
                // load (FieldL — see pattern_test for the arity caveat: an
                // out-of-range position binds nil).
                for (i, sub) in pats.iter().enumerate() {
                    let elem = self.b.add_temp(Type::unit());
                    self.b.assign(
                        elem,
                        mir::RValue::LoadFieldPos {
                            obj: sid,
                            index: i as u8,
                        },
                    );
                    self.bind_pattern(sub, elem);
                }
            }
            Pattern::Record(fields) => {
                // Bind each field sub-pattern to its named field load
                // (LoadFieldNamed/RecL; a missing field binds nil).
                for (field, sub) in fields {
                    let v = self.b.add_temp(Type::unit());
                    self.b.assign(
                        v,
                        mir::RValue::LoadFieldNamed {
                            obj: sid,
                            field: field.clone(),
                        },
                    );
                    self.bind_pattern(sub, v);
                }
            }
            _ => {}
        }
    }

    fn lower_for(
        &mut self,
        dst: mir::LocalId,
        var: &str,
        iterable: &hir::Operand,
        body: &hir::Body,
    ) -> NuResult<()> {
        use crate::bytecode::Constant;
        let iter = self.lower_operand(iterable)?;
        let len = self.b.add_temp(Type::int());
        self.b.assign(len, mir::RValue::ArrayLen(iter));
        let idx = self.b.add_temp(Type::int());
        self.b.assign(idx, mir::RValue::Const(Constant::Int(0)));
        let one = self.b.add_temp(Type::int());
        self.b.assign(one, mir::RValue::Const(Constant::Int(1)));

        let head = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();

        let loop_result = self.b.add_temp(Type::unit());
        self.b
            .assign(loop_result, mir::RValue::Const(Constant::Int(0)));

        self.b.terminate(mir::Terminator::Jump(head));

        self.b.switch_to(head);
        let cond = self.b.add_temp(Type::bool());
        self.b
            .assign(cond, mir::RValue::Binary(crate::ast::BinOp::Lt, idx, len));
        self.b.terminate(mir::Terminator::Branch {
            cond,
            then_: body_bb,
            else_: exit,
        });

        self.b.switch_to(body_bb);
        let elem = self.b.add_temp(Type::unit());
        self.b
            .assign(elem, mir::RValue::ArrayLoad { arr: iter, idx });
        self.push_scope();
        self.bind(var, elem);
        self.loop_exits.push(exit);
        self.loop_results.push(loop_result);
        for stmt in &body.stmts {
            self.lower_stmt(stmt)?;
        }
        if !self.b.is_terminated() {
            match &body.terminator {
                hir::Terminator::Yield(_) => {
                    self.b
                        .assign(idx, mir::RValue::Binary(crate::ast::BinOp::Add, idx, one));
                    self.b.terminate(mir::Terminator::Jump(head));
                }
                hir::Terminator::FnReturn(op) => {
                    let id = match op {
                        Some(op) => self.lower_operand(op)?,
                        None => self.unit_temp(),
                    };
                    self.emit_return(id);
                }
                hir::Terminator::Break(op) => {
                    match op {
                        Some(op) => {
                            let val = self.lower_operand(op)?;
                            self.b.assign(loop_result, mir::RValue::Load(val));
                        }
                        None => {
                            self.b
                                .assign(loop_result, mir::RValue::Const(Constant::Unit));
                        }
                    }
                    self.b.terminate(mir::Terminator::Jump(exit));
                }
            }
        }
        self.loop_exits.pop();
        self.loop_results.pop();
        self.pop_scope();

        self.b.switch_to(exit);
        self.b.assign(dst, mir::RValue::Load(loop_result));
        Ok(())
    }

    fn lower_while(
        &mut self,
        dst: mir::LocalId,
        cond: &hir::Body,
        body: &hir::Body,
    ) -> NuResult<()> {
        use crate::bytecode::Constant;
        let head = self.b.create_block();
        let body_bb = self.b.create_block();
        let exit = self.b.create_block();

        let loop_result = self.b.add_temp(Type::unit());
        self.b
            .assign(loop_result, mir::RValue::Const(Constant::Int(0)));

        self.b.terminate(mir::Terminator::Jump(head));

        self.b.switch_to(head);
        // Lower cond body into head
        for stmt in &cond.stmts {
            self.lower_stmt(stmt)?;
        }
        let c = match &cond.terminator {
            hir::Terminator::Yield(op) => self.lower_operand(op)?,
            hir::Terminator::FnReturn(_) | hir::Terminator::Break(_) => {
                let dummy = self.b.add_temp(Type::bool());
                self.b
                    .assign(dummy, mir::RValue::Const(Constant::Bool(false)));
                dummy
            }
        };

        if !self.b.is_terminated() {
            self.b.terminate(mir::Terminator::Branch {
                cond: c,
                then_: body_bb,
                else_: exit,
            });
        }

        self.b.switch_to(body_bb);
        self.push_scope();
        self.loop_exits.push(exit);
        self.loop_results.push(loop_result);
        for stmt in &body.stmts {
            self.lower_stmt(stmt)?;
        }
        if !self.b.is_terminated() {
            match &body.terminator {
                hir::Terminator::Yield(_) => {
                    self.b.terminate(mir::Terminator::Jump(head));
                }
                hir::Terminator::FnReturn(op) => {
                    let id = match op {
                        Some(op) => self.lower_operand(op)?,
                        None => self.unit_temp(),
                    };
                    self.emit_return(id);
                }
                hir::Terminator::Break(op) => {
                    match op {
                        Some(op) => {
                            let val = self.lower_operand(op)?;
                            self.b.assign(loop_result, mir::RValue::Load(val));
                        }
                        None => {
                            self.b
                                .assign(loop_result, mir::RValue::Const(Constant::Unit));
                        }
                    }
                    self.b.terminate(mir::Terminator::Jump(exit));
                }
            }
        }
        self.loop_exits.pop();
        self.loop_results.pop();
        self.pop_scope();

        self.b.switch_to(exit);
        self.b.assign(dst, mir::RValue::Load(loop_result));
        Ok(())
    }

    fn lower_handle(
        &mut self,
        dst: mir::LocalId,
        body: &hir::Body,
        handlers: &[hir::EffectHandler],
    ) -> NuResult<()> {
        let join = self.b.create_block();
        let table_idx = self.b.add_handler_table(mir::HandlerTableDef {
            bindings: Vec::new(),
        });
        self.b.emit(mir::Stmt::EnterHandle { table: table_idx });
        // Push handler scope with pre-scanned binding entries so that
        // `Perform` operations inside the body (lowered next) can resolve
        // to statically-known `HandlerRef` values.
        let scope_bindings: Vec<(usize, String)> = handlers
            .iter()
            .enumerate()
            .map(|(i, h)| (i, format!("{}.{}", h.effect_name, h.op_name)))
            .collect();
        self.handler_scope.push((table_idx, scope_bindings));
        // The depth is tracked so a `return` inside the body (or inside
        // nested branches/loops) unwinds every frame still on the stack —
        // see emit_return.
        self.handle_depth += 1;
        for stmt in &body.stmts {
            self.lower_stmt(stmt)?;
        }
        if !self.b.is_terminated() {
            match &body.terminator {
                hir::Terminator::Yield(op) => {
                    let id = self.lower_operand(op)?;
                    self.b.assign(dst, mir::RValue::Load(id));
                    self.b.emit(mir::Stmt::PopHandler);
                    self.b.terminate(mir::Terminator::Jump(join));
                }
                hir::Terminator::FnReturn(op) => {
                    let id = match op {
                        Some(op) => self.lower_operand(op)?,
                        None => self.unit_temp(),
                    };
                    self.emit_return(id);
                }
                hir::Terminator::Break(_) => {
                    return Err(compile_err(
                        "break out of an effect handler body",
                        Span::default(),
                    ));
                }
            }
        }
        self.handle_depth -= 1;

        // Handler bodies: entered only by the VM's effect dispatch. The
        // handle's frame is still on `handler_stack` while a handler runs,
        // so the depth is bumped here too — a `return` inside a handler
        // body must unwind it (and every enclosing frame) like any other
        // return inside the handled scope. Resuming handlers end with
        // `Resume`; non-resuming (abortive) handlers assign the body value
        // to the handle's dst, pop the frame (discarding the captured
        // continuation), and jump to the handle's join block, so the body
        // value becomes the handle expression's value.
        let mut bindings = Vec::with_capacity(handlers.len());
        for h in handlers {
            let hb = self.b.create_block();
            self.b.switch_to(hb);
            self.handle_depth += 1;
            self.push_scope();
            let mut params = Vec::with_capacity(h.params.len());
            for (pname, pty) in &h.params {
                let id = self.b.add_local(pname.clone(), pty.clone());
                self.bind(pname, id);
                params.push(id);
            }
            for stmt in &h.body.stmts {
                self.lower_stmt(stmt)?;
            }
            if !self.b.is_terminated() {
                match &h.body.terminator {
                    hir::Terminator::Yield(op) => {
                        let id = self.lower_operand(op)?;
                        if h.resume {
                            self.b.terminate(mir::Terminator::Resume(id));
                        } else {
                            self.b.assign(dst, mir::RValue::Load(id));
                            self.b.emit(mir::Stmt::PopHandler);
                            self.b.terminate(mir::Terminator::Jump(join));
                        }
                    }
                    hir::Terminator::FnReturn(op) => {
                        let id = match op {
                            Some(op) => self.lower_operand(op)?,
                            None => self.unit_temp(),
                        };
                        self.emit_return(id);
                    }
                    hir::Terminator::Break(_) => {
                        return Err(compile_err(
                            "break out of an effect handler",
                            Span::default(),
                        ));
                    }
                }
            }
            self.handle_depth -= 1;
            self.pop_scope();
            bindings.push(mir::HandlerBindingDef {
                // Op-qualified ("Effect.op") so the VM dispatches on the exact
                // (effect, op) pair; a perform of `IO.foo` must not match a
                // handler for `IO.bar`. Hand-built modules may still use bare
                // effect names, which the VM matches against any op.
                effect_name: format!("{}.{}", h.effect_name, h.op_name),
                params,
                resume: h.resume,
                single_shot: crate::effect_checker::is_single_shot(&h.body, h.resume),
                body: hb,
            });
        }
        self.b.handler_table_mut(table_idx).bindings = bindings;

        self.handler_scope.pop();
        self.b.switch_to(join);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn place_is_self(place: &hir::Place) -> bool {
    matches!(place, hir::Place::Var(name, _) if name == "self")
}

fn operand_is_self(op: &hir::Operand) -> bool {
    matches!(op, hir::Operand::Var(name, _) if name == "self")
}

/// Best-effort actor-type-name hint for `send`/`ask` behavior resolution,
/// mirroring the stable compiler's `actor_name_from_expr`: only a bare
/// variable reference yields a usable name (the receiver's own binding
/// name), anything else falls back to the empty string.
fn operand_name_hint(op: &hir::Operand) -> String {
    match op {
        hir::Operand::Var(name, _) => name.clone(),
        _ => String::new(),
    }
}

fn literal_to_constant(lit: &crate::ast::Literal) -> crate::bytecode::Constant {
    use crate::ast::Literal;
    use crate::bytecode::Constant;
    match lit {
        Literal::Int(n) => Constant::Int(*n),
        Literal::Float(f) => Constant::Float(*f),
        Literal::String(s) => Constant::String(s.clone()),
        Literal::Bool(b) => Constant::Bool(*b),
        Literal::Nil => Constant::Nil,
        Literal::Unit => Constant::Unit,
    }
}

// ---------------------------------------------------------------------------
// Peephole: fuse temp materialization copies
// ---------------------------------------------------------------------------

/// Fuse `tmp = <rvalue>; x = Load(tmp)` pairs into a single `x = <rvalue>`.
///
/// The HIR pipeline materializes every expression value into a `__tmpN`
/// temporary and binds every `let` through `RValue::Use`, so without this
/// pass named locals are always defined by a non-owning `Load` of a temp
/// whose only use is that same copy. Codegen's drop planning
/// (`plan_drops`) can never prove such a local solely owns its heap value,
/// so no `Drop` is ever emitted and arrays/records accumulate until actor
/// exit. Fusing the pair lets the owning rvalue bind the named local
/// directly, which is exactly the shape the drop analysis needs.
///
/// A pair fuses only when all of these hold:
///   - the two assignments are adjacent in the same block (no intervening
///     side effects could be reordered by moving the rvalue);
///   - the source is a fusable temp (an anonymous MIR temp or a `__tmpN`
///     HIR materialization temp — see `is_fusable_temp`); params, captures,
///     handler params and ordinary named locals keep their copies, since
///     those arrive through uncounted channels and must stay non-owning;
///   - the temp has exactly one use in the whole function — this Load (a
///     second reader would observe the temp's register, which the fusion
///     leaves undefined);
///   - the rvalue does not read the temp itself.
fn fuse_single_use_temps(func: &mut mir::Function) {
    let use_counts = count_local_uses(func);
    let fusable: Vec<bool> = (0..func.locals.len())
        .map(|i| {
            let id = mir::LocalId(i as u32);
            is_fusable_temp(func, id) && use_counts[i] == 1
        })
        .collect();
    for block in &mut func.blocks {
        let mut fused: Vec<mir::Stmt> = Vec::with_capacity(block.stmts.len());
        for stmt in block.stmts.drain(..) {
            let mir::Stmt::Assign {
                dst,
                op: mir::RValue::Load(src),
            } = stmt
            else {
                fused.push(stmt);
                continue;
            };
            let can_fuse = match fused.last() {
                Some(mir::Stmt::Assign {
                    dst: def_dst,
                    op: def_op,
                }) => *def_dst == src && fusable[src.0 as usize] && !rvalue_mentions(def_op, src),
                _ => false,
            };
            if can_fuse {
                let Some(mir::Stmt::Assign { op: def_op, .. }) = fused.pop() else {
                    unreachable!("can_fuse requires an Assign on top");
                };
                fused.push(mir::Stmt::Assign { dst, op: def_op });
            } else {
                fused.push(mir::Stmt::Assign {
                    dst,
                    op: mir::RValue::Load(src),
                });
            }
        }
        block.stmts = fused;
    }
}

/// A local whose only purpose is to materialize an expression value for a
/// single later copy: an anonymous MIR temp (`add_temp`) or one of the
/// `__tmpN` temporaries `hir_lower` materializes every expression into
/// (see `hir_lower::fresh_temp_name`). Params, captures, handler params and
/// ordinary named locals are never fusable — their values arrive through
/// uncounted channels and must stay non-owning.
fn is_fusable_temp(func: &mir::Function, id: mir::LocalId) -> bool {
    match &func.locals[id.0 as usize].name {
        None => true,
        Some(n) => n.starts_with("__tmp"),
    }
}

/// Whole-function use counts per local id (an assignment's destination is a
/// definition, not a use).
fn count_local_uses(func: &mir::Function) -> Vec<usize> {
    let mut counts = vec![0usize; func.locals.len()];
    let mut used: Vec<mir::LocalId> = Vec::new();
    for block in &func.blocks {
        for stmt in &block.stmts {
            match stmt {
                mir::Stmt::Assign { op, .. } => rvalue_use_locals(op, &mut used),
                mir::Stmt::StoreFieldNamed { obj, src, .. } => {
                    used.push(*obj);
                    used.push(*src);
                }
                mir::Stmt::ArrayStore { arr, idx, src } => {
                    used.push(*arr);
                    used.push(*idx);
                    used.push(*src);
                }
                mir::Stmt::EnterHandle { .. } | mir::Stmt::PopHandler => {}
                mir::Stmt::Emit { args, .. } => used.extend(args.iter().copied()),
                mir::Stmt::StateSet { src, .. } => used.push(*src),
            }
            for id in used.drain(..) {
                counts[id.0 as usize] += 1;
            }
        }
        match &block.terminator {
            mir::Terminator::Return(Some(v)) | mir::Terminator::Resume(v) => {
                counts[v.0 as usize] += 1
            }
            mir::Terminator::Branch { cond, .. } => counts[cond.0 as usize] += 1,
            _ => {}
        }
    }
    counts
}

fn rvalue_mentions(op: &mir::RValue, id: mir::LocalId) -> bool {
    let mut used = Vec::new();
    rvalue_use_locals(op, &mut used);
    used.contains(&id)
}

/// Every local referenced by an rvalue.
fn rvalue_use_locals(op: &mir::RValue, out: &mut Vec<mir::LocalId>) {
    use mir::RValue::*;
    match op {
        Const(_)
        | SignalWait { .. }
        | Receive
        | ReceiveMatch { .. }
        | ReceiveCommit
        | Spawn { .. }
        | SelfRef
        | Panic(_)
        | StateGet { .. } => {}
        mir::RValue::Resume(x) => out.push(*x),
        ReceiveWait { timeout, .. } => out.push(*timeout),
        Load(x) | ArrayLen(x) | Unary(_, x) | CapabilityCheck { val: x } => out.push(*x),
        PerformAsync { args, .. } => out.extend(args.iter().copied()),
        LoadFieldNamed { obj, .. } | LoadFieldPos { obj, .. } => out.push(*obj),
        ArrayLoad { arr, idx } => {
            out.push(*arr);
            out.push(*idx);
        }
        ArrayLit(elems) | Tuple(elems) => out.extend(elems.iter().copied()),
        Binary(_, l, r) | StringEq(l, r) | StrConcat(l, r) => {
            out.push(*l);
            out.push(*r);
        }
        Call { func, args } => {
            if let mir::FuncRef::Local(f) = func {
                out.push(*f);
            }
            out.extend(args.iter().copied());
        }
        Closure { captures, .. } => out.extend(captures.iter().copied()),
        Record(fields) => {
            for (_, v) in fields {
                out.push(*v);
            }
        }
        Perform { args, .. } | FFICall { args, .. } => out.extend(args.iter().copied()),
        Migrate { actor, node } => {
            out.push(*actor);
            out.push(*node);
        }
        Send { actor, args, .. } | Ask { actor, args, .. } => {
            out.push(*actor);
            out.extend(args.iter().copied());
        }
        RecordUpdate { base, overrides } => {
            out.push(*base);
            for (_, v) in overrides {
                out.push(*v);
            }
        }
    }
}

/// Variable names used anywhere in a HIR body, minus `exclude` (the
/// function's parameters and recursive self-name). Conservative: names
/// bound *inside* the body may be included spuriously — callers filter
/// against what is actually in scope, and capture locals are bound before
/// the body so internal bindings still shadow them correctly. Sorted for
/// a deterministic capture order shared with codegen.
fn hir_body_used_vars(body: &hir::Body, exclude: &HashSet<String>) -> Vec<String> {
    let mut acc = HashSet::new();
    walk_hir_body(body, &mut acc);
    let mut names: Vec<String> = acc.into_iter().filter(|n| !exclude.contains(n)).collect();
    names.sort();
    names
}

fn walk_hir_body(body: &hir::Body, acc: &mut HashSet<String>) {
    for stmt in &body.stmts {
        match stmt {
            hir::Stmt::Let { value, .. } => walk_hir_rvalue(value, acc),
            hir::Stmt::Assign { target, value, .. } => {
                walk_hir_place(target, acc);
                walk_hir_rvalue(value, acc);
            }
            hir::Stmt::StateSet { value, .. } => walk_hir_operand(value, acc),
            hir::Stmt::Emit { args, .. } => {
                for a in args {
                    walk_hir_operand(a, acc);
                }
            }
        }
    }
    match &body.terminator {
        hir::Terminator::Yield(op) => walk_hir_operand(op, acc),
        hir::Terminator::FnReturn(op) => {
            if let Some(op) = op {
                walk_hir_operand(op, acc);
            }
        }
        hir::Terminator::Break(_) => {}
    }
}

fn walk_hir_place(place: &hir::Place, acc: &mut HashSet<String>) {
    match place {
        hir::Place::Var(name, _) => {
            acc.insert(name.clone());
        }
        hir::Place::Field { base, .. } => walk_hir_place(base, acc),
        hir::Place::Index { base, idx, .. } => {
            walk_hir_place(base, acc);
            walk_hir_operand(idx, acc);
        }
    }
}

fn walk_hir_operand(op: &hir::Operand, acc: &mut HashSet<String>) {
    if let hir::Operand::Var(name, _) = op {
        acc.insert(name.clone());
    }
}

fn walk_hir_rvalue(rv: &hir::RValue, acc: &mut HashSet<String>) {
    match rv {
        hir::RValue::Use(op) => walk_hir_operand(op, acc),
        hir::RValue::Literal(_, _) | hir::RValue::SelfRef(_) | hir::RValue::Panic(_) => {}
        hir::RValue::Block(body) => walk_hir_body(body, acc),
        hir::RValue::Binary(_, l, r, _) => {
            walk_hir_operand(l, acc);
            walk_hir_operand(r, acc);
        }
        hir::RValue::Unary(_, op, _) => walk_hir_operand(op, acc),
        hir::RValue::Call { func, args, .. } => {
            walk_hir_operand(func, acc);
            for a in args {
                walk_hir_operand(a, acc);
            }
        }
        hir::RValue::Closure { body, .. } | hir::RValue::RecClosure { body, .. } => {
            walk_hir_body(body, acc)
        }
        hir::RValue::Tuple(ops, _) | hir::RValue::Array(ops, _) => {
            for op in ops {
                walk_hir_operand(op, acc);
            }
        }
        hir::RValue::Record(fields, _) => {
            for (_, op) in fields {
                walk_hir_operand(op, acc);
            }
        }
        hir::RValue::RecordUpdate {
            base, overrides, ..
        } => {
            walk_hir_operand(base, acc);
            for (_, op) in overrides {
                walk_hir_operand(op, acc);
            }
        }
        hir::RValue::FieldAccess { base, .. } => walk_hir_operand(base, acc),
        hir::RValue::Index { base, idx, .. } => {
            walk_hir_operand(base, acc);
            walk_hir_operand(idx, acc);
        }
        hir::RValue::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            walk_hir_operand(cond, acc);
            walk_hir_body(then_body, acc);
            if let Some(e) = else_body {
                walk_hir_body(e, acc);
            }
        }
        hir::RValue::Match {
            scrutinee, arms, ..
        } => {
            walk_hir_operand(scrutinee, acc);
            for (_, guard, arm_body) in arms {
                if let Some(guard_body) = guard {
                    walk_hir_body(guard_body, acc);
                }
                walk_hir_body(arm_body, acc);
            }
        }
        hir::RValue::For { iterable, body, .. } => {
            walk_hir_operand(iterable, acc);
            walk_hir_body(body, acc);
        }
        hir::RValue::While { cond, body, .. } => {
            walk_hir_body(cond, acc);
            walk_hir_body(body, acc);
        }
        hir::RValue::Spawn { init, .. } => {
            for (_, op) in init {
                walk_hir_operand(op, acc);
            }
        }
        hir::RValue::Send { actor, args, .. } | hir::RValue::Ask { actor, args, .. } => {
            walk_hir_operand(actor, acc);
            for a in args {
                walk_hir_operand(a, acc);
            }
        }
        hir::RValue::Perform { args, .. } => {
            for a in args {
                walk_hir_operand(a, acc);
            }
        }
        hir::RValue::Resume { value, .. } => {
            walk_hir_operand(value, acc);
        }
        hir::RValue::Handle { body, handlers, .. } => {
            walk_hir_body(body, acc);
            for h in handlers {
                walk_hir_body(&h.body, acc);
            }
        }
        hir::RValue::Receive { arms, after, .. } => {
            for (_, _patterns, guard, arm_body) in arms {
                if let Some(g) = guard {
                    walk_hir_body(g, acc);
                }
                walk_hir_body(arm_body, acc);
            }
            if let Some((ms_body, timeout_body)) = after {
                walk_hir_body(ms_body, acc);
                walk_hir_body(timeout_body, acc);
            }
        }
        hir::RValue::Migrate { actor, node, .. } => {
            walk_hir_operand(actor, acc);
            walk_hir_operand(node, acc);
        }
        hir::RValue::CapCheck { operand, .. } => walk_hir_operand(operand, acc),
        hir::RValue::FFICall { args, .. } => {
            for a in args {
                walk_hir_operand(a, acc);
            }
        }
        hir::RValue::PipelineNew { .. } | hir::RValue::SupervisorNew { .. } => {}
        hir::RValue::PipelineStage {
            id,
            name,
            actor,
            template,
            ..
        } => {
            walk_hir_operand(id, acc);
            walk_hir_operand(name, acc);
            walk_hir_operand(actor, acc);
            walk_hir_operand(template, acc);
        }
        hir::RValue::PipelineRun { id, input, .. } => {
            walk_hir_operand(id, acc);
            walk_hir_operand(input, acc);
        }
        hir::RValue::SupervisorWorker {
            id,
            name,
            actor,
            description,
            ..
        } => {
            walk_hir_operand(id, acc);
            walk_hir_operand(name, acc);
            walk_hir_operand(actor, acc);
            walk_hir_operand(description, acc);
        }
        hir::RValue::SupervisorRun { id, task, .. } => {
            walk_hir_operand(id, acc);
            walk_hir_operand(task, acc);
        }
        hir::RValue::DebateNew {
            topic,
            rounds,
            threshold,
            ..
        } => {
            walk_hir_operand(topic, acc);
            walk_hir_operand(rounds, acc);
            walk_hir_operand(threshold, acc);
        }
        hir::RValue::DebateParticipant {
            id,
            name,
            stance,
            actor,
            ..
        } => {
            walk_hir_operand(id, acc);
            walk_hir_operand(name, acc);
            walk_hir_operand(stance, acc);
            walk_hir_operand(actor, acc);
        }
        hir::RValue::DebateRun { id, .. } => walk_hir_operand(id, acc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lower_empty_module() {
        let hir_module = hir::Module::new("test");
        let mir_module = lower_module(&hir_module).unwrap();
        assert_eq!(mir_module.name, "test");
    }

    // -----------------------------------------------------------------------
    // Peephole: temp/Load fusion (keeps codegen's drop planning effective)
    // -----------------------------------------------------------------------

    fn lower_source(source: &str) -> NuResult<mir::Module> {
        let mut lexer = crate::lexer::Lexer::new(source);
        let tokens = lexer.lex()?;
        let mut parser = crate::parser::Parser::new(tokens);
        let ast = parser.parse_module()?;
        let hir = crate::hir_lower::lower_module(&ast, &std::collections::HashMap::default());
        lower_module(&hir)
    }

    fn find_fn<'m>(module: &'m mir::Module, name: &str) -> &'m mir::Function {
        module
            .functions
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("function '{}' not lowered", name))
    }

    #[test]
    fn test_chained_string_concat_with_int_lowers_to_strconcat() {
        // Regression: `s + 2 + 3` where s is a String must lower BOTH adds to
        // StrConcat. Previously the second add lowered to Binary(Add): the
        // dst of the first StrConcat carried a placeholder type (HIR
        // Operand::Var is always unit), so `is_string` failed for the
        // chained add and the native/AOT backends mis-tagged the string
        // pointer as an int — the interpreter/AOT differential-fuzz
        // divergence on `"hello" + 2 + 3`. `set_local_ty(dst, String)` on
        // the StrConcat dst is the fix; this test locks the lowering in.
        let module = lower_source("let s = \"hello\"; s + 2 + 3").unwrap();
        let main = find_fn(&module, "__main");
        let mut strconcat_count = 0usize;
        let mut binary_add_count = 0usize;
        for b in &main.blocks {
            for st in &b.stmts {
                if let mir::Stmt::Assign {
                    op: mir::RValue::StrConcat(..),
                    ..
                } = st
                {
                    strconcat_count += 1;
                }
                if let mir::Stmt::Assign {
                    op: mir::RValue::Binary(crate::ast::BinOp::Add, ..),
                    ..
                } = st
                {
                    binary_add_count += 1;
                }
            }
        }
        assert_eq!(
            strconcat_count, 2,
            "both adds in `s + 2 + 3` must lower to StrConcat (got {strconcat_count})"
        );
        assert_eq!(
            binary_add_count, 0,
            "no Binary(Add) may remain in `s + 2 + 3` (got {binary_add_count})"
        );
    }

    #[test]
    fn test_fuse_binds_owning_rvalue_to_named_local() {
        // `let a = [1,2,3]` must lower to `a = ArrayLit(...)` directly — not
        // `__tmp = ArrayLit(...); a = Load(__tmp)` — so codegen can prove
        // `a` solely owns the array and emit a real Drop.
        let module = lower_source("let a = [1, 2, 3] in a[0]").unwrap();
        let main = find_fn(&module, "__main");
        let a_id = main
            .locals
            .iter()
            .find(|l| l.name.as_deref() == Some("a"))
            .expect("named local 'a'")
            .id;
        let owns_array = main.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(s, mir::Stmt::Assign { dst, op: mir::RValue::ArrayLit(_) } if *dst == a_id)
            })
        });
        assert!(
            owns_array,
            "named local 'a' must be defined directly by the ArrayLit, got blocks: {:?}",
            main.blocks
        );
        // And no leftover Load copy into `a`.
        let load_into_a = main.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(s, mir::Stmt::Assign { dst, op: mir::RValue::Load(_) } if *dst == a_id)
            })
        });
        assert!(!load_into_a, "the Load copy into 'a' should be fused away");
    }

    #[test]
    fn test_fuse_keeps_multi_use_temp() {
        // A temp read by two Loads must keep its definition; fusing either
        // copy would leave the other reading an undefined register.
        let mut b = mir::FunctionBuilder::new("f", None);
        let t = b.add_temp(Type::unit());
        let x = b.add_local("x", Type::unit());
        let y = b.add_local("y", Type::unit());
        b.assign(t, mir::RValue::ArrayLit(Vec::new()));
        b.assign(x, mir::RValue::Load(t));
        b.assign(y, mir::RValue::Load(t));
        b.terminate(mir::Terminator::Return(Some(y)));
        let mut func = b.build();
        fuse_single_use_temps(&mut func);
        let stmts = &func.blocks[0].stmts;
        assert_eq!(stmts.len(), 3, "multi-use temp must not fuse: {:?}", stmts);
        assert!(matches!(
            stmts[1],
            mir::Stmt::Assign {
                op: mir::RValue::Load(_),
                ..
            }
        ));
        assert!(matches!(
            stmts[2],
            mir::Stmt::Assign {
                op: mir::RValue::Load(_),
                ..
            }
        ));
    }

    #[test]
    fn test_fuse_requires_adjacent_load() {
        // A non-adjacent Load does not fuse: moving the rvalue across an
        // intervening statement could reorder side effects.
        let mut b = mir::FunctionBuilder::new("f", None);
        let t = b.add_temp(Type::unit());
        let z = b.add_local("z", Type::unit());
        let x = b.add_local("x", Type::unit());
        b.assign(t, mir::RValue::ArrayLit(Vec::new()));
        b.assign(z, mir::RValue::Const(crate::bytecode::Constant::Int(1)));
        b.assign(x, mir::RValue::Load(t));
        b.terminate(mir::Terminator::Return(Some(x)));
        let mut func = b.build();
        fuse_single_use_temps(&mut func);
        let stmts = &func.blocks[0].stmts;
        assert_eq!(
            stmts.len(),
            3,
            "non-adjacent Load must not fuse: {:?}",
            stmts
        );
        assert!(matches!(
            stmts[2],
            mir::Stmt::Assign {
                op: mir::RValue::Load(_),
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------
    // Effect handlers: resume flag and return unwinding
    // -----------------------------------------------------------------------

    #[test]
    fn test_non_resuming_handler_lowers_to_pop_and_jump() {
        // `| E.op() => 42` (no `resume`): the body value becomes the handle
        // expression's value — pop the handler frame and jump to the join
        // block instead of resuming the captured continuation.
        let module = lower_source("handle { perform E.op(); 100 } { | E.op() => 42 }").unwrap();
        let main = find_fn(&module, "__main");
        assert_eq!(main.handler_tables.len(), 1);
        let binding = &main.handler_tables[0].bindings[0];
        assert!(!binding.resume, "resume flag must reach MIR");
        let body = &main.blocks[binding.body.0 as usize];
        assert!(
            body.stmts
                .iter()
                .any(|s| matches!(s, mir::Stmt::PopHandler)),
            "abortive handler must pop the handler frame: {:?}",
            body
        );
        assert!(
            matches!(body.terminator, mir::Terminator::Jump(_)),
            "abortive handler must jump to the handle join, got {:?}",
            body.terminator
        );
    }

    #[test]
    fn test_resuming_handler_lowers_to_resume() {
        // `| E.op() resume => 42`: the continuation is resumed with the
        // body value, exactly like before.
        let module =
            lower_source("handle { perform E.op(); 100 } { | E.op() resume => 42 }").unwrap();
        let main = find_fn(&module, "__main");
        let binding = &main.handler_tables[0].bindings[0];
        assert!(binding.resume, "resume flag must reach MIR");
        let body = &main.blocks[binding.body.0 as usize];
        assert!(
            matches!(body.terminator, mir::Terminator::Resume(_)),
            "resuming handler must end in Resume, got {:?}",
            body.terminator
        );
        assert!(
            !body
                .stmts
                .iter()
                .any(|s| matches!(s, mir::Stmt::PopHandler)),
            "resuming handler must not pop the frame (the body does)"
        );
    }

    #[test]
    fn test_return_inside_handler_body_unwinds_frame() {
        // A `return` inside a handler body runs with the handle's frame on
        // the VM handler_stack, so it must unwind that frame (PopHandler)
        // before returning — otherwise the frame leaks and a later
        // unhandled perform dispatches into the dead function.
        let module = lower_source(
            "fn f() -> Int { handle { perform E.op() } { | E.op() => return 7 } } f()",
        )
        .unwrap();
        let f = find_fn(&module, "f");
        let binding = &f.handler_tables[0].bindings[0];
        let body = &f.blocks[binding.body.0 as usize];
        assert!(
            body.stmts
                .iter()
                .any(|s| matches!(s, mir::Stmt::PopHandler)),
            "return inside a handler body must pop the handler frame: {:?}",
            body
        );
        assert!(
            matches!(body.terminator, mir::Terminator::Return(_)),
            "handler-body return returns from the function, got {:?}",
            body.terminator
        );
    }

    // -----------------------------------------------------------------------
    // Variant patterns: payload-carrying constructors are { ctor, payload }
    // -----------------------------------------------------------------------

    #[test]
    fn test_variant_pattern_tests_ctor_field_and_binds_payload() {
        // `| Some(x) => ...` on a payload-carrying constructor must compare
        // the scrutinee's `ctor` field against "Some" and bind `x` to the
        // `payload` field — not to the whole scrutinee record.
        let module = lower_source(
            "let o = { ctor: \"Some\", payload: 41 } in match o { | Some(x) => x + 1 | None => 0 }",
        )
        .unwrap();
        let main = find_fn(&module, "__main");
        let loads_ctor = main.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(s, mir::Stmt::Assign { op: mir::RValue::LoadFieldNamed { field, .. }, .. } if field == "ctor")
            })
        });
        let loads_payload = main.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(s, mir::Stmt::Assign { op: mir::RValue::LoadFieldNamed { field, .. }, .. } if field == "payload")
            })
        });
        assert!(
            loads_ctor,
            "variant tag test must read the scrutinee's ctor field: {:?}",
            main.blocks
        );
        assert!(
            loads_payload,
            "variant pattern must bind the payload field, not the scrutinee: {:?}",
            main.blocks
        );
    }

    #[test]
    fn test_nested_variant_pattern_recurses_into_payload_test() {
        // `| Some(Some(x)) => ...` must test the OUTER ctor, load the
        // payload, and recursively test the INNER ctor against it — two
        // `ctor` field loads combined by a logical And — so that
        // `Some(None)` is rejected. (Outer-tag-only lowering emits a single
        // ctor load and no And.)
        let module = lower_source(
            "let o = { ctor: \"Some\", payload: { ctor: \"Some\", payload: 41 } } in match o { | Some(Some(x)) => x | _ => 0 }",
        )
        .unwrap();
        let main = find_fn(&module, "__main");
        let ctor_loads = main
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .filter(|s| {
                matches!(s, mir::Stmt::Assign { op: mir::RValue::LoadFieldNamed { field, .. }, .. } if field == "ctor")
            })
            .count();
        assert!(
            ctor_loads >= 2,
            "nested variant pattern must test the ctor field at both levels: {:?}",
            main.blocks
        );
        let joins_with_and = main.blocks.iter().any(|b| {
            b.stmts.iter().any(|s| {
                matches!(
                    s,
                    mir::Stmt::Assign {
                        op: mir::RValue::Binary(crate::ast::BinOp::And, _, _),
                        ..
                    }
                )
            })
        });
        assert!(
            joins_with_and,
            "outer and inner tag tests must be combined with And: {:?}",
            main.blocks
        );
    }

    #[test]
    fn test_tuple_pattern_loads_elements_positionally() {
        // `| (a, b) => ...` must load both element positions (bytecode
        // FieldL via LoadFieldPos) so the sub-patterns are tested and the
        // bindings are wired — previously the arm matched unconditionally
        // and the element bindings were never emitted.
        let module = lower_source("let t = (1, 2) in match t { | (a, b) => a + b }").unwrap();
        let main = find_fn(&module, "__main");
        let mut indices: Vec<u8> = main
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .filter_map(|s| match s {
                mir::Stmt::Assign {
                    op: mir::RValue::LoadFieldPos { index, .. },
                    ..
                } => Some(*index),
                _ => None,
            })
            .collect();
        indices.sort_unstable();
        indices.dedup();
        assert!(
            indices.contains(&0) && indices.contains(&1),
            "tuple pattern must load element positions 0 and 1, got {:?}: {:?}",
            indices,
            main.blocks
        );
    }

    // -----------------------------------------------------------------------
    // BEAM Phase 1: spawn link/monitor desugar and receive-after lowering
    // -----------------------------------------------------------------------

    fn fn_uses_rvalue(func: &mir::Function, pred: impl Fn(&mir::RValue) -> bool) -> bool {
        func.blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .any(|s| matches!(s, mir::Stmt::Assign { op, .. } if pred(op)))
    }

    #[test]
    fn test_spawn_link_lowers_to_spawn_plus_actor_link_perform() {
        // The parser desugars `spawn link A { ... }` to
        // `let t = spawn A { ... } in { perform Actor.link(t); t }`, so MIR
        // must contain a Spawn and a generic Perform "Actor"."link" — no new
        // IR nodes or opcodes.
        let module = lower_source("spawn link Counter { count = 0 }").unwrap();
        let main = find_fn(&module, "__main");
        assert!(
            fn_uses_rvalue(main, |op| matches!(op, mir::RValue::Spawn { .. })),
            "desugar must keep a plain Spawn rvalue: {:?}",
            main.blocks
        );
        assert!(
            fn_uses_rvalue(main, |op| matches!(
                op,
                mir::RValue::Perform { effect, op, .. } if effect == "Actor" && op == "link"
            )),
            "desugar must perform Actor.link: {:?}",
            main.blocks
        );
    }

    #[test]
    fn test_spawn_monitor_lowers_to_actor_monitor_perform() {
        let module = lower_source("spawn monitor Counter { count = 0 }").unwrap();
        let main = find_fn(&module, "__main");
        assert!(
            fn_uses_rvalue(main, |op| matches!(
                op,
                mir::RValue::Perform { effect, op, .. } if effect == "Actor" && op == "monitor"
            )),
            "desugar must perform Actor.monitor: {:?}",
            main.blocks
        );
    }

    #[test]
    fn test_receive_after_lowers_to_receivewait() {
        // receive { | Msg(x) => x } after 100 => 0 lowers to a ReceiveWait
        // (timed wait) whose no-match branch runs the timeout body — the
        // legacy pop-any Receive must NOT appear.
        let module = lower_source("receive { | Msg(x) => x } after 100 => 0").unwrap();
        let main = find_fn(&module, "__main");
        let wait = main
            .blocks
            .iter()
            .flat_map(|b| b.stmts.iter())
            .find_map(|s| match s {
                mir::Stmt::Assign {
                    op:
                        mir::RValue::ReceiveWait {
                            behavior_ids,
                            max_params,
                            ..
                        },
                    ..
                } => Some((behavior_ids.len(), *max_params)),
                _ => None,
            });
        let (n_ids, max_params) = wait.unwrap_or_else(|| {
            panic!("receive-after must lower to ReceiveWait: {:?}", main.blocks)
        });
        assert_eq!(n_ids, 1, "one arm -> one candidate behavior id");
        assert_eq!(max_params, 1, "arm has one param");
        assert!(
            !fn_uses_rvalue(main, |op| matches!(op, mir::RValue::ReceiveMatch { .. })),
            "timed receive must not emit a ReceiveMatch scan: {:?}",
            main.blocks
        );
        assert!(
            !fn_uses_rvalue(main, |op| matches!(op, mir::RValue::Receive)),
            "receive-after replaces the legacy pop-any fallback: {:?}",
            main.blocks
        );
    }

    #[test]
    fn test_receive_without_after_keeps_receivematch_and_fallback() {
        // Without `after`, lowering is unchanged: ReceiveMatch scan plus the
        // legacy pop-any Receive fallback on no match.
        let module = lower_source("receive { | Msg(x) => x }").unwrap();
        let main = find_fn(&module, "__main");
        assert!(
            fn_uses_rvalue(main, |op| matches!(op, mir::RValue::ReceiveMatch { .. })),
            "plain receive must keep the ReceiveMatch scan: {:?}",
            main.blocks
        );
        assert!(
            fn_uses_rvalue(main, |op| matches!(op, mir::RValue::Receive)),
            "plain receive must keep the legacy fallback: {:?}",
            main.blocks
        );
        assert!(
            !fn_uses_rvalue(main, |op| matches!(op, mir::RValue::ReceiveWait { .. })),
            "plain receive must not use the timed wait: {:?}",
            main.blocks
        );
    }
}
