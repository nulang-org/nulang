//! AOT (Ahead-of-Time) native code compilation backend.
//!
//! Compiles Nulang MIR modules to native code via Cranelift, leveraging
//! compile-time type information to emit unboxed operations.
//!
//! # Architecture
//!
//! - `codegen`: MIR → Cranelift CLIF compilation (per-function)
//! - This module: orchestrates module-level compilation, registers runtime
//!   helpers, and provides the execution entry point.
//!
//! # Current status
//!
//! Uses `cranelift_jit::JITModule` (same as the tiered JIT) rather than
//! true AOT object-file emission. This gives us native code without needing
//! a linker — the trampoline calls into the JIT module at startup.

pub mod codegen;

use cranelift::prelude::*;
use cranelift_frontend::FunctionBuilderContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::Module;

use crate::mir;
use crate::runtime::heap::TypeTag as HeapTypeTag;
use crate::types::{NuResult, Span};

/// Compiled AOT module ready for execution.
pub struct AotModule {
    /// The Cranelift JIT module that owns compiled code memory.
    #[allow(dead_code)]
    jit_module: JITModule,
    /// Reusable function builder context.
    #[allow(dead_code)]
    builder_context: FunctionBuilderContext,
    /// Compiled function pointers indexed by MIR function index.
    compiled_funcs: Vec<*const u8>,
    /// Actor behavior names, parallel to `compiled_behaviors`.
    behavior_names: Vec<String>,
    /// Compiled actor behavior pointers (native code), parallel to
    /// `behavior_names`. Empty when the module has no `actor` declarations.
    compiled_behaviors: Vec<*const u8>,
    /// Entry point index (the `__main` or `main` function).
    entry_idx: Option<usize>,
    /// Module-wide field name → slot index mapping for records.
    #[allow(dead_code)]
    field_map: std::collections::HashMap<String, u8>,
    /// Constant pool (String literals), for runtime string resolution.
    constants: Vec<crate::bytecode::Constant>,
    /// The bytecode `CodeModule` compiled from the same MIR, retained so a
    /// native behavior can spawn real Runtime actors through
    /// `Runtime::spawn_from_module` (which needs a CodeModule). Built
    /// best-effort alongside the AOT code.
    code_module: Option<crate::bytecode::CodeModule>,
}

impl AotModule {
    /// Compile a MIR module to native code for the specified target.
    pub fn compile(mir_module: &mir::Module) -> NuResult<Self> {
        Self::compile_for_target(mir_module, "native")
    }

    /// Compile a MIR module to native code for a specific target ISA.
    pub fn compile_for_target(mir_module: &mir::Module, target: &str) -> NuResult<Self> {
        // Set up Cranelift with the target ISA.
        let mut flag_builder = settings::builder();
        let _ = flag_builder.set("enable_simd", "true");
        let _ = flag_builder.set("opt_level", "speed");
        let isa_builder = create_isa_builder(target)?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| crate::types::NuError::VMError {
                msg: format!("failed to finalize ISA for target '{}': {}", target, e),
                span: Span::default(),
            })?;

        let mut jit_builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());

        // Register NaN-tag-aware runtime helpers.
        register_runtime_helpers(&mut jit_builder);

        let mut jit_module = JITModule::new(jit_builder);
        let mut builder_context = FunctionBuilderContext::new();

        // Pre-scan: build module-wide field name → slot index map and
        // constant pool for string literals.
        let mut field_map: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
        let mut next_field_id: u8 = 0;
        let mut constants: Vec<crate::bytecode::Constant> = Vec::new();

        for func in &mir_module.functions {
            for block in &func.blocks {
                for stmt in &block.stmts {
                    collect_field_and_consts(
                        stmt,
                        &mut field_map,
                        &mut next_field_id,
                        &mut constants,
                        &mir_module.foreign_functions,
                    );
                }
            }
        }
        for func in &mir_module.behaviors {
            for block in &func.blocks {
                for stmt in &block.stmts {
                    collect_field_and_consts(
                        stmt,
                        &mut field_map,
                        &mut next_field_id,
                        &mut constants,
                        &mir_module.foreign_functions,
                    );
                }
            }
        }

        // Pass 1: declare all functions so forward references resolve.
        let mut func_ids: Vec<cranelift_module::FuncId> =
            Vec::with_capacity(mir_module.functions.len());
        // Unboxed variants for all-Int functions (same indices, empty for non-Int).
        let mut unboxed_ids: Vec<Option<cranelift_module::FuncId>> =
            vec![None; mir_module.functions.len()];

        for (idx, func) in mir_module.functions.iter().enumerate() {
            let func_name = format!("nulang_fn_{}", idx);
            let mut sig = jit_module.make_signature();
            for _ in &func.params {
                sig.params.push(AbiParam::new(types::I64));
            }
            for _ in &func.captures {
                sig.params.push(AbiParam::new(types::I64));
            }
            sig.returns.push(AbiParam::new(types::I64));
            let fid = jit_module
                .declare_function(&func_name, cranelift_module::Linkage::Local, &sig)
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("failed to declare '{}': {}", func.name, e),
                    span: Span::default(),
                })?;
            func_ids.push(fid);
            // If the function is all-Int, also declare an unboxed variant.
            if codegen::is_all_int(func) {
                let ub_name = format!("nulang_fn_{}_unboxed", idx);
                let mut ub_sig = jit_module.make_signature();
                for _ in &func.params {
                    ub_sig.params.push(AbiParam::new(types::I64));
                }
                for _ in &func.captures {
                    ub_sig.params.push(AbiParam::new(types::I64));
                }
                ub_sig.returns.push(AbiParam::new(types::I64));
                let ub_fid = jit_module
                    .declare_function(&ub_name, cranelift_module::Linkage::Local, &ub_sig)
                    .map_err(|e| crate::types::NuError::VMError {
                        msg: format!("failed to declare unboxed '{}': {}", func.name, e),
                        span: Span::default(),
                    })?;
                unboxed_ids[idx] = Some(ub_fid);
            }
        }

        // Pass 2: compile each function body (boxed + optionally unboxed).
        let mut entry_idx: Option<usize> = None;

        for (idx, func) in mir_module.functions.iter().enumerate() {
            // For all-Int functions: compile unboxed body first, then
            // generate a boxing wrapper as the boxed entry point. The
            // original boxed body is never compiled.
            // For non-all-Int functions: compile boxed body as usual.
            if let Some(ub_fid) = unboxed_ids[idx] {
                // Compile unboxed variant (self-recursive calls resolve to ub_fid).
                let mut ctx2 = codegen::AotContext::new(&mut jit_module, &mut builder_context);
                ctx2.func_ids = func_ids.clone();
                ctx2.func_ids[idx] = ub_fid;
                ctx2.field_map = field_map.clone();
                ctx2.constants = constants.clone();
                ctx2.foreign_functions = mir_module.foreign_functions.clone();
                codegen::compile_mir_function_body(
                    &mut ctx2,
                    func,
                    idx,
                    ub_fid,
                    codegen::CompileMode::Unboxed,
                )
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("AOT compilation of unboxed '{}' failed: {}", func.name, e),
                    span: Span::default(),
                })?;

                // Compile boxing wrapper as the boxed function table entry.
                let mut ctx3 = codegen::AotContext::new(&mut jit_module, &mut builder_context);
                codegen::compile_boxing_wrapper(
                    &mut ctx3,
                    func.params.len(),
                    func_ids[idx],
                    ub_fid,
                )
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("AOT boxing wrapper for '{}' failed: {}", func.name, e),
                    span: Span::default(),
                })?;
            } else {
                // Normal boxed compilation for non-all-Int functions.
                let mut ctx = codegen::AotContext::new(&mut jit_module, &mut builder_context);
                ctx.func_ids = func_ids.clone();
                ctx.field_map = field_map.clone();
                ctx.constants = constants.clone();
                ctx.foreign_functions = mir_module.foreign_functions.clone();
                codegen::compile_mir_function_body(
                    &mut ctx,
                    func,
                    idx,
                    func_ids[idx],
                    codegen::CompileMode::Boxed,
                )
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("AOT compilation of '{}' failed: {}", func.name, e),
                    span: Span::default(),
                })?;
            }

            if func.name == "__main" || func.name == "main" {
                if entry_idx.is_none() || func.name == "__main" {
                    entry_idx = Some(idx);
                }
            }
        }

        // Pass: compile actor behaviors to native code, indexed by behavior
        // name. Behaviors are ordinary `Function`s (params + blocks); they
        // are never `Call` targets, so each compiles into its own native
        // entry point keyed by name. The actor runtime can later dispatch
        // messages straight to these pointers, bypassing the bytecode VM.
        let mut behavior_names: Vec<String> = Vec::new();
        let mut behavior_fids: Vec<cranelift_module::FuncId> = Vec::new();
        for (idx, func) in mir_module.behaviors.iter().enumerate() {
            let func_name = format!("nulang_behavior_{}", idx);
            let mut sig = jit_module.make_signature();
            for _ in &func.params {
                sig.params.push(AbiParam::new(types::I64));
            }
            sig.returns.push(AbiParam::new(types::I64));
            let fid = jit_module
                .declare_function(&func_name, cranelift_module::Linkage::Local, &sig)
                .map_err(|e| crate::types::NuError::VMError {
                    msg: format!("failed to declare behavior '{}': {}", func.name, e),
                    span: Span::default(),
                })?;
            let mut ctx = codegen::AotContext::new(&mut jit_module, &mut builder_context);
            ctx.func_ids = func_ids.clone();
            ctx.field_map = field_map.clone();
            ctx.constants = constants.clone();
            codegen::compile_mir_function_body(
                &mut ctx,
                func,
                idx,
                fid,
                codegen::CompileMode::Boxed,
            )
            .map_err(|e| crate::types::NuError::VMError {
                msg: format!("AOT compilation of behavior '{}' failed: {}", func.name, e),
                span: Span::default(),
            })?;
            behavior_names.push(func.name.clone());
            behavior_fids.push(fid);
        }
        jit_module
            .finalize_definitions()
            .map_err(|e| crate::types::NuError::VMError {
                msg: format!("failed to finalize JIT definitions: {}", e),
                span: Span::default(),
            })?;

        let compiled_funcs: Vec<*const u8> = func_ids
            .iter()
            .map(|fid| jit_module.get_finalized_function(*fid))
            .collect();
        let compiled_behaviors: Vec<*const u8> = behavior_fids
            .iter()
            .map(|fid| jit_module.get_finalized_function(*fid))
            .collect();

        // Best-effort bytecode companion so native spawn can route through
        // `Runtime::spawn_from_module`. The AOT JIT path borrows the MIR
        // immutably throughout, so the companion compiles an optimized
        // clone rather than mutating the shared module.
        let mut optimized = mir_module.clone();
        let code_module = crate::mir_codegen::compile_mir(&mut optimized, &mir_module.name).ok();

        Ok(AotModule {
            jit_module,
            builder_context,
            compiled_funcs,
            behavior_names,
            compiled_behaviors,
            entry_idx,
            field_map,
            constants,
            code_module,
        })
    }

    /// The bytecode `CodeModule` compiled from the same MIR, if it compiled.
    pub fn code_module(&self) -> Option<&crate::bytecode::CodeModule> {
        self.code_module.as_ref()
    }

    /// Look up a compiled behavior's native entry pointer by name.
    ///
    /// Returns `None` when the module has no behavior with that name. The
    /// returned pointer is a function with the AOT calling convention:
    /// `extern "C" fn(boxed_param_0, boxed_param_1, ...) -> u64`. It is only
    /// valid while the `AotModule` is alive (the pointer lives in the JIT
    /// code memory it owns).
    pub fn fn_ptr_for_behavior(&self, name: &str) -> Option<*const u8> {
        self.behavior_names
            .iter()
            .position(|n| n == name)
            .map(|idx| self.compiled_behaviors[idx])
    }

    /// The module's constant pool (string literals). AOT behavior dispatch
    /// sets these so `StateGet`/`StateSet` field names resolve.
    pub fn constants(&self) -> &[crate::bytecode::Constant] {
        &self.constants
    }

    /// Unique actor type names declared by this module, derived from the
    /// `"{Actor}.{behavior}"` behavior-name prefixes.
    pub fn actor_type_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .behavior_names
            .iter()
            .filter_map(|n| n.split('.').next().map(str::to_string))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Create a standalone actor of the type referenced by `behavior_idx`
    /// (the actor's first behavior's module index, per `spawn_behavior_idx`).
    /// Registers all of the actor's behaviors with the AOT adapter (in module
    /// order, so the actor's local behavior-table indices match module
    /// indices), applies `init` state overrides (name constant idx → value),
    /// and returns the new actor's id. The spawned actor is boxed and owned by
    /// this module's registry, and its raw pointer is registered in
    /// `AOT_ACTORS` so native `send` can deliver to it.
    pub fn spawn_actor(
        &self,
        behavior_idx: usize,
        init: Vec<(u64, crate::vm::Value)>,
    ) -> Option<u64> {
        let full = self.behavior_names.get(behavior_idx)?;
        let actor_name = full.split('.').next()?.to_string();
        let id = AOT_FRESH_ACTOR_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut actor = Box::new(crate::runtime::Actor::new(id, actor_name.clone(), 64));

        let prefix = format!("{}.", actor_name);
        for name in &self.behavior_names {
            if let Some(short) = name.strip_prefix(&prefix) {
                actor.register_behavior(short.to_string(), aot_behavior_adapter);
            }
        }

        for (name_idx, value) in init {
            let s = self
                .constants
                .get(name_idx as usize)
                .and_then(|c| match c {
                    crate::bytecode::Constant::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            actor.set_state_field(s, value);
        }

        let raw = &mut *actor as *mut crate::runtime::Actor;
        AOT_SPAWNED_ACTORS.with(|m| {
            m.borrow_mut().insert(id, actor);
        });
        AOT_ACTORS.with(|m| {
            m.borrow_mut().insert(id, raw);
        });
        Some(id)
    }

    /// Execute the module entry point and return the result as a u64 value.
    ///
    /// A module with no `__main`/`main` (e.g. only function definitions, a
    /// library) has no entry expression; running it yields nil, matching the
    /// interpreter. Do NOT fall back to function 0 — that could be a
    /// parameterized function, and calling it with no args would return
    /// garbage.
    pub fn run(&self) -> NuResult<u64> {
        // A module with no `__main`/`main` (e.g. only function definitions, a
        // library) has no entry expression; running it yields nil, matching the
        // interpreter. Do NOT fall back to function 0 — that could be a
        // parameterized function, and calling it with no args would return
        // garbage.
        let Some(idx) = self.entry_idx else {
            return Ok(crate::vm::Value::nil().as_raw());
        };
        let ptr = self
            .compiled_funcs
            .get(idx)
            .ok_or_else(|| crate::types::NuError::VMError {
                msg: "no compiled entry point".into(),
                span: Span::default(),
            })?;

        // Set up standalone heap for AOT runtime helpers.
        let mut heap = crate::runtime::heap::ActorHeap::new(1024 * 1024);
        heap.set_actor_id(0);
        crate::jit::runtime::aot_set_heap(heap);

        // Set up constant pool for string resolution.
        if !self.constants.is_empty() {
            unsafe {
                crate::jit::runtime::aot_set_constants(&self.constants);
            }
        }
        // Arm the compiled-function context so captured closures can resolve
        // their target's native entry point.
        set_aot_module_ctx(self);

        // Install StandaloneVmCallbacks so perform_builtin_effect works
        // (e.g., IO.print, String.length, etc.) in the native backend.
        // This mirrors how the bytecode VM uses StandaloneVmCallbacks for
        // top-level execution in `VM::run()`.
        let callbacks = Box::new(crate::vm::StandaloneVmCallbacks::new());
        let callbacks_ptr = Box::into_raw(callbacks) as *mut dyn crate::vm::ActorVmCallbacks;
        unsafe {
            crate::jit::runtime::set_jit_callbacks(callbacks_ptr);
        }

        // Call the compiled function. Signature: extern "C" fn() -> u64
        // (for the entry point with no params).
        let func: extern "C" fn() -> u64 = unsafe { std::mem::transmute(*ptr) };
        let result = func();
        // Arithmetic helpers (pow/neg/...) cannot unwind from compiled code,
        // so they record interpreter-parity runtime errors (48-bit overflow,
        // type errors) in a thread-local. Surface it as the run's error so
        // the AOT backend agrees with the interpreter.
        if let Some(msg) = crate::jit::runtime::aot_take_pending_error() {
            // Clean up before returning (mirror the normal path).
            unsafe {
                crate::jit::runtime::clear_jit_callbacks();
                let _ = Box::from_raw(callbacks_ptr as *mut crate::vm::StandaloneVmCallbacks);
            }
            crate::jit::runtime::aot_clear_constants();
            clear_aot_module_ctx();
            let _ = crate::jit::runtime::aot_take_heap();
            return Err(crate::types::NuError::runtime_error(
                msg,
                crate::types::Span::default(),
            ));
        }

        // Clean up: reconstruct Box to drop callbacks and free heap/GC.
        unsafe {
            crate::jit::runtime::clear_jit_callbacks();
            let _ = Box::from_raw(callbacks_ptr as *mut crate::vm::StandaloneVmCallbacks);
        }
        crate::jit::runtime::aot_clear_constants();
        clear_aot_module_ctx();
        let _ = crate::jit::runtime::aot_take_heap();

        Ok(result)
    }

    /// Run the module entry point inside a real actor `Runtime`.
    ///
    /// This is the `--backend native` equivalent of the bytecode path's
    /// `run_with_runtime`: the top-level native code runs with runtime-backed
    /// callbacks, so `spawn` creates live runtime actors and `send` enqueues
    /// real messages. After the entry function returns, the scheduler runs until
    /// the run queue drains.
    ///
    /// The `AotModule` is consumed and registered with the runtime so that
    /// spawned actors dispatch their behaviors through native code.
    pub fn run_in_runtime(self, rt: &mut crate::runtime::Runtime) -> NuResult<u64> {
        let Some(idx) = self.entry_idx else {
            return Ok(crate::vm::Value::nil().as_raw());
        };
        let ptr = self.compiled_funcs.get(idx).copied().ok_or_else(|| {
            crate::types::NuError::VMError {
                msg: "no compiled entry point".into(),
                span: crate::types::Span::default(),
            }
        })?;

        // The bytecode companion carries actor metadata needed by
        // `spawn_from_module`. Require it for any module that uses actors.
        let code_module =
            self.code_module
                .clone()
                .ok_or_else(|| crate::types::NuError::VMError {
                    msg: "AOT runtime execution requires a bytecode companion module".into(),
                    span: crate::types::Span::default(),
                })?;
        let constants = self.constants.clone();

        // Register the AOT module and its bytecode grains with the runtime.
        let module_ptr = rt.register_aot_module(self);
        rt.register_module_grains(&code_module);

        // Set up the AOT helper context (heap fallback, constant pool, closure
        // dispatch). The module pointer stays valid for the lifetime of `rt`.
        let mut heap = crate::runtime::heap::ActorHeap::new(1024 * 1024);
        heap.set_actor_id(0);
        crate::jit::runtime::aot_set_heap(heap);
        if !constants.is_empty() {
            unsafe {
                crate::jit::runtime::aot_set_constants(&constants);
            }
        }
        unsafe {
            set_aot_module_ctx(&*module_ptr);
        }

        // Install runtime-backed callbacks and call the native entry.
        let mut callbacks = AotTopLevelCallbacks { runtime: rt };
        unsafe {
            crate::jit::runtime::set_jit_callbacks(&mut callbacks);
        }
        let func: extern "C" fn() -> u64 = unsafe { std::mem::transmute(ptr) };
        let result = func();

        // Clear helper context before running the scheduler.
        crate::jit::runtime::clear_jit_callbacks();
        if let Some(msg) = crate::jit::runtime::aot_take_pending_error() {
            crate::jit::runtime::aot_clear_constants();
            clear_aot_module_ctx();
            let _ = crate::jit::runtime::aot_take_heap();
            return Err(crate::types::NuError::runtime_error(
                msg,
                crate::types::Span::default(),
            ));
        }

        crate::jit::runtime::aot_clear_constants();
        clear_aot_module_ctx();
        let _ = crate::jit::runtime::aot_take_heap();

        // Drain actor mailboxes.
        rt.run_scheduler();

        Ok(result)
    }

    /// Emit assembly text for the compiled module.
    pub fn emit_assembly(&self) -> String {
        // For now, we'll just show the function names and basic info
        // Full assembly emission would require using cranelift_object or TextSectionBuilder
        let mut output = String::new();
        output.push_str(&format!("; AOT Module for target\n"));
        output.push_str(&format!("; Functions: {}\n", self.compiled_funcs.len()));
        for (idx, _) in self.compiled_funcs.iter().enumerate() {
            output.push_str(&format!("nulang_fn_{}:\n", idx));
            output.push_str("  ; [assembly would be emitted here]\n");
        }
        output
    }
}

// ---------------------------------------------------------------------------
// Native behavior dispatch
// ---------------------------------------------------------------------------
// Bridges the actor runtime's plain-fn behavior handler
// (`fn(&mut Actor, &[Value])`) to AOT-compiled native code. The compiled
// behavior functions are `extern "C" fn(boxed_param...) -> u64`; this adapter
// (a) installs an `ActorVmCallbacks` over the target actor so `StateGet` /
// `StateSet` / heap ops inside the native body route to it, and (b) packs the
// message payload into boxed args and calls the native pointer.
//
// The native target is supplied through a thread-local (set by the driver
// immediately before invoking the handler), mirroring how `set_jit_callbacks`
// feeds the VM's tiered JIT. This keeps the adapter a plain `fn` so it can sit
// in `Actor::behavior_table` without a closure.

/// Per-behavior AOT dispatch info the scheduler (or standalone driver) arms
/// before invoking `aot_behavior_adapter`. Holds the native fn pointer, the
/// owning `AotModule` (for the constant pool and spawn context), and the real
/// `Runtime` when dispatching inside the actor runtime (null in the
/// standalone driver).
#[derive(Clone, Copy)]
pub struct AotDispatchTarget {
    /// Native entry point of the behavior (`extern "C" fn(boxed...) -> u64`).
    pub fn_ptr: *const u8,
    /// The module that compiled `fn_ptr`; kept alive by the owning Runtime or
    /// the standalone driver for the duration of the dispatch.
    pub module: *const AotModule,
    /// The real actor `Runtime`, or null when dispatching standalone.
    pub runtime: *mut crate::runtime::Runtime,
}

impl AotDispatchTarget {
    /// Build a standalone (no real Runtime) target from an `&AotModule`.
    pub fn standalone(fn_ptr: *const u8, module: &AotModule) -> Self {
        AotDispatchTarget {
            fn_ptr,
            module: module as *const AotModule,
            runtime: std::ptr::null_mut(),
        }
    }
}

thread_local! {
    /// Native target the next `aot_behavior_adapter` call dispatches through.
    /// None when no target is armed.
    static AOT_DISPATCH: std::cell::RefCell<Option<AotDispatchTarget>> =
        std::cell::RefCell::new(None);
}

/// Arm the thread-local native target for the next `aot_behavior_adapter`
/// invocation, and install the module constant pool so `StateGet`/`StateSet`/
/// spawn field-name string constants resolve. The driver must call this
/// immediately before dispatching a message to an AOT-compiled behavior, and
/// `clear_aot_dispatch` after.
pub fn set_aot_dispatch(target: Option<AotDispatchTarget>) {
    if let Some(t) = target {
        // SAFETY: `t.module` outlives the dispatched native call (owned by the
        // Runtime or the standalone driver's local). `aot_set_constants` copies
        // the slice.
        unsafe {
            crate::jit::runtime::aot_set_constants(&(*t.module).constants());
            set_aot_module_ctx(&*t.module);
        }
    }
    AOT_DISPATCH.with(|c| *c.borrow_mut() = target);
}

/// Disarm the native target after a dispatched behavior returns.
pub fn clear_aot_dispatch() {
    AOT_DISPATCH.with(|c| *c.borrow_mut() = None);
    crate::jit::runtime::aot_clear_constants();
    clear_aot_module_ctx();
}

thread_local! {
    /// The `AotModule` whose `spawn_actor` resolves the next
    /// `nulang_aot_spawn` call (armed by the driver around dispatch).
    static AOT_SPAWN_CTX: std::cell::RefCell<*const AotModule> =
        std::cell::RefCell::new(std::ptr::null());
    /// The `AotModule` whose compiled function table resolves the next
    /// `nulang_aot_resolve_fn` call (armed around dispatch, so captured
    /// closures can look up their target's native entry point).
    static AOT_MODULE_CTX: std::cell::RefCell<*const AotModule> =
        std::cell::RefCell::new(std::ptr::null());
}

/// Arm the module whose compiled function table resolves closure targets.
/// The caller must clear it after dispatch.
pub fn set_aot_module_ctx(module: &AotModule) {
    AOT_MODULE_CTX.with(|c| *c.borrow_mut() = module as *const AotModule);
}

/// Disarm the compiled-function context.
pub fn clear_aot_module_ctx() {
    AOT_MODULE_CTX.with(|c| *c.borrow_mut() = std::ptr::null());
}

/// The armed module's constant pool, for callbacks that resolve string
/// arguments (async effect dispatch). Empty when no module is armed.
pub fn aot_module_constants() -> &'static [crate::bytecode::Constant] {
    let module = AOT_MODULE_CTX.with(|c| *c.borrow());
    if module.is_null() {
        &[]
    } else {
        // SAFETY: the armed module outlives the dispatched native call.
        unsafe { (*module).constants() }
    }
}

/// Native-code entry point for captured-closure dispatch: resolve a compiled
/// function pointer by MIR function index from the armed module context.
/// Returns the pointer as u64 (0 when no module is armed or the index is out
/// of range). Defined here (not in `jit/runtime.rs`) because it needs
/// `AotModule`; the JIT linker resolves it by symbol name at link time.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_resolve_fn(fn_idx: u64) -> u64 {
    let module = AOT_MODULE_CTX.with(|c| *c.borrow());
    if module.is_null() {
        return 0;
    }
    let m = unsafe { &*module };
    m.compiled_funcs
        .get(fn_idx as usize)
        .copied()
        .map(|p| p as u64)
        .unwrap_or(0)
}

/// Arm the module the next `nulang_aot_spawn` (from native behavior code)
/// uses to create actors. The driver must call this before dispatching a
/// behavior that spawns, and `clear_aot_spawn_ctx` after.
pub fn set_aot_spawn_ctx(module: &AotModule) {
    AOT_SPAWN_CTX.with(|c| *c.borrow_mut() = module as *const AotModule);
}

/// Disarm the spawn context after a dispatched native behavior returns.
pub fn clear_aot_spawn_ctx() {
    AOT_SPAWN_CTX.with(|c| *c.borrow_mut() = std::ptr::null());
}

/// Native-code entry point for `RValue::Spawn`: creates an actor of the type
/// whose first behavior is at module index `behavior_idx`, applying any queued
/// init pairs. When dispatched inside the real actor `Runtime` (the armed
/// `AOT_DISPATCH` target carries a non-null runtime), the spawn routes through
/// `Runtime::spawn_from_module` so the new actor joins the scheduler and gets
/// AOT-wired; otherwise it creates a boxed standalone actor. Returns the new
/// actor's id (boxed), or nil when no context is armed. Defined here (not in
/// `jit/runtime.rs`) because it needs `AotModule`; the JIT linker resolves it
/// by symbol name at link time.
#[no_mangle]
pub unsafe extern "C" fn nulang_aot_spawn(behavior_idx: u64) -> u64 {
    let init = crate::jit::runtime::take_aot_spawn_init();
    let dispatch = AOT_DISPATCH.with(|c| *c.borrow());
    if let Some(t) = dispatch {
        let module = unsafe { &*t.module };
        if !t.runtime.is_null() {
            // Real Runtime path: spawn through the scheduler so the new actor
            // is a live runtime actor (and its behaviors are AOT-wired).
            if let Some(code) = module.code_module() {
                let init: Vec<(String, crate::vm::Value)> = init
                    .iter()
                    .map(|(idx, v)| {
                        let name = module
                            .constants()
                            .get(*idx as usize)
                            .and_then(|c| match c {
                                crate::bytecode::Constant::String(s) => Some(s.clone()),
                                _ => None,
                            })
                            .unwrap_or_default();
                        (name, *v)
                    })
                    .collect();
                let val =
                    unsafe { (*t.runtime).spawn_from_module(code, behavior_idx as usize, init) };
                return val.as_raw();
            }
            return crate::vm::Value::nil().as_raw();
        }
        // Standalone path: spawn a boxed standalone actor.
        return match module.spawn_actor(behavior_idx as usize, init) {
            Some(id) => crate::vm::Value::actor_ref(id).as_raw(),
            None => crate::vm::Value::nil().as_raw(),
        };
    }
    // Fallback: standalone spawn via the explicit spawn context.
    let module = AOT_SPAWN_CTX.with(|c| *c.borrow());
    if module.is_null() {
        return crate::vm::Value::nil().as_raw();
    }
    match (*module).spawn_actor(behavior_idx as usize, init) {
        Some(id) => crate::vm::Value::actor_ref(id).as_raw(),
        None => crate::vm::Value::nil().as_raw(),
    }
}

// ---------------------------------------------------------------------------
// AOT builtin effect dispatch
// ---------------------------------------------------------------------------
// `perform <Effect>.<op>(args...)` in an AOT-compiled behavior with no
// statically-resolved user handler (`resolved_handler: None`) lowers to an
// arity-matched `nulang_aot_perform_N` call. The helper resolves the effect/
// op strings (TAG_STRING constants from the module pool), collects the boxed
// args, and routes through the current callbacks' `perform_builtin_effect_in_module`,
// which (via the real Runtime) dispatches IO/Actor/Timer/Test/Otp/Http/Workflow
// builtins exactly as the bytecode VM does. This covers the dominant
// builtin-effect usage without continuations. Dynamically-handled user
// effects (an active handler for the same effect at runtime) are not
// supported by the native backend — the compile-time `resolved_handler: None`
// only guarantees no *lexical* handler, matching the bytecode fallback for
// unbound effects. Outside an actor context the helper degrades to nil.

macro_rules! define_aot_perform {
    ($name:ident, $($arg:ident),*) => {
        /// Perform a builtin effect from AOT-compiled code.
        #[no_mangle]
        pub unsafe extern "C" fn $name(eff_raw: u64, op_raw: u64 $(, $arg: u64)*) -> u64 {
            let effect = crate::jit::runtime::resolve_string_coerce(eff_raw).unwrap_or_default();
            let op = crate::jit::runtime::resolve_string_coerce(op_raw).unwrap_or_default();
            let regs = [$(crate::vm::Value::from_bits($arg)),*];
            // The module is only needed by `perform_builtin_effect_in_module`
            // for a few effects (Otp/Http resolve against it); the common
            // IO/Actor/Timer path ignores it.
            let module = AOT_DISPATCH.with(|c| {
                c.borrow()
                    .and_then(|t| unsafe { (&*t.module).code_module() })
                    .map(|cm| cm as *const crate::bytecode::CodeModule)
                    .unwrap_or(std::ptr::null())
            });
            let constants = if module.is_null() {
                crate::aot::aot_module_constants()
            } else {
                unsafe { &(*module).constants }
            };
            crate::jit::runtime::try_with_callbacks(|cb| {
                if module.is_null() {
                    cb.perform_builtin_effect(&effect, Some(&op), constants, &regs)
                } else {
                    cb.perform_builtin_effect_in_module(
                        &effect,
                        Some(&op),
                        unsafe { &*module },
                        &regs,
                    )
                }
            })
            .flatten()
            .unwrap_or_else(crate::vm::Value::nil)
            .as_raw()
        }
    };
}

define_aot_perform!(nulang_aot_perform_0,);
define_aot_perform!(nulang_aot_perform_1, a0);
define_aot_perform!(nulang_aot_perform_2, a0, a1);
define_aot_perform!(nulang_aot_perform_3, a0, a1, a2);
define_aot_perform!(nulang_aot_perform_4, a0, a1, a2, a3);
define_aot_perform!(nulang_aot_perform_5, a0, a1, a2, a3, a4);
define_aot_perform!(nulang_aot_perform_6, a0, a1, a2, a3, a4, a5);
define_aot_perform!(nulang_aot_perform_7, a0, a1, a2, a3, a4, a5, a6);
define_aot_perform!(nulang_aot_perform_8, a0, a1, a2, a3, a4, a5, a6, a7);

thread_local! {
    /// Standalone actor registry: actor id → raw actor pointer. Populated by
    /// the AOT driver so `send` from native behavior code can deliver into a
    /// target actor's mailbox without a full `Runtime`.
    static AOT_ACTORS: std::cell::RefCell<std::collections::HashMap<u64, *mut crate::runtime::Actor>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

thread_local! {
    /// Ownership store for actors created by `AotModule::spawn_actor`. A
    /// spawned actor is boxed here (so its heap-allocated pointer is stable)
    /// and its raw pointer is also registered in `AOT_ACTORS` for `send`.
    static AOT_SPAWNED_ACTORS: std::cell::RefCell<std::collections::HashMap<u64, Box<crate::runtime::Actor>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Next id for a standalone-spawned actor, kept clear of the small ids the
/// tests use for manually-created actors.
static AOT_FRESH_ACTOR_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1_000_000);

/// Register a standalone actor so native `send` can deliver to its mailbox.
/// The pointer must stay valid until `unregister_aot_actor`.
pub fn register_aot_actor(actor: &mut crate::runtime::Actor) {
    AOT_ACTORS.with(|c| {
        c.borrow_mut()
            .insert(actor.id, actor as *mut crate::runtime::Actor);
    });
}

/// Ids of every actor registered in the standalone send registry (both
/// driver-registered and spawned).
pub fn aot_actor_ids() -> Vec<u64> {
    AOT_ACTORS.with(|c| c.borrow().keys().copied().collect())
}

/// Read the actor pointer for an id owned by the standalone spawn registry.
pub fn aot_spawned_actor(id: u64) -> Option<*mut crate::runtime::Actor> {
    AOT_SPAWNED_ACTORS.with(|m| {
        m.borrow()
            .get(&id)
            .map(|b| &**b as *const crate::runtime::Actor as *mut crate::runtime::Actor)
    })
}

/// Remove a standalone actor from the native send registry.
pub fn unregister_aot_actor(id: u64) {
    AOT_ACTORS.with(|c| {
        c.borrow_mut().remove(&id);
    });
}

/// Invoke an AOT-compiled behavior with a boxed payload (arity-matched). The
/// target is the `AOT_DISPATCH` thread-local armed by the driver/scheduler.
fn call_aot_behavior(ptr: *const u8, raw: &[u64]) {
    // SAFETY (each arm): `ptr` is a finalized AOT behavior with this arity.
    match raw.len() {
        0 => {
            let f: extern "C" fn() -> u64 = unsafe { std::mem::transmute(ptr) };
            let _ = f();
        }
        1 => {
            let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0]);
        }
        2 => {
            let f: extern "C" fn(u64, u64) -> u64 = unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0], raw[1]);
        }
        3 => {
            let f: extern "C" fn(u64, u64, u64) -> u64 = unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0], raw[1], raw[2]);
        }
        4 => {
            let f: extern "C" fn(u64, u64, u64, u64) -> u64 = unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0], raw[1], raw[2], raw[3]);
        }
        5 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64) -> u64 =
                unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0], raw[1], raw[2], raw[3], raw[4]);
        }
        6 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64) -> u64 =
                unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0], raw[1], raw[2], raw[3], raw[4], raw[5]);
        }
        7 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64) -> u64 =
                unsafe { std::mem::transmute(ptr) };
            let _ = f(raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6]);
        }
        8 => {
            let f: extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64 =
                unsafe { std::mem::transmute(ptr) };
            let _ = f(
                raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7],
            );
        }
        n => panic!(
            "call_aot_behavior: unsupported arity {} (add an arity arm)",
            n
        ),
    }
}

/// `Actor::register_behavior` handler that runs the actor's current message
/// through AOT-compiled native code, bypassing the bytecode VM.
///
/// Reads the armed `AOT_DISPATCH` target to find the native entry point and
/// select callbacks: when dispatching inside the real actor `Runtime` (the
/// target's `runtime` is non-null) it uses `AotRuntimeCallbacks`, which route
/// state/send/receive/alloc through the runtime; otherwise it uses the
/// standalone `AotActorCallbacks` over the raw actor.
pub fn aot_behavior_adapter(actor: &mut crate::runtime::Actor, args: &[crate::vm::Value]) {
    let target = AOT_DISPATCH.with(|c| *c.borrow());
    let target =
        target.expect("aot_behavior_adapter: no native target armed (call set_aot_dispatch first)");
    assert!(
        !target.fn_ptr.is_null(),
        "aot_behavior_adapter: null fn ptr"
    );

    let raw: Vec<u64> = args.iter().map(|v| v.as_raw()).collect();
    if target.runtime.is_null() {
        // SAFETY: `actor` outlives the native call; `cb` holds a raw pointer
        // to it (mirroring `BytecodeRuntimeCallbacks`) so the `dyn
        // ActorVmCallbacks` fat pointer coerces to `'static` for the
        // thread-local, and is cleared before `cb` (and the borrow) ends.
        let mut cb = AotActorCallbacks {
            actor: actor as *mut crate::runtime::Actor,
        };
        unsafe { crate::jit::runtime::set_jit_callbacks(&mut cb) };
        call_aot_behavior(target.fn_ptr, &raw);
        crate::jit::runtime::clear_jit_callbacks();
        crate::jit::runtime::aot_clear_constants();
    } else {
        // SAFETY: the scheduler holds `&mut Runtime` while dispatching, so the
        // raw pointer is a live, exclusively-borrowed handle; the callback is
        // cleared before the borrow (and dispatch) ends.
        let mut cb = AotRuntimeCallbacks {
            runtime: target.runtime,
            actor_id: actor.id,
        };
        unsafe { crate::jit::runtime::set_jit_callbacks(&mut cb) };
        call_aot_behavior(target.fn_ptr, &raw);
        crate::jit::runtime::clear_jit_callbacks();
        crate::jit::runtime::aot_clear_constants();
    }
}

/// Minimal `ActorVmCallbacks` that routes AOT actor operations (state access,
/// heap allocation) to a single `Actor`. Used by `aot_behavior_adapter` so
/// `StateGet`/`StateSet` and object allocation inside a native behavior body
/// target the right actor. Spawn/Send are unsupported in the standalone
/// native path (they need the full `Runtime`).
struct AotActorCallbacks {
    /// Raw pointer to the actor, kept alive by the caller across the native
    /// call. Mirrors `BytecodeRuntimeCallbacks` (raw `*mut Runtime`) so the
    /// fat pointer stored in `JIT_CALLBACKS` is `'static`.
    actor: *mut crate::runtime::Actor,
}

impl std::fmt::Debug for AotActorCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AotActorCallbacks(actor={:p})", self.actor)
    }
}

impl crate::vm::ActorVmCallbacks for AotActorCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        // SAFETY: `actor` is the caller's live `&mut Actor`.
        Some(unsafe { (*self.actor).id })
    }

    fn alloc(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        // SAFETY: `actor` is the caller's live `&mut Actor`.
        unsafe { (*self.actor).heap.alloc(size, type_tag) }
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        // SAFETY: both raw pointers are valid; `ptr` is from this actor's heap.
        unsafe {
            (*self.actor)
                .orca_gc
                .drop_local_ref(&mut (*self.actor).heap, ptr)
        };
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        // SAFETY: both raw pointers are valid; `ptr` is from this actor's heap.
        unsafe { (*self.actor).orca_gc.local_ref(&(*self.actor).heap, ptr) };
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        // SAFETY: `ptr` is a valid heap pointer from this actor's heap.
        unsafe {
            let header = &*crate::runtime::heap::ActorHeap::header_of(ptr);
            if header.type_tag == HeapTypeTag::Array {
                let payload = header
                    .size
                    .saturating_sub(crate::runtime::heap::ActorHeap::HEADER_SIZE);
                Some(payload / std::mem::size_of::<crate::vm::Value>())
            } else {
                None
            }
        }
    }

    fn get_state_field(&self, field: &str) -> crate::vm::Value {
        // SAFETY: `actor` is the caller's live `&mut Actor`.
        unsafe {
            (*self.actor)
                .get_state_field(field)
                .unwrap_or(crate::vm::Value::nil())
        }
    }

    fn set_state_field(&mut self, field: &str, value: crate::vm::Value) {
        // SAFETY: `actor` is the caller's live `&mut Actor`.
        unsafe { (*self.actor).set_state_field(field, value) };
    }

    fn spawn_actor(
        &mut self,
        _module: &crate::bytecode::CodeModule,
        _behavior_idx: usize,
        _init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
        crate::vm::Value::actor_ref(0)
    }

    fn try_receive(&mut self) -> Option<(u16, crate::vm::Value)> {
        // SAFETY: `actor` is the caller's live `&mut Actor`; mailbox access
        // runs on the owning thread (the standalone driver's dispatcher).
        unsafe { (*self.actor).mailbox.pop() }.map(|msg| {
            let first = msg
                .payload
                .first()
                .copied()
                .unwrap_or(crate::vm::Value::nil());
            (msg.behavior_id, first)
        })
    }

    fn try_receive_match(
        &mut self,
        behavior_ids: &[u16],
    ) -> Option<(usize, Vec<crate::vm::Value>)> {
        // SAFETY: `actor` is the caller's live `&mut Actor`; mailbox access
        // runs on the owning thread (the standalone driver's dispatcher).
        unsafe { (*self.actor).mailbox.receive_match(behavior_ids) }
            .map(|(pos, payload)| (pos, payload.to_vec()))
    }

    fn send_message(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) {
        let Some(target_id) = target.as_actor_id() else {
            return;
        };
        // SAFETY: registry entries are registered by the driver and unregistered
        // before the actor drops; the pointer is valid for this dispatch.
        let target_actor = AOT_ACTORS.with(|c| c.borrow().get(&target_id).copied());
        let Some(target_actor) = target_actor else {
            return;
        };
        unsafe {
            let _ = (*target_actor).mailbox.push_local(crate::runtime::Message {
                behavior_id,
                payload: std::sync::Arc::new(args.to_vec()),
                sender: (*self.actor).id,
                priority: crate::runtime::MessagePriority::Normal,
                trace_id: None,
            });
        }
    }
}

/// `ActorVmCallbacks` that routes AOT actor operations through the real actor
/// `Runtime`, so a native behavior dispatched by the scheduler sees the same
/// state/send/receive/heap semantics as a bytecode behavior. Mirrors
/// `BytecodeRuntimeCallbacks` (raw `*mut Runtime`) and is installed only while
/// the scheduler holds `&mut Runtime`, so the pointer is live and unique.
struct AotRuntimeCallbacks {
    runtime: *mut crate::runtime::Runtime,
    actor_id: u64,
}

impl std::fmt::Debug for AotRuntimeCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AotRuntimeCallbacks(actor={})", self.actor_id)
    }
}

impl crate::vm::ActorVmCallbacks for AotRuntimeCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        Some(self.actor_id)
    }

    fn alloc(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        // SAFETY: the scheduler holds `&mut Runtime`; re-borrow through the
        // raw pointer, mirroring `BytecodeRuntimeCallbacks`.
        unsafe {
            (*self.runtime)
                .actors
                .get_mut(&self.actor_id)?
                .heap
                .alloc(size, type_tag)
        }
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
            }
        }
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                actor.orca_gc.local_ref(&actor.heap, ptr);
            }
        }
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        unsafe {
            let _actor = (*self.runtime).actors.get(&self.actor_id)?;
            let header = &*crate::runtime::heap::ActorHeap::header_of(ptr);
            if header.type_tag == HeapTypeTag::Array {
                let payload_size = header
                    .size
                    .saturating_sub(crate::runtime::heap::ActorHeap::HEADER_SIZE);
                Some(payload_size / std::mem::size_of::<crate::vm::Value>())
            } else {
                None
            }
        }
    }

    fn spawn_actor(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
        // SAFETY: as above; spawning mutates runtime state but never re-enters
        // the VM.
        unsafe { (*self.runtime).spawn_from_module(module, behavior_idx, init) }
    }

    fn send_message(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) {
        if let Some(target_id) = target.as_actor_id() {
            // SAFETY: as above. `send_message_by_id` is safe mid-behavior.
            unsafe { (*self.runtime).send_message_by_id(target_id, behavior_id, args) }
        }
    }

    fn get_state_field(&self, field: &str) -> crate::vm::Value {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get(&self.actor_id) {
                return actor
                    .get_state_field(field)
                    .unwrap_or(crate::vm::Value::nil());
            }
        }
        crate::vm::Value::nil()
    }

    fn set_state_field(&mut self, field: &str, value: crate::vm::Value) {
        unsafe {
            if let Some(actor) = (*self.runtime).actors.get_mut(&self.actor_id) {
                actor.set_state_field(field, value);
            }
        }
    }

    fn perform_builtin_effect_in_module(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        module: &crate::bytecode::CodeModule,
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        // Delegate to the bytecode callbacks, which route the Test/Otp/Http/
        // Actor/IO/Timer/Workflow builtin effects through the real Runtime.
        // `AotRuntimeCallbacks` has the same shape (runtime + actor_id), so
        // this gives AOT-compiled `perform` the exact bytecode semantics.
        let mut bc =
            crate::runtime::callbacks::BytecodeRuntimeCallbacks::new(self.runtime, self.actor_id);
        bc.perform_builtin_effect_in_module(effect_name, op_name, module, regs)
    }

    fn perform_async(
        &mut self,
        effect_op: &str,
        constants: &[crate::bytecode::Constant],
        args: &[crate::vm::Value],
    ) -> crate::vm::PerformAsyncResult {
        // Delegate to the bytecode callbacks, which route the async-effect
        // family (Inference/LLM.ask, Timer.sleep, Pipeline.*, Supervisor.*)
        // through the real Runtime — the exact bytecode PerformAsync path.
        let mut bc =
            crate::runtime::callbacks::BytecodeRuntimeCallbacks::new(self.runtime, self.actor_id);
        bc.perform_async(effect_op, constants, args)
    }

    fn emit_event(&mut self, event: &str, args: &[crate::vm::Value]) {
        // SAFETY: as above.
        unsafe { (*self.runtime).emit_event(self.actor_id, event, args) };
    }

    fn try_receive(&mut self) -> Option<(u16, crate::vm::Value)> {
        // SAFETY: as above; mailbox access runs on the owning scheduler thread.
        unsafe {
            (*self.runtime)
                .actors
                .get_mut(&self.actor_id)?
                .mailbox
                .pop()
        }
        .map(|msg| {
            let first = msg
                .payload
                .first()
                .copied()
                .unwrap_or(crate::vm::Value::nil());
            (msg.behavior_id, first)
        })
    }

    fn try_receive_match(
        &mut self,
        behavior_ids: &[u16],
    ) -> Option<(usize, Vec<crate::vm::Value>)> {
        // SAFETY: as above; mailbox access runs on the owning scheduler thread.
        unsafe {
            (*self.runtime)
                .actors
                .get_mut(&self.actor_id)?
                .mailbox
                .receive_match(behavior_ids)
        }
        .map(|(pos, payload)| (pos, payload.to_vec()))
    }
}

/// `ActorVmCallbacks` for running AOT-compiled top-level code inside a real
/// `Runtime`. Unlike `AotRuntimeCallbacks` (which is fixed to one scheduler-
/// driven actor), this reads `runtime.current_actor` dynamically: outside an
/// actor context allocations go to `Runtime::main_heap`, and `Actor.*` builtin
/// effects are no-ops, exactly like the bytecode `RuntimeVmCallbacks` path.
struct AotTopLevelCallbacks {
    runtime: *mut crate::runtime::Runtime,
}

impl AotTopLevelCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        // SAFETY: caller guarantees `runtime` is live and uniquely borrowed
        // for the duration of the native entry call.
        unsafe { (*self.runtime).current_actor }
    }
}

impl std::fmt::Debug for AotTopLevelCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AotTopLevelCallbacks")
    }
}

impl crate::vm::ActorVmCallbacks for AotTopLevelCallbacks {
    fn current_actor_id(&self) -> Option<u64> {
        self.current_actor_id()
    }

    fn alloc(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        // SAFETY: as in `AotRuntimeCallbacks`.
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    return actor.heap.alloc(size, type_tag);
                }
            }
            rt.main_heap.alloc(size, type_tag)
        }
    }

    fn alloc_arena(&mut self, size: usize, type_tag: HeapTypeTag) -> Option<*mut u8> {
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    return actor.iso_arena.alloc(size, type_tag);
                }
            }
            rt.main_heap.alloc(size, type_tag)
        }
    }

    fn reset_arena(&mut self) {
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    actor.iso_arena.reset();
                }
            }
        }
    }

    fn is_arena_ptr(&self, ptr: *const u8) -> bool {
        unsafe {
            let rt = &*self.runtime;
            rt.current_actor
                .and_then(|id| rt.actors.get(&id))
                .map(|a| a.iso_arena.contains(ptr))
                .unwrap_or(false)
        }
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    if actor.iso_arena.contains(ptr) {
                        return;
                    }
                    actor.orca_gc.drop_local_ref(&mut actor.heap, ptr);
                    return;
                }
            }
            rt.main_gc.drop_local_ref(&mut rt.main_heap, ptr);
        }
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    if actor.iso_arena.contains(ptr) {
                        return;
                    }
                    actor.orca_gc.local_ref(&actor.heap, ptr);
                    return;
                }
            }
            rt.main_gc.local_ref(&rt.main_heap, ptr);
        }
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        unsafe {
            let _rt = &*self.runtime;
            let header = &*crate::runtime::heap::ActorHeap::header_of(ptr);
            if header.type_tag == HeapTypeTag::Array {
                let payload_size = header
                    .size
                    .saturating_sub(crate::runtime::heap::ActorHeap::HEADER_SIZE);
                Some(payload_size / std::mem::size_of::<crate::vm::Value>())
            } else {
                None
            }
        }
    }

    fn spawn_actor(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
        unsafe { (*self.runtime).spawn_from_module(module, behavior_idx, init) }
    }

    fn send_message(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) {
        if let Some(target_id) = target.as_actor_id() {
            unsafe { (*self.runtime).send_message_by_id(target_id, behavior_id, args) }
        }
    }

    fn ask_actor(
        &mut self,
        target: crate::vm::Value,
        behavior_id: u16,
        args: &[crate::vm::Value],
    ) -> crate::vm::Value {
        if let Some(target_id) = target.as_actor_id() {
            unsafe {
                return (*self.runtime)
                    .ask_actor_sync(target_id, behavior_id, args)
                    .unwrap_or(crate::vm::Value::nil());
            }
        }
        crate::vm::Value::nil()
    }

    fn get_state_field(&self, field: &str) -> crate::vm::Value {
        unsafe {
            let rt = &*self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get(&actor_id) {
                    return actor
                        .get_state_field(field)
                        .unwrap_or(crate::vm::Value::nil());
                }
            }
        }
        crate::vm::Value::nil()
    }

    fn set_state_field(&mut self, field: &str, value: crate::vm::Value) {
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                if let Some(actor) = rt.actors.get_mut(&actor_id) {
                    if actor
                        .state_models
                        .get(field)
                        .map(|m| m.is_crdt())
                        .unwrap_or(false)
                    {
                        return;
                    }
                    actor.set_state_field(field, value);
                }
            }
        }
    }

    fn perform_builtin_effect_in_module(
        &mut self,
        effect_name: &str,
        op_name: Option<&str>,
        module: &crate::bytecode::CodeModule,
        regs: &[crate::vm::Value],
    ) -> Option<crate::vm::Value> {
        let actor_id = self.current_actor_id().unwrap_or(0);
        let mut bc =
            crate::runtime::callbacks::BytecodeRuntimeCallbacks::new(self.runtime, actor_id);
        bc.perform_builtin_effect_in_module(effect_name, op_name, module, regs)
    }

    fn perform_async(
        &mut self,
        effect_op: &str,
        constants: &[crate::bytecode::Constant],
        args: &[crate::vm::Value],
    ) -> crate::vm::PerformAsyncResult {
        let actor_id = self.current_actor_id().unwrap_or(0);
        let mut bc =
            crate::runtime::callbacks::BytecodeRuntimeCallbacks::new(self.runtime, actor_id);
        bc.perform_async(effect_op, constants, args)
    }

    fn emit_event(&mut self, event: &str, args: &[crate::vm::Value]) {
        unsafe {
            let rt = &mut *self.runtime;
            if let Some(actor_id) = rt.current_actor {
                rt.emit_event(actor_id, event, args);
            }
        }
    }

    fn try_receive(&mut self) -> Option<(u16, crate::vm::Value)> {
        unsafe {
            let rt = &mut *self.runtime;
            let actor_id = rt.current_actor?;
            rt.actors.get_mut(&actor_id)?.mailbox.pop()
        }
        .map(|msg| {
            let first = msg
                .payload
                .first()
                .copied()
                .unwrap_or(crate::vm::Value::nil());
            (msg.behavior_id, first)
        })
    }

    fn try_receive_match(
        &mut self,
        behavior_ids: &[u16],
    ) -> Option<(usize, Vec<crate::vm::Value>)> {
        unsafe {
            let rt = &mut *self.runtime;
            let actor_id = rt.current_actor?;
            rt.actors
                .get_mut(&actor_id)?
                .mailbox
                .receive_match(behavior_ids)
        }
        .map(|(pos, payload)| (pos, payload.to_vec()))
    }

    fn wait_signal(&mut self, _name: &str) -> crate::vm::SignalWaitResult {
        // Native top-level code has no workflow continuation suspension.
        crate::vm::SignalWaitResult::Ready(crate::vm::Value::unit())
    }
}

/// Create an ISA builder for the specified target.
fn create_isa_builder(target: &str) -> NuResult<isa::Builder> {
    use target_lexicon::Triple;

    match target {
        "native" => cranelift_native::builder().map_err(|msg| crate::types::NuError::VMError {
            msg: format!("host machine not supported: {}", msg),
            span: Span::default(),
        }),
        "ptx" | "nvptx64" => {
            // PTX (NVIDIA GPU) target
            let triple: Triple =
                "nvptx64-nvidia-cuda"
                    .parse()
                    .map_err(|e| crate::types::NuError::VMError {
                        msg: format!("invalid PTX triple: {}", e),
                        span: Span::default(),
                    })?;
            isa::lookup(triple).map_err(|e| crate::types::NuError::VMError {
                msg: format!("PTX target not supported: {}", e),
                span: Span::default(),
            })
        }
        "riscv64" | "riscv" => {
            // RISC-V 64-bit target
            let triple: Triple = "riscv64gc-unknown-none-elf".parse().map_err(|e| {
                crate::types::NuError::VMError {
                    msg: format!("invalid RISC-V triple: {}", e),
                    span: Span::default(),
                }
            })?;
            isa::lookup(triple).map_err(|e| crate::types::NuError::VMError {
                msg: format!("RISC-V target not supported: {}", e),
                span: Span::default(),
            })
        }
        _ => Err(crate::types::NuError::VMError {
            msg: format!(
                "unknown target '{}' (expected native | ptx | riscv64)",
                target
            ),
            span: Span::default(),
        }),
    }
}

/// Register all runtime helper symbols with the JIT builder.
/// Single source of truth: `src/jit/helpers.rs` `define_helpers!` macro.
fn register_runtime_helpers(builder: &mut JITBuilder) {
    crate::jit::helpers::register_with_builder(builder);
}

/// Scan MIR statements to collect field names and string constants.
fn collect_field_and_consts(
    stmt: &mir::Stmt,
    field_map: &mut std::collections::HashMap<String, u8>,
    next_field_id: &mut u8,
    constants: &mut Vec<crate::bytecode::Constant>,
    foreign_functions: &[mir::ForeignFunction],
) {
    match stmt {
        mir::Stmt::Assign { op, .. } => {
            collect_rvalue_field_and_consts(
                op,
                field_map,
                next_field_id,
                constants,
                foreign_functions,
            );
        }
        mir::Stmt::StoreFieldNamed { field, .. } => {
            field_map.entry(field.clone()).or_insert_with(|| {
                let id = *next_field_id;
                *next_field_id = next_field_id.saturating_add(1);
                id
            });
        }
        mir::Stmt::StateSet { field, .. } => {
            let c = crate::bytecode::Constant::String(field.clone());
            if !constants.contains(&c) {
                constants.push(c);
            }
        }
        mir::Stmt::Emit { event, .. } => {
            // Intern the event name so AOT codegen can emit it as a TAG_STRING
            // constant resolved back to content by `nulang_aot_emit_N`.
            let c = crate::bytecode::Constant::String(event.clone());
            if !constants.contains(&c) {
                constants.push(c);
            }
        }
        _ => {}
    }
}

fn collect_rvalue_field_and_consts(
    rv: &mir::RValue,
    field_map: &mut std::collections::HashMap<String, u8>,
    next_field_id: &mut u8,
    constants: &mut Vec<crate::bytecode::Constant>,
    foreign_functions: &[mir::ForeignFunction],
) {
    match rv {
        mir::RValue::Const(c) => {
            if let crate::bytecode::Constant::String(_) = c {
                // Add string constant to pool, returning index
                constants.push(c.clone());
            }
        }
        mir::RValue::Record(fields)
        | mir::RValue::RecordUpdate {
            overrides: fields, ..
        } => {
            for (name, _) in fields {
                field_map.entry(name.clone()).or_insert_with(|| {
                    let id = *next_field_id;
                    *next_field_id = next_field_id.saturating_add(1);
                    id
                });
            }
        }
        mir::RValue::LoadFieldNamed { field, .. } => {
            field_map.entry(field.clone()).or_insert_with(|| {
                let id = *next_field_id;
                *next_field_id = next_field_id.saturating_add(1);
                id
            });
        }
        mir::RValue::Spawn { init, .. } => {
            for (name, rv) in init {
                field_map.entry(name.clone()).or_insert_with(|| {
                    let id = *next_field_id;
                    *next_field_id = next_field_id.saturating_add(1);
                    id
                });
                // Intern the init field name so native spawn code can resolve
                // it back to a string via the constant pool.
                let c = crate::bytecode::Constant::String(name.clone());
                if !constants.contains(&c) {
                    constants.push(c);
                }
                collect_rvalue_field_and_consts(
                    rv,
                    field_map,
                    next_field_id,
                    constants,
                    foreign_functions,
                );
            }
        }
        mir::RValue::StateGet { field } => {
            let c = crate::bytecode::Constant::String(field.clone());
            if !constants.contains(&c) {
                constants.push(c);
            }
        }
        mir::RValue::Perform { effect, op, .. } => {
            // Intern both strings so AOT codegen can emit the effect/op as
            // TAG_STRING constants resolved back to content at dispatch time
            // by `nulang_aot_perform_N` (via the module constant pool).
            for s in [effect, op] {
                let c = crate::bytecode::Constant::String(s.clone());
                if !constants.contains(&c) {
                    constants.push(c);
                }
            }
        }
        mir::RValue::FFICall { idx, .. } => {
            // Intern the library and symbol names so AOT codegen can emit them
            // as TAG_STRING constants resolved back to content at call time by
            // `nulang_aot_ffi_call_N` (via the module constant pool).
            if let Some(ff) = foreign_functions.get(*idx) {
                for s in [&ff.library, &ff.symbol] {
                    let c = crate::bytecode::Constant::String(s.clone());
                    if !constants.contains(&c) {
                        constants.push(c);
                    }
                }
            }
        }
        mir::RValue::PerformAsync { effect_op, .. } => {
            // Intern the fully-qualified effect name so AOT codegen can emit
            // it as a TAG_STRING constant resolved back to content at dispatch
            // time by `nulang_aot_perform_async_N`.
            let c = crate::bytecode::Constant::String(effect_op.clone());
            if !constants.contains(&c) {
                constants.push(c);
            }
        }
        mir::RValue::SignalWait { name } => {
            // Intern the signal name so AOT codegen can emit it as a TAG_STRING
            // constant resolved back to content by `nulang_aot_signal_wait`.
            let c = crate::bytecode::Constant::String(name.clone());
            if !constants.contains(&c) {
                constants.push(c);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    /// End-to-end: `"hello" + 2 + 3` must concatenate with coercion ("hello23"),
    /// not fall through to integer arithmetic on the string's tag bits. Replicates
    /// `AotModule::run`'s heap + constants setup but keeps the heap alive so the
    /// result (a heap string) can be resolved back to its content.
    #[test]
    fn test_aot_str_concat_coercion_end_to_end() {
        let source = r#"
            fn f() -> String { "hello" + 2 + 3 }
            fn main() { f() }
        "#;
        let tokens = crate::lexer::Lexer::new(source).lex().unwrap();
        let ast = crate::parser::Parser::new(tokens).parse_module().unwrap();
        let mut tc = crate::typechecker::TypeChecker::new();
        tc.check_module(&ast).unwrap();
        let hir = crate::hir_lower::lower_module(&ast, &tc.inferred_decl_types);
        let mir = crate::mir_lower::lower_module(&hir).unwrap();
        let aot = super::AotModule::compile(&mir).expect("AOT compile");
        let idx = aot.entry_idx.unwrap_or(0);
        let ptr = aot.compiled_funcs[idx];

        let mut heap = crate::runtime::heap::ActorHeap::new(1024 * 1024);
        heap.set_actor_id(0);
        crate::jit::runtime::aot_set_heap(heap);
        unsafe {
            crate::jit::runtime::aot_set_constants(&aot.constants);
        }
        let func: extern "C" fn() -> u64 = unsafe { std::mem::transmute(ptr) };
        let raw = func();
        let s = crate::jit::runtime::resolve_string_coerce(raw);
        crate::jit::runtime::aot_clear_constants();
        let _ = crate::jit::runtime::aot_take_heap();
        assert_eq!(s.as_deref(), Some("hello23"));
    }
}
